//! RFB 命令行 TUI agent：ratatui + rfb-adk 的轻量循环（adk 组件生态：
//! 同一套 Llm 客户端与 Tool 面，思想借鉴 pi——循环即 agent）。
//!
//! **默认会话零沙箱**：工具面上只有 `sandbox_start`/`sandbox_stop`；agent
//! 判断任务需要在隔离环境里执行/读写时，主动调 `sandbox_start` 才引导
//! Firecracker VM（约 1-2s）并解锁 read/write/edit/bash/grep/find/ls。
//! `sandbox_stop` 退役 VM（kill + reap，无孤儿）。宿主进程零 VM 成本起步。
//!
//! 交互：输入任务回车派发；agent 运行中再输入 = **转向**（下一轮注入，
//! 不取消不重启）；Esc = 运行中中止 / 空闲时退出。
//! 模型：`RFB_AGENT_BASE_URL`/`RFB_AGENT_MODEL`/`RFB_AGENT_API_KEY`（OpenAI
//! 兼容网关）走真实 LLM；否则内置演示模型（脚本化工具调用，循环/工具/VM
//! 全部真实）。自测：`RFB_TUI_AUTOTEST=1`。

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::terminal;
use ratatui::crossterm::ExecutableCommand as _;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use rfb_adk::{AbortFlag, LoopEvent, LoopOutcome, SteeringInbox, Toolset};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 演示模型：脚本化工具调用（循环/工具/VM 全部真实）
// ---------------------------------------------------------------------------

struct DemoModel {
    responses: Mutex<Vec<adk_rust::Content>>,
}

impl DemoModel {
    fn plan() -> Self {
        let script = "#!/bin/sh\necho hello from the sandbox\ndate\n";
        use adk_rust::Part;
        let call = |name: &str, args: Value| adk_rust::Content {
            role: "model".into(),
            parts: vec![Part::FunctionCall {
                name: name.into(),
                args,
                id: Some(format!("call-{name}")),
                thought_signature: None,
            }],
        };
        Self {
            responses: Mutex::new(vec![
                // agent 主动请求沙箱——默认会话没有沙箱
                call("sandbox_start", json!({})),
                call(
                    "write",
                    json!({"path": "hello.sh", "mode": 0o755,
                           "data": script.as_bytes().iter().copied().collect::<Vec<u8>>()}),
                ),
                call("bash", json!({"command": "/workspace/hello.sh", "timeout_ms": 5000})),
                call("read", json!({"path": "hello.sh"})),
            ]),
        }
    }
}

#[adk_rust::async_trait]
impl adk_rust::Llm for DemoModel {
    fn name(&self) -> &str {
        "demo-scripted"
    }
    async fn generate_content(
        &self,
        _req: adk_rust::LlmRequest,
        _stream: bool,
    ) -> adk_rust::Result<adk_rust::LlmResponseStream> {
        let mut responses = self.responses.lock().unwrap();
        let content = if responses.is_empty() {
            adk_rust::Content {
                role: "model".into(),
                parts: vec![adk_rust::Part::Text {
                    text: "演示完成：请求沙箱 → 写脚本(mode=0755) → 执行 → 读回。".into(),
                }],
            }
        } else {
            responses.remove(0)
        };
        drop(responses);
        let response = adk_rust::LlmResponse {
            content: Some(content),
            turn_complete: true,
            finish_reason: Some(adk_rust::FinishReason::Stop),
            ..Default::default()
        };
        Ok(Box::pin(adk_rust::futures::stream::once(async move {
            Ok(response)
        })))
    }
}

// ---------------------------------------------------------------------------
// 后台运行时：专职线程 + tokio
// ---------------------------------------------------------------------------

enum Cmd {
    Run(String),
    Shutdown,
}

/// UI 可渲染的一行。
#[derive(Clone)]
struct Entry {
    text: String,
    style: Style,
}

impl Entry {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default(),
        }
    }
    fn dim(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default().fg(Color::DarkGray),
        }
    }
    fn accent(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        }
    }
    fn tool(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default().fg(Color::Blue),
        }
    }
    fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: Style::default().fg(Color::Red),
        }
    }
}

type Log = Arc<Mutex<Vec<Entry>>>;

fn push(log: &Log, entry: Entry) {
    let mut log = log.lock().unwrap();
    for line in entry.text.split('\n').filter(|l| !l.trim().is_empty()) {
        log.push(Entry {
            text: line.to_owned(),
            style: entry.style,
        });
    }
    let len = log.len();
    if len > 400 {
        log.drain(..len - 400);
    }
}

