//! 轻量 agent 循环：贴合 adk 组件生态（`Llm` 客户端、`Tool`、`Content`
//! 与 Runner 完全同源、可互换），循环本身借鉴 pi 的思想——**agent 就是
//! 循环**，可读、可干预、一屏看完。不是 pi 的移植，也不是 Runner 的替代。
//!
//! 不用 Runner/SessionService 编排——对话是一个调用方拥有的普通
//! `Vec<Content>`，循环做的事只有四件：
//!
//! 1. 把转向消息（steering：运行中追加的用户输入）作为普通 user 消息
//!    追加进历史——转向即数据；
//! 2. 调模型（非流式，一次性取整轮回复）；
//! 3. 回复没有工具调用 → 循环结束，返回最终文本；
//! 4. 有工具调用 → 逐个执行、把结果（**包括错误**——错误也是给模型的
//!    数据，不是异常路径）作为 tool 消息回填、超大输出截断入库，回到 1。
//!
//! 两条路径二选一：要会话管理/回调/多 agent（adk 的强项）用
//! [`crate::adapter::sandbox_agent`]（Runner）；要内嵌进自己的 UI/服务、
//! 需要运行中转向与中止的，用这里。同一份 [`crate::sandbox_tools`] 工具
//! 面与同一个 adk 模型客户端两边通用。

use adk_rust::{Content, LlmRequest, Part};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// 运行中转向：用户在 agent 干活时输入的消息，循环在下一轮开始前作为
/// 普通 user 消息注入——pi 的 steering 语义，不取消、不重启，模型自己
/// 看到新指令后自行调整。
#[derive(Clone, Default)]
pub struct SteeringInbox(Arc<Mutex<Vec<String>>>);

