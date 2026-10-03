//! RFB 命令行 TUI agent：ratatui + adk-rust agent + 本机 Firecracker 沙箱。
//!
//! 启动即自动引导 microVM（~2s），输入框给 agent 派任务，Esc 退出（VM 随之
//! 退役：RAII Drop kill + reap）。
//!
//! 模型：设置了 `RFB_AGENT_BASE_URL` / `RFB_AGENT_API_KEY` / `RFB_AGENT_MODEL`
//! （OpenAI 兼容 chat/completions 网关）走真实 LLM；否则用内置演示模型
//! （脚本化多步工具调用，工具循环全程真实：真实 runner + 真实 VM）。
//!
//! 自测：`RFB_TUI_AUTOTEST=1` 启动 4s 后自动派一个演示任务，收到 agent 回
//! 复即关沙箱退出（非交互验证通道）。

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::crossterm::terminal;
use ratatui::crossterm::ExecutableCommand as _;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Terminal;
use rfb::zeroboot::{Config, ZeroBootProvider};
use rfb::{Capability, Sandbox, SandboxProvider, SandboxSpec};
use serde_json::{json, Value};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 演示模型：脚本化多步工具调用
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
                call(
                    "write",
                    json!({"path": "hello.sh", "mode": 0o755,
                           "data": script.as_bytes().iter().copied().collect::<Vec<u8>>()}),
                ),
                call(
                    "bash",
                    json!({"command": "/workspace/hello.sh", "timeout_ms": 5000}),
                ),
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
        let next = self.responses.lock().unwrap().pop();
        let content = next.unwrap_or_else(|| adk_rust::Content {
            role: "model".into(),
            parts: vec![adk_rust::Part::Text {
                text: "演示完成：写脚本(mode=0755) → 直接执行 → 读回，工具循环全程真实。".into(),
            }],
        });
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
// 后台运行时：专职线程 + tokio，UI 通过 channel 交互
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

fn spawn_worker(log: Log) -> Sender<Cmd> {
    let (tx, rx): (Sender<Cmd>, Receiver<Cmd>) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("rfb-tui-agent".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            let mut sandbox: Option<Arc<dyn Sandbox>> = rt.block_on(async {
                push(&log, Entry::dim("正在启动 Firecracker microVM…"));
                let provider = ZeroBootProvider::new(Config {
                    kernel: Some(PathBuf::from("../resx/kernel/vmlinux-arcbox-0.0.24")),
                    rootfs: Some(PathBuf::from("../resx/rootfs/zeroboot-zbrt.ext4")),
                    firecracker: Some(PathBuf::from("../sdk/examples/python/assets/firecracker")),
                    guest_port: 5000,
                    timeout: Duration::from_secs(30),
                });
                match provider
                    .create(SandboxSpec {
                        capabilities: vec![
                            Capability::Execute,
                            Capability::Health,
                            Capability::ReadFile,
                            Capability::WriteFile,
                        ],
                        ..SandboxSpec::default()
                    })
                    .await
                {
                    Ok(boxed) => {
                        let sandbox: Arc<dyn Sandbox> = boxed.into();
                        match sandbox.ping().await {
                            Ok(h) if h.healthy => {
                                push(
                                    &log,
                                    Entry::accent("沙箱就绪（ping ok），输入任务回车派发。"),
                                );
                                Some(sandbox)
                            }
                            other => {
                                push(&log, Entry::error(format!("沙箱 ping 失败：{other:?}")));
                                None
                            }
                        }
                    }
                    Err(e) => {
                        push(&log, Entry::error(format!("VM 启动失败：{e}")));
                        None
                    }
                }
            });
            let mut agent: Option<rfb_adk::SandboxAgent> = None;
            let instruction = "你是 RFB 沙箱 agent：用工具（read/write/edit/bash/grep/find/ls）\
                在 VM 里完成用户的任务，做完用一句话汇报。guest 的 sh 只支持 -c 形式；\
                没有 rm/cat/ls/tail（用 read/ls 工具替代，清空文件用 `: > file`）。";
            for cmd in rx {
                match cmd {
                    Cmd::Run(task) => {
                        let Some(sandbox) = sandbox.as_ref() else {
                            push(&log, Entry::error("沙箱未就绪。"));
                            continue;
                        };
                        if agent.is_none() {
                            agent = match rt.block_on(make_agent(sandbox.clone(), instruction)) {
                                Ok(agent) => Some(agent),
                                Err(e) => {
                                    push(&log, Entry::error(format!("agent 组装失败：{e}")));
                                    continue;
                                }
                            };
                        }
                        push(&log, Entry::accent(format!(">>> {task}")));
                        match rt.block_on(agent.as_ref().unwrap().run(&task)) {
                            Ok(reply) => {
                                push(&log, Entry::plain(format!("agent：{}", reply.text)));
                                push(
                                    &log,
                                    Entry::dim(format!(
                                        "（turns={} tool_calls={}）",
                                        reply.turns, reply.tool_calls
                                    )),
                                );
                            }
                            Err(e) => push(&log, Entry::error(format!("agent 运行失败：{e}"))),
                        }
                    }
                    Cmd::Shutdown => {
                        push(&log, Entry::dim("沙箱关闭中…"));
                        agent = None;
                        sandbox = None; // drop → VM kill + reap（RAII）
                        push(&log, Entry::dim("沙箱已关闭（VM kill + reap）。"));
                    }
                }
            }
            // worker 结束：agent/sandbox drop → VM kill + reap（RAII）。
        })
        .expect("worker thread");
    tx
}