const INSTRUCTION: &str = "你是 RFB 沙箱 agent。默认没有沙箱——只有当任务需要在隔离环境\
     里执行命令或读写文件时，才调用 sandbox_start（约 1-2s，解锁 \
     read/write/edit/bash/grep/find/ls）；不需要则直接回答。任务完成后可用 \
     sandbox_stop 释放 VM。guest 的 sh 只支持 -c 形式；没有 rm/cat/ls/tail\
     （用 read/ls 工具替代，清空文件用 `: > file`）。";

/// 沙箱引导配置：锚定本 crate 的仓库位置（cargo test/cwd 无关）。
fn sandbox_setup() -> rfb_adk::SandboxSetup {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    rfb_adk::SandboxSetup {
        kernel: root.join("resx/kernel/vmlinux-arcbox-0.0.24"),
        rootfs: root.join("resx/rootfs/zeroboot-zbrt.ext4"),
        firecracker: root.join("sdk/examples/python/assets/firecracker"),
        guest_port: 5000,
        boot_timeout: Duration::from_secs(30),
    }
}

fn make_model() -> Arc<dyn adk_rust::Llm> {
    if let (Ok(base), Ok(model)) = (
        std::env::var("RFB_AGENT_BASE_URL"),
        std::env::var("RFB_AGENT_MODEL"),
    ) {
        Arc::new(
            adk_rust::model::openai::OpenAIClient::new(
                adk_rust::model::openai::OpenAIConfig::compatible(
                    std::env::var("RFB_AGENT_API_KEY").unwrap_or_default(),
                    base,
                    model,
                ),
            )
            .expect("model client"),
        )
    } else {
        Arc::new(DemoModel::plan())
    }
}

struct AgentSession {
    toolset: Toolset,
    model: Arc<dyn adk_rust::Llm>,
    messages: Vec<adk_rust::Content>,
}

fn spawn_worker(log: Log) -> (Sender<Cmd>, SteeringInbox, AbortFlag, Receiver<()>) {
    let (tx, rx): (Sender<Cmd>, Receiver<Cmd>) = std::sync::mpsc::channel();
    let (done_tx, done_rx): (Sender<()>, Receiver<()>) = std::sync::mpsc::channel();
    let inbox = SteeringInbox::new();
    let abort = AbortFlag::new();
    let (inbox2, abort2) = (inbox.clone(), abort.clone());
    std::thread::Builder::new()
        .name("rfb-tui-agent".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            // 会话 = 消息历史 + 模型 + 工具集。工具集初始只有 sandbox_start/
            // sandbox_stop——沙箱由 agent 主动请求。
            let mut session: Option<AgentSession> = None;
            for cmd in rx {
                match cmd {
                    Cmd::Run(task) => {
                        if session.is_none() {
                            let toolset = Toolset::new();
                            let state = rfb_adk::SandboxState::new();
                            rfb_adk::install_session_tools(
                                sandbox_setup(),
                                &toolset,
                                state,
                            );
                            session = Some(AgentSession {
                                toolset,
                                model: make_model(),
                                messages: Vec::new(),
                            });
                        }
                        let sess = session.as_mut().unwrap();
                        sess.messages
                            .push(adk_rust::Content::new("user").with_text(task));
                        let outcome = rt.block_on(rfb_adk::agent_loop(
                            sess.model.clone(),
                            &sess.toolset,
                            &mut sess.messages,
                            rfb_adk::LoopOptions {
                                inbox: Some(inbox2.clone()),
                                abort: Some(abort2.clone()),
                                on_event: Some(&|event| match event {
                                    LoopEvent::RoundStart(round) => {
                                        push(&log, Entry::dim(format!("—— 第 {} 轮 ——", round + 1)));
                                    }
                                    LoopEvent::Text(text) => {
                                        push(&log, Entry::plain(format!("模型：{text}")));
                                    }
                                    LoopEvent::ToolCall { name, args } => {
                                        let args = serde_json::to_string(args)
                                            .unwrap_or_default()
                                            .chars()
                                            .take(120)
                                            .collect::<String>();
                                        if name == rfb_adk::SANDBOX_START_NAME {
                                            push(&log, Entry::accent("agent 请求沙箱，正在引导 VM…"));
                                        } else {
                                            push(&log, Entry::tool(format!("🔧 {name}({args})")));
                                        }
                                    }
                                    LoopEvent::ToolResult { name, ok } => {
                                        if name == rfb_adk::SANDBOX_START_NAME && ok {
                                            push(&log, Entry::accent("沙箱已就绪（VM 引导完成）。"));
                                        } else if !ok {
                                            push(&log, Entry::error(format!("🔧 {name} 失败（错误已回给模型）")));
                                        }
                                    }
                                }),
                                ..Default::default()
                            },
                        ));
                        match outcome {
                            Ok(LoopOutcome::Done(text)) => {
                                push(&log, Entry::plain(format!("agent：{text}")));
                            }
                            Ok(LoopOutcome::MaxRounds(rounds)) => {
                                push(&log, Entry::error(format!("达到 {rounds} 轮上限；可继续输入追问。")));
                            }
                            Ok(LoopOutcome::Aborted) => {
                                push(&log, Entry::dim("已中止（历史保留，可继续输入）。"));
                            }
                            Err(e) => push(&log, Entry::error(format!("模型调用失败：{e}"))),
                        }
                        let _ = done_tx.send(());
                    }
                    Cmd::Shutdown => {
                        push(&log, Entry::dim("沙箱关闭中…"));
                        session = None; // drop → VM kill + reap（RAII）
                        push(&log, Entry::dim("已退出（VM 已退役）。"));
                    }
                }
            }
        })
        .expect("worker thread");
    (tx, inbox, abort, done_rx)
}