impl SteeringInbox {
    pub fn new() -> Self {
        Self::default()
    }
    /// 用户侧：随时可投（包括循环正在跑的时候）。
    pub fn send(&self, message: impl Into<String>) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(message.into());
    }
    fn drain(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// 中止标志：循环在每轮模型调用与每个工具执行之间检查。
#[derive(Clone, Default)]
pub struct AbortFlag(Arc<AtomicBool>);

impl AbortFlag {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn abort(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn is_aborted(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// 循环事件（TUI/日志的实时展示钩子）。
pub enum LoopEvent<'a> {
    /// 第 `round` 轮开始。
    RoundStart(usize),
    /// 模型输出了一段文本。
    Text(&'a str),
    /// 模型发起了一次工具调用。
    ToolCall { name: &'a str, args: &'a Value },
    /// 工具调用已回填（`ok=false` = 错误作为数据回给了模型）。
    ToolResult { name: &'a str, ok: bool },
}

/// 事件回调类型（同步、快速——TUI 里只 push 日志）。
pub type EventHandler = Arc<dyn Fn(LoopEvent<'_>) + Send + Sync>;

pub struct LoopOptions {
    /// 最大轮数（一轮 = 一次模型调用 + 其工具执行）。
    pub max_rounds: usize,
    /// 单条工具结果进入历史的字符上限；超出截断（超大输出会永久占据
    /// 每一轮的上下文，截断是 pi 的上下文卫生纪律）。
    pub tool_output_chars: usize,
    /// 传给工具的执行上下文；`None` 用内置空实现（rfb 工具不读 ctx）。
    pub ctx: Option<Arc<dyn adk_rust::ToolContext>>,
    /// 运行中转向收件箱。
    pub inbox: Option<SteeringInbox>,
    /// 中止标志。
    pub abort: Option<AbortFlag>,
    /// 事件回调。
    pub on_event: Option<EventHandler>,
}

impl Default for LoopOptions {
    fn default() -> Self {
        Self {
            max_rounds: 40,
            tool_output_chars: 16 * 1024,
            ctx: None,
            inbox: None,
            abort: None,
            on_event: None,
        }
    }
}

/// 循环的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopOutcome {
    /// 模型给出了不带工具调用的最终回复。
    Done(String),
    /// 达到轮数上限仍未收尾——历史保留在调用方的 `messages` 里，可续跑。
    MaxRounds(usize),
    /// 被中止标志打断（历史同样保留）。
    Aborted,
}

/// 内置空工具上下文（rfb 工具不读 ctx；第三方工具拿到的是诚实声明过的
/// stub——所有查询返回空）。
struct NoCtx;

#[adk_rust::async_trait]
impl adk_rust::ReadonlyContext for NoCtx {
    fn invocation_id(&self) -> &str {
        "agent-loop"
    }
    fn agent_name(&self) -> &str {
        "agent-loop"
    }
    fn user_id(&self) -> &str {
        "agent-loop"
    }
    fn app_name(&self) -> &str {
        "agent-loop"
    }
    fn session_id(&self) -> &str {
        "agent-loop"
    }
    fn branch(&self) -> &str {
        ""
    }
    fn user_content(&self) -> &Content {
        static EMPTY: std::sync::OnceLock<Content> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| Content::new("user"))
    }
}

impl adk_rust::CallbackContext for NoCtx {
    fn artifacts(&self) -> Option<Arc<dyn adk_rust::Artifacts>> {
        None
    }
}

#[adk_rust::async_trait]
impl adk_rust::ToolContext for NoCtx {
    fn function_call_id(&self) -> &str {
        ""
    }
    fn actions(&self) -> adk_rust::EventActions {
        adk_rust::EventActions::default()
    }
    fn set_actions(&self, _actions: adk_rust::EventActions) {}
    async fn search_memory(&self, _query: &str) -> adk_rust::Result<Vec<adk_rust::MemoryEntry>> {
        Ok(Vec::new())
    }
}

/// 可变的工具集句柄：循环每轮重建声明与分发表，所以工具可以**运行中
/// 注册/注销**（例如 agent 请求沙箱后才解锁沙箱工具——默认会话零沙箱）。
/// Clone = 共享同一份注册表。
#[derive(Clone, Default)]
pub struct Toolset(Arc<std::sync::RwLock<Vec<Arc<dyn adk_rust::Tool>>>>);

impl Toolset {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&self, tool: Arc<dyn adk_rust::Tool>) {
        let mut tools = self.0.write().unwrap_or_else(|p| p.into_inner());
        tools.retain(|t| t.name() != tool.name());
        tools.push(tool);
    }
    pub fn unregister(&self, name: &str) {
        self.0
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|t| t.name() != name);
    }
    pub fn snapshot(&self) -> Vec<Arc<dyn adk_rust::Tool>> {
        self.0.read().unwrap_or_else(|p| p.into_inner()).clone()
    }
    pub fn names(&self) -> Vec<String> {
        self.snapshot()
            .iter()
            .map(|t| t.name().to_owned())
            .collect()
    }
}

fn emit(opts: &LoopOptions, event: LoopEvent<'_>) {
    if let Some(on_event) = &opts.on_event {
        on_event(event);
    }
}

/// 跑一轮循环直到模型给出最终回复、达到轮数上限或被中止。
///
/// `messages` 由调用方拥有并持久（跨多次 `agent_loop` 调用 = 会话连续）；
/// 本函数在结束前会追加：转向 user 消息、assistant 回复、tool 结果。
/// `tools` 每轮重新快照——运行中注册/注销工具（如按需沙箱）即时生效。
///
/// # Errors
///
/// 模型调用失败（传输/网关错误）时返回 `Err`；**工具错误不是 Err**——
/// 它作为 `{"error": ...}` 数据回填给模型，由模型决定重试或放弃。
pub async fn agent_loop(
    model: Arc<dyn adk_rust::Llm>,
    tools: &Toolset,
    messages: &mut Vec<Content>,
    opts: LoopOptions,
) -> adk_rust::Result<LoopOutcome> {
    let ctx: Arc<dyn adk_rust::ToolContext> = match &opts.ctx {
        Some(ctx) => ctx.clone(),
        None => Arc::new(NoCtx),
    };

    for round in 0..opts.max_rounds {
        if opts.abort.as_ref().is_some_and(AbortFlag::is_aborted) {
            return Ok(LoopOutcome::Aborted);
        }
        // 转向：用户中途输入 = 普通 user 消息。
        for message in opts
            .inbox
            .as_ref()
            .map(SteeringInbox::drain)
            .unwrap_or_default()
        {
            if !message.trim().is_empty() {
                messages.push(Content::new("user").with_text(message));
            }
        }
        emit(&opts, LoopEvent::RoundStart(round));

        // 每轮重建声明与分发表：工具面可运行中变化。
        let mut declarations: HashMap<String, Value> = HashMap::new();
        let mut dispatch: HashMap<String, Arc<dyn adk_rust::Tool>> = HashMap::new();
        for tool in tools.snapshot() {
            declarations.insert(tool.name().to_owned(), tool.declaration());
            dispatch.insert(tool.name().to_owned(), tool);
        }

        let request = LlmRequest {
            model: model.name().to_owned(),
            contents: messages.clone(),
            config: None,
            tools: declarations.clone(),
            previous_response_id: None,
        };
        let stream = model.generate_content(request, false).await?;
        use adk_rust::futures::StreamExt;
        let mut pinned = std::pin::pin!(stream);
        let mut response: Option<adk_rust::LlmResponse> = None;
        while let Some(item) = pinned.next().await {
            response = Some(item?);
        }
        let Some(response) = response else {
            return Err(adk_rust::AdkError::new(
                adk_rust::ErrorComponent::Model,
                adk_rust::ErrorCategory::Internal,
                "rfb.loop.empty_response",
                "model returned an empty response stream",
            ));
        };
        if let Some(message) = &response.error_message {
            return Err(adk_rust::AdkError::new(
                adk_rust::ErrorComponent::Model,
                adk_rust::ErrorCategory::Unavailable,
                "rfb.loop.model_error",
                message.clone(),
            ));
        }

        let content = response.content.unwrap_or_else(|| Content::new("model"));
        let mut text = String::new();
        let mut calls: Vec<(Option<String>, String, Value)> = Vec::new();
        for part in &content.parts {
            match part {
                Part::Text { text: t } if !t.trim().is_empty() => {
                    text.push_str(t);
                    emit(&opts, LoopEvent::Text(t));
                }
                Part::FunctionCall { name, args, id, .. } => {
                    calls.push((id.clone(), name.clone(), args.clone()));
                }
                _ => {}
            }
        }
        messages.push(Content {
            role: "model".into(),
            parts: content.parts,
        });
        if calls.is_empty() {
            return Ok(LoopOutcome::Done(text));
        }

        for (id, name, args) in calls {
            emit(
                &opts,
                LoopEvent::ToolCall {
                    name: &name,
                    args: &args,
                },
            );
            let result = match dispatch.get(&name) {
                Some(tool) => match tool.execute(ctx.clone(), args.clone()).await {
                    Ok(value) => {
                        emit(
                            &opts,
                            LoopEvent::ToolResult {
                                name: &name,
                                ok: true,
                            },
                        );
                        value
                    }
                    Err(error) => {
                        emit(
                            &opts,
                            LoopEvent::ToolResult {
                                name: &name,
                                ok: false,
                            },
                        );
                        serde_json::json!({"error": error.to_string()})
                    }
                },
                None => {
                    emit(
                        &opts,
                        LoopEvent::ToolResult {
                            name: &name,
                            ok: false,
                        },
                    );
                    serde_json::json!({"error": format!("unknown tool: {name}")})
                }
            };
            let mut encoded = serde_json::to_string(&result).unwrap_or_else(|e| {
                serde_json::json!({"error": format!("result not serializable: {e}")}).to_string()
            });
            if encoded.chars().count() > opts.tool_output_chars {
                let head: String = encoded.chars().take(opts.tool_output_chars).collect();
                encoded = format!("{head}\n…[truncated]");
            }
            messages.push(Content {
                role: "tool".into(),
                parts: vec![Part::FunctionResponse {
                    function_response: adk_rust::FunctionResponseData::new(name.clone(), {
                        serde_json::from_str(&encoded).unwrap_or(Value::String(encoded.clone()))
                    }),
                    id,
                    annotations: None,
                }],
            });
        }
    }
    Ok(LoopOutcome::MaxRounds(opts.max_rounds))
}
