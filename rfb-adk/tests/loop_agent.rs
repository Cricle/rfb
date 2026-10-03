//! pi 风格循环的验收：假模型（可编程的 turn 序列）+ **真实沙箱工具** +
//! 真实循环——验证循环语义本身：完成、转向注入、中止、输出截断、未知
//! 工具错误回填后循环继续。

use adk_rust::{Content, LlmRequest, LlmResponse, LlmResponseStream, Part};
use rfb::{
    BackendKind, BoxFuture, Capability, ExecResult, ExecSpec, Sandbox, SandboxError, TransportKind,
};
use rfb_adk::{agent_loop, AbortFlag, LoopOptions, LoopOutcome, SteeringInbox};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

/// 内存结构化沙箱（write/read 往返 + 记录调用）。
#[derive(Clone, Default)]
struct MemorySandbox {
    files: Arc<Mutex<std::collections::HashMap<String, Vec<u8>>>>,
    calls: Arc<AtomicUsize>,
}

impl Sandbox for MemorySandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 3] = [
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Health,
        ];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn read<'a>(
        &'a self,
        request: rfb::guest::ReadRequest,
    ) -> BoxFuture<'a, Result<rfb::guest::ReadResult, SandboxError>> {
        let files = self.files.clone();
        Box::pin(async move {
            let files = files.lock().unwrap();
            let data = files.get(&request.path).cloned().unwrap_or_default();
            let total = data.len() as u64;
            Ok(rfb::guest::ReadResult {
                data,
                truncated: false,
                total_bytes: Some(total),
            })
        })
    }
    fn write<'a>(
        &'a self,
        request: rfb::guest::WriteRequest,
    ) -> BoxFuture<'a, Result<rfb::guest::WriteResult, SandboxError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let files = self.files.clone();
        Box::pin(async move {
            let mut files = files.lock().unwrap();
            let mut data = if request.append {
                files.get(&request.path).cloned().unwrap_or_default()
            } else {
                Vec::new()
            };
            data.extend_from_slice(&request.data);
            let bytes = data.len() as u64;
            files.insert(request.path, data);
            Ok(rfb::guest::WriteResult {
                bytes_written: bytes,
            })
        })
    }
}

/// 可编程假模型：按调用次序回放脚本（function call / 文本）。
struct FakeModel {
    script: Mutex<Vec<Content>>,
    requests: Mutex<Vec<LlmRequest>>,
}

impl FakeModel {
    fn new(script: Vec<Content>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn call(name: &str, args: Value) -> Content {
        Content {
            role: "model".into(),
            parts: vec![Part::FunctionCall {
                name: name.into(),
                args,
                id: Some(format!("call-{name}")),
                thought_signature: None,
            }],
        }
    }
    fn text(t: &str) -> Content {
        Content {
            role: "model".into(),
            parts: vec![Part::Text { text: t.into() }],
        }
    }
}

#[adk_rust::async_trait]
impl adk_rust::Llm for FakeModel {
    fn name(&self) -> &str {
        "fake"
    }
    async fn generate_content(
        &self,
        req: LlmRequest,
        _stream: bool,
    ) -> adk_rust::Result<LlmResponseStream> {
        self.requests.lock().unwrap().push(req);
        let mut script = self.script.lock().unwrap();
        let next = if script.is_empty() {
            None
        } else {
            Some(script.remove(0))
        };
        let content = next.unwrap_or_else(|| Self::text("done"));
        let response = LlmResponse {
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

fn tools(sandbox: Arc<dyn Sandbox>) -> rfb_adk::Toolset {
    let set = rfb_adk::Toolset::new();
    for tool in rfb_adk::sandbox_tools(sandbox).tools() {
        set.register(tool);
    }
    set
}

#[tokio::test]
async fn loop_completes_a_write_read_task_with_real_tools() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(MemorySandbox::default());
    let model = FakeModel::new(vec![
        FakeModel::call("write", json!({"path": "out.txt", "data": [104, 105]})),
        FakeModel::call("read", json!({"path": "out.txt"})),
        FakeModel::text("任务完成：写了两字节并读回"),
    ]);
    let mut messages = vec![Content::new("user").with_text("写 out.txt 再读回")];

    let outcome = agent_loop(
        model.clone(),
        &tools(sandbox.clone()),
        &mut messages,
        LoopOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        LoopOutcome::Done("任务完成：写了两字节并读回".into())
    );
    // 真实工具真的执行了：文件在沙箱里
    let files = sandbox
        .read_file(rfb::guest::ReadRequest::new("out.txt"))
        .await
        .unwrap();
    assert_eq!(files.data, b"hi");
    // 消息形状：user + model(write) + tool + model(read) + tool + model(final)
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[2].role, "tool");
    // 第二轮请求里带上了第一条工具结果（循环闭合）
    let requests = model.requests.lock().unwrap();
    assert!(requests[1].contents.iter().any(|c| c.role == "tool"));
    // 工具声明随请求携带
    assert!(requests[0].tools.contains_key("write"));
    assert!(requests[0].tools.contains_key("read"));
}

#[tokio::test]
async fn steering_messages_arrive_as_user_turns_between_rounds() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(MemorySandbox::default());
    let inbox = SteeringInbox::new();
    let model = FakeModel::new(vec![
        FakeModel::call("read", json!({"path": "a.txt"})),
        FakeModel::text("收到转向，按新要求收尾"),
    ]);
    let mut messages = vec![Content::new("user").with_text("开始")];
    // 循环开始前就投一条转向（循环第一轮 drain 后注入）
    inbox.send("改成只报告文件大小");