// ---------------------------------------------------------------------------
// TUI 主循环
// ---------------------------------------------------------------------------

fn main() -> std::io::Result<()> {
    let log: Log = Arc::new(Mutex::new(vec![Entry::plain(
        "RFB 沙箱 TUI agent（默认零沙箱，agent 需要时自己引导）— 回车派任务；运行中输入=转向；Esc=中止/退出",
    )]));
    let (tx, inbox, abort, done_rx) = spawn_worker(log.clone());

    terminal::enable_raw_mode()?;
    std::io::stdout().execute(terminal::EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut input = String::new();
    let mut running = false;
    let autotest = std::env::var("RFB_TUI_AUTOTEST").is_ok();
    let mut task_sent = false;
    let started = std::time::Instant::now();

    loop {
        // 自测：启动 2s 后自动派任务（演示模型会自己请求沙箱）；
        // 收到 agent 回复即落盘退出。
        if autotest && !task_sent && started.elapsed() > Duration::from_secs(2) {
            task_sent = true;
            running = true;
            tx.send(Cmd::Run("演示：写脚本、执行、读回".into())).ok();
        }
        if autotest && task_sent && log.lock().unwrap().iter().any(|e| e.text.starts_with("agent：")) {
            let entries = log.lock().unwrap().clone();
            let mut err = std::io::stderr();
            for e in &entries {
                let _ = write!(err, "[对话] {}\n", e.text);
            }
            tx.send(Cmd::Shutdown).ok();
            break;
        }

        let entries = log.lock().unwrap().clone();
        terminal.draw(|f| {
            let [head, body, foot] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(3),
            ])
            .areas(f.area());
            let mode = if std::env::var("RFB_AGENT_BASE_URL").is_ok() {
                "网关模型"
            } else {
                "演示模型"
            };
            let status = if running {
                "运行中（回车=转向 Esc=中止）"
            } else {
                "空闲（回车=派任务 Esc=退出）"
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(
                        " RFB 沙箱 TUI Agent ",
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!("模型：{mode} · {status}"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ])),
                head,
            );
            let lines: Vec<Line> = entries
                .iter()
                .map(|e| Line::from(Span::styled(e.text.clone(), e.style)))
                .collect();
            f.render_widget(
                Paragraph::new(lines)
                    .block(Block::default().borders(Borders::ALL).title(" agent 对话 ")),
                body,
            );
            f.render_widget(
                Paragraph::new(input.clone()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" 任务（回车发送） "),
                ),
                foot,
            );
        })?;

        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Esc => {
                            if running {
                                abort.abort(); // 中止本轮，历史保留
                                running = false;
                            } else {
                                tx.send(Cmd::Shutdown).ok();
                                break;
                            }
                        }
                        KeyCode::Enter => {
                            let task = input.trim().to_owned();
                            if !task.is_empty() {
                                if running {
                                    inbox.send(task); // 转向：下一轮注入
                                    push(&log, Entry::dim("（已转向，下一轮注入）"));
                                } else {
                                    running = true;
                                    tx.send(Cmd::Run(task)).ok();
                                }
                                input.clear();
                            }
                        }
                        KeyCode::Backspace => {
                            input.pop();
                        }
                        KeyCode::Char(c) => input.push(c),
                        _ => {}
                    }
                }
            }
        }
        // 运行结束检测：worker 通知后回空闲
        if running && done_rx.try_recv().is_ok() {
            running = false;
        }
    }

    std::io::stdout().execute(ratatui::crossterm::cursor::Show)?;
    std::io::stdout().execute(terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    Ok(())
}