/// make_agent 没有 log 句柄；模型选择信息走 stderr（TUI 原始模式下可见）。
fn note_model_choice() {
    let mode = if std::env::var("RFB_AGENT_BASE_URL").is_ok() {
        "网关（OpenAI 兼容）"
    } else {
        "内置演示（设 RFB_AGENT_BASE_URL/RFB_AGENT_MODEL/RFB_AGENT_API_KEY 用真实 LLM）"
    };
    let _ = std::io::stderr().write_all(format!("模型：{mode}\n").as_bytes());
}

async fn make_agent(
    sandbox: Arc<dyn Sandbox>,
    instruction: &str,
) -> adk_rust::Result<rfb_adk::SandboxAgent> {
    note_model_choice();
    if let (Ok(base), Ok(model)) = (
        std::env::var("RFB_AGENT_BASE_URL"),
        std::env::var("RFB_AGENT_MODEL"),
    ) {
        let config = rfb_adk::SandboxAgentConfig {
            api_key: std::env::var("RFB_AGENT_API_KEY").unwrap_or_default(),
            base_url: base,
            model,
            model_timeout: Duration::from_secs(300),
            max_iterations: 30,
        };
        return rfb_adk::sandbox_agent("rfb-tui-agent", instruction, sandbox, &config).await;
    }
    rfb_adk::sandbox_agent_with_model(
        "rfb-tui-agent",
        instruction,
        sandbox,
        Arc::new(DemoModel::plan()),
        30,
    )
    .await
}

// ---------------------------------------------------------------------------
// TUI 主循环
// ---------------------------------------------------------------------------

fn main() -> std::io::Result<()> {
    let log: Log = Arc::new(Mutex::new(vec![Entry::plain(
        "RFB 沙箱 TUI agent — 输入任务回车派发，Esc 退出（VM 自动退役）",
    )]));
    let tx = spawn_worker(log.clone());

    terminal::enable_raw_mode()?;
    std::io::stdout().execute(terminal::EnterAlternateScreen)?;
    std::io::stdout().execute(ratatui::crossterm::cursor::Show)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut input = String::new();
    let autotest = std::env::var("RFB_TUI_AUTOTEST").is_ok();
    let mut task_sent = false;

    loop {
        // 自测通道：boot 稳定后自动派演示任务，收到 agent 回复即退出
        if autotest
            && !task_sent
            && log
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.text.contains("沙箱就绪"))
        {
            task_sent = true;
            tx.send(Cmd::Run("演示：写脚本、执行、读回".into())).ok();
        }
        if autotest
            && task_sent
            && log
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.text.starts_with("agent："))
        {
            // 自测：退出前把完整对话落 stderr（typescript 可验）。
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
                        format!("模型：{mode} · Esc 退出"),
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
                            tx.send(Cmd::Shutdown).ok();
                            break;
                        }
                        KeyCode::Enter => {
                            let task = input.trim().to_owned();
                            if !task.is_empty() {
                                tx.send(Cmd::Run(task)).ok();
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
    }

    // 还原终端；VM 的退役在 worker 线程完成（Shutdown 已发送）。
    std::io::stdout().execute(ratatui::crossterm::cursor::Show)?;
    std::io::stdout().execute(terminal::LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;
    Ok(())
}