    let outcome = agent_loop(
        model.clone(),
        &tools(sandbox),
        &mut messages,
        LoopOptions {
            inbox: Some(inbox),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::Done(_)));

    // 第二轮（模型看到 read 结果）的请求里，转向消息作为 user 内容出现
    let requests = model.requests.lock().unwrap();
    let second = &requests[1];
    assert!(
        second.contents.iter().any(|c| c.role == "user"
            && c.parts
                .iter()
                .any(|p| matches!(p, Part::Text { text } if text.contains("改成只报告文件大小")))),
        "steering must be a plain user message in the next round"
    );
}

#[tokio::test]
async fn abort_stops_the_loop_between_rounds() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(MemorySandbox::default());
    let abort = AbortFlag::new();
    // 无限调用的脚本（pop 后 fallback "done" 不会触发——用 endless 工具调用）
    let model = FakeModel::new(vec![
        FakeModel::call("read", json!({"path": "a.txt"})),
        FakeModel::call("read", json!({"path": "a.txt"})),
        FakeModel::call("read", json!({"path": "a.txt"})),
    ]);
    let mut messages = vec![Content::new("user").with_text("长任务")];
    abort.abort();

    let outcome = agent_loop(
        model,
        &tools(sandbox),
        &mut messages,
        LoopOptions {
            abort: Some(abort),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome, LoopOutcome::Aborted);
    // 一轮都没跑：历史里只有最初的用户消息
    assert_eq!(messages.len(), 1);
}

#[tokio::test]
async fn oversized_tool_output_is_capped_in_history() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(MemorySandbox::default());
    let big = vec![b'x'; 100_000];
    sandbox
        .write_file(rfb::guest::WriteRequest::new("big.txt", big))
        .await
        .unwrap();
    let model = FakeModel::new(vec![
        FakeModel::call("read", json!({"path": "big.txt"})),
        FakeModel::text("read done"),
    ]);
    let mut messages = vec![Content::new("user").with_text("读大文件")];

