//! The full ADK agent loop over the sandbox tools: a scripted model drives
//! `sandbox_agent`'s real `Runner` — tool dispatch, function-response round
//! trips, and session continuity — without a network. The only scripted piece
//! is the LLM; everything else is the production assembly.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use adk_rust::{Content, LlmRequest, LlmResponse, LlmResponseStream, Part};
use rfb::{
    BackendKind, BoxFuture, Capability, ExecResult, ExecSpec, Sandbox, SandboxError, TransportKind,
};
use rfb_adk::{SandboxAgentConfig, TOOL_NAMES};
use serde_json::{json, Value};

/// In-memory structured sandbox: write/read round trip without a VM.
#[derive(Clone, Default)]
struct MemorySandbox {
    files: Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>>,
}

impl Sandbox for MemorySandbox {
    fn backend(&self) -> BackendKind {
        BackendKind::InMemory
    }
    fn transport(&self) -> TransportKind {
        TransportKind::InProcess
    }
    fn capabilities(&self) -> &[Capability] {
        static CAPS: [Capability; 4] = [
            Capability::ReadFile,
            Capability::WriteFile,
            Capability::Execute,
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

/// A scripted model: replays a fixed sequence of responses and counts how
/// often the runner asked it to generate (i.e., that tool results flow back).
struct ScriptedModel {
    responses: Vec<Content>,
    generate_calls: AtomicUsize,
    /// Set by the model each call: whether the request carried function
    /// responses (proof the tool loop is closed).
    saw_function_response: std::sync::Mutex<Vec<bool>>,
}

impl ScriptedModel {
    fn new(responses: Vec<Content>) -> Arc<Self> {
        Arc::new(Self {
            responses,
            generate_calls: AtomicUsize::new(0),
            saw_function_response: std::sync::Mutex::new(Vec::new()),
        })
    }
}

fn contains_function_response(req: &LlmRequest) -> bool {
    req.contents.iter().any(|content| {
        content
            .parts
            .iter()
            .any(|part| matches!(part, Part::FunctionResponse { .. }))
    })
}

#[adk_rust::async_trait]
impl adk_rust::Llm for ScriptedModel {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn generate_content(
        &self,
        req: LlmRequest,
        _stream: bool,
    ) -> adk_rust::Result<LlmResponseStream> {
        let index = self.generate_calls.fetch_add(1, Ordering::SeqCst);
        self.saw_function_response
            .lock()
            .unwrap()
            .push(contains_function_response(&req));
        let content = self
            .responses
            .get(index)
            .cloned()
            .unwrap_or_else(|| Content {
                role: "model".into(),
                parts: vec![Part::Text {
                    text: "script exhausted".into(),
                }],
            });
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

fn function_call(name: &str, args: Value) -> Content {
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

#[tokio::test]
async fn sandbox_agent_drives_tools_through_the_real_runner() {
    let sandbox: Arc<dyn Sandbox> = Arc::new(MemorySandbox::default());
    let model = ScriptedModel::new(vec![
        // Turn 1: the model writes a file through the sandbox tool.
        function_call("write", json!({"path": "agent.txt", "data": [104, 105]})),
        // Turn 2: reads it back to verify.
        function_call("read", json!({"path": "agent.txt"})),
        // Turn 3: final answer.
        Content {
            role: "model".into(),
            parts: vec![Part::Text {
                text: "file verified".into(),
            }],
        },
    ]);

    let config = SandboxAgentConfig::default();
    let agent = rfb_adk::sandbox_agent_with_model(
        "rfb-adk-loop-test",
        "You operate an RFB sandbox. Use the tools to complete the task.",
        sandbox,
        model.clone(),
        config.max_iterations,
    )
    .await
    .expect("agent assembly");

    // The seven-tool surface is gated by the sandbox's capabilities; this
    // sandbox advertises read/write/execute (+health ping not on the
    // default surface), so bash shows up but grep/find/ls do not.
    let surface = agent.tools.tools();
    let names: Vec<&str> = surface.iter().map(|t| t.name()).collect();
    assert!(names.contains(&"write") && names.contains(&"read") && names.contains(&"bash"));
    assert_eq!(
        TOOL_NAMES.len(),
        7,
        "the stable surface contract is unchanged"
    );

    let reply = agent
        .run("write hi to agent.txt and verify it")
        .await
        .unwrap();
    assert_eq!(reply.text, "file verified");
    assert_eq!(reply.tool_calls, 2, "write + read");

    assert_eq!(model.generate_calls.load(Ordering::SeqCst), 3);
    let seen = model.saw_function_response.lock().unwrap();
    // Call 1 carries no tool result yet; calls 2 and 3 must carry the closed
    // tool loop (function responses in the conversation).
    assert!(!seen[0], "first call has no function response yet");
    assert!(seen[1], "tool result must flow back before the next call");
    assert!(seen[2], "tool result must flow back before the final call");
}

#[tokio::test]
async fn sandbox_agent_tool_error_reaches_the_model_and_the_loop_continues() {
    // A scripted model that misfires once (invalid write args), sees the tool
    // error, corrects itself, and finishes — the error taxonomy must let the
    // loop continue (not abort the run).
    let sandbox: Arc<dyn Sandbox> = Arc::new(MemorySandbox::default());
    let model = ScriptedModel::new(vec![
        // Turn 1: invalid write (byte 256) → tool error.
        function_call("write", json!({"path": "x.txt", "data": [256]})),
        // Turn 2: corrected write.
        function_call("write", json!({"path": "x.txt", "data": [111, 107]})),
        // Turn 3: verify + finish.
        function_call("read", json!({"path": "x.txt"})),
        Content {
            role: "model".into(),
            parts: vec![Part::Text {
                text: "recovered".into(),
            }],
        },
    ]);

    let config = SandboxAgentConfig::default();
    let agent = rfb_adk::sandbox_agent_with_model(
        "rfb-adk-error-test",
        "You operate an RFB sandbox.",
        sandbox,
        model.clone(),
        config.max_iterations,
    )
    .await
    .unwrap();
    let reply = agent.run("do the flawed write then recover").await.unwrap();
    assert_eq!(reply.text, "recovered");
    assert_eq!(reply.tool_calls, 3);
    assert_eq!(model.generate_calls.load(Ordering::SeqCst), 4);
}