    agent_loop(
        model.clone(),
        &tools(sandbox),
        &mut messages,
        LoopOptions {
            tool_output_chars: 2000,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // 回填进历史的工具结果被截断
    let tool_msg = &messages[2];
    let Part::FunctionResponse {
        function_response, ..
    } = &tool_msg.parts[0]
    else {
        panic!("expected function response");
    };
    let encoded = function_response.response.to_string();
    assert!(
        encoded.chars().count() < 3000,
        "not capped: {}",
        encoded.len()
    );
    assert!(encoded.contains("truncated"));
}

#[tokio::test]
async fn unknown_tool_and_tool_errors_flow_back_as_data() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(MemorySandbox::default());
    let model = FakeModel::new(vec![
        // 不存在的工具 → 错误数据
        FakeModel::call("teleport", json!({})),
        // 路径逃逸 → 工具错误数据（循环必须继续；内存沙箱对缺失文件返回空
        // 数据而非错误，所以这里用路径校验制造真实工具错误）
        FakeModel::call("read", json!({"path": "../escape"})),
        FakeModel::text("两处都失败了，但我还在"),
    ]);
    let mut messages = vec![Content::new("user").with_text("走弯路")];

    let outcome = agent_loop(
        model.clone(),
        &tools(sandbox),
        &mut messages,
        LoopOptions::default(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, LoopOutcome::Done(_)));

    // 错误以 {"error": ...} 数据回填给模型，而非中断
    let requests = model.requests.lock().unwrap();
    let third = &requests[2];
    let error_messages: Vec<&Content> =
        third.contents.iter().filter(|c| c.role == "tool").collect();
    assert_eq!(error_messages.len(), 2, "both errors must be in history");
    for message in error_messages {
        let Part::FunctionResponse {
            function_response, ..
        } = &message.parts[0]
        else {
            panic!("expected function response");
        };
        assert!(function_response.response.get("error").is_some());
    }
}

/// 一个会自我了断的工具：执行时置 abort 标志并返回成功——用它验证
/// abort 在**同一轮的两个工具之间**生效（第一个工具跑完即停）。
struct AborterTool {
    abort: rfb_adk::AbortFlag,
    executions: Arc<AtomicUsize>,
}

#[adk_rust::async_trait]
impl adk_rust::Tool for AborterTool {
    fn name(&self) -> &str {
        "aborter"
    }
    fn description(&self) -> &str {
        "sets the abort flag"
    }
    async fn execute(
        &self,
        _ctx: Arc<dyn adk_rust::ToolContext>,
        _args: Value,
    ) -> adk_rust::Result<Value> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.abort.abort();
        Ok(json!({"ok": true}))
    }
}

#[derive(Default)]
struct CountingSandbox {
    reads: Arc<AtomicUsize>,
}
impl Sandbox for CountingSandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 1] = [Capability::ReadFile];
        &CAPS
    }
    fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
        Box::pin(async { Err(SandboxError::NotReady) })
    }
    fn read<'a>(
        &'a self,
        _: rfb::guest::ReadRequest,
    ) -> BoxFuture<'a, Result<rfb::guest::ReadResult, SandboxError>> {
        let reads = self.reads.clone();
        Box::pin(async move {
            reads.fetch_add(1, Ordering::SeqCst);
            Ok(rfb::guest::ReadResult {
                data: vec![],
                truncated: false,
                total_bytes: Some(0),
            })
        })
    }
}

#[tokio::test]
async fn abort_stops_between_tool_calls_within_one_round() {
    let sandbox = Arc::new(CountingSandbox::default());
    let abort = rfb_adk::AbortFlag::new();
    let executions = Arc::new(AtomicUsize::new(0));
    let aborter: Arc<dyn adk_rust::Tool> = Arc::new(AborterTool {
        abort: abort.clone(),
        executions: executions.clone(),
    });
    let set = rfb_adk::Toolset::new();
    set.register(aborter);
    for tool in rfb_adk::sandbox_tools(sandbox.clone()).tools() {
        set.register(tool);
    }
    // 同一轮两个调用：aborter 先跑（置标志），read 必须不再执行
    let model = FakeModel::new(vec![Content {
        role: "model".into(),
        parts: vec![
            Part::FunctionCall {
                name: "aborter".into(),
                args: json!({}),
                id: Some("c1".into()),
                thought_signature: None,
            },
            Part::FunctionCall {
                name: "read".into(),
                args: json!({"path": "x"}),
                id: Some("c2".into()),
                thought_signature: None,
            },
        ],
    }]);
    let mut messages = vec![Content::new("user").with_text("go")];
    let outcome = agent_loop(
        model,
        &set,
        &mut messages,
        rfb_adk::LoopOptions {
            abort: Some(abort),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome, rfb_adk::LoopOutcome::Aborted);
    assert_eq!(executions.load(Ordering::SeqCst), 1, "aborter 跑了一次");
    assert_eq!(
        sandbox.reads.load(Ordering::SeqCst),
        0,
        "同轮后继工具不得执行"
    );
}
