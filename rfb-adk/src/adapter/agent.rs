//! Assemble a ready-to-run ADK agent over a live RFB sandbox.
//!
//! The assembly mirrors the production harness pattern (monitor's xpi
//! author-agent): an OpenAI-compatible model client forced onto the
//! non-streaming path, an `LlmAgent` with the sandbox's seven tools, and an
//! in-memory session under a `Runner`. The application owns credentials and
//! the conversation; this module only owns the wiring.

use super::tools::{sandbox_tools, SandboxTools};
use rfb::Sandbox;
use std::sync::Arc;
use std::time::Duration;

/// Model wiring for an OpenAI-compatible endpoint (any gateway that serves
/// `chat/completions`).
#[derive(Debug, Clone)]
pub struct SandboxAgentConfig {
    /// API key for the endpoint (never logged).
    pub api_key: String,
    /// Base URL, e.g. `https://gateway.internal/v1`.
    pub base_url: String,
    /// Model name as the endpoint knows it.
    pub model: String,
    /// Per-call HTTP ceiling for one model round trip.
    pub model_timeout: Duration,
    /// Max agent turns (tool-call rounds) before the runner gives up.
    pub max_iterations: u32,
}

impl Default for SandboxAgentConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            base_url: String::new(),
            model: String::new(),
            model_timeout: Duration::from_secs(300),
            max_iterations: 30,
        }
    }
}

/// Force the model client onto the non-streaming code path.
///
/// Gateways that only half-support SSE can accept a streaming request and
/// stall it forever (measured in the production harness: 13 minutes with zero
/// turns on the streaming path). The non-streaming path parses the whole
/// response — including `usage` — as one unit. Request construction, schema
/// adaptation and retry all stay in adk's client; this wrapper only forces
/// the path and bounds each call.
struct NonStreamingModel {
    inner: adk_rust::model::openai::OpenAIClient,
    name: String,
    timeout: Duration,
}

impl NonStreamingModel {
    fn new(inner: adk_rust::model::openai::OpenAIClient, timeout: Duration) -> Self {
        use adk_rust::Llm as _;
        let name = inner.name().to_owned();
        Self {
            inner,
            name,
            timeout,
        }
    }
}

#[adk_rust::async_trait]
impl adk_rust::Llm for NonStreamingModel {
    fn name(&self) -> &str {
        &self.name
    }

    async fn generate_content(
        &self,
        req: adk_rust::LlmRequest,
        _stream: bool,
    ) -> adk_rust::Result<adk_rust::LlmResponseStream> {
        use adk_rust::futures::StreamExt;
        let call = async {
            let stream = self.inner.generate_content(req, false).await?;
            let mut pinned = std::pin::pin!(stream);
            let mut last: Option<adk_rust::LlmResponse> = None;
            while let Some(item) = pinned.next().await {
                last = Some(item?);
            }
            last.ok_or_else(|| {
                adk_rust::AdkError::new(
                    adk_rust::ErrorComponent::Model,
                    adk_rust::ErrorCategory::Internal,
                    "rfb.agent.empty_response",
                    "model returned an empty response stream",
                )
            })
        };
        let resp = tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| {
                adk_rust::AdkError::new(
                    adk_rust::ErrorComponent::Model,
                    adk_rust::ErrorCategory::Timeout,
                    "rfb.agent.model_timeout",
                    format!("model call exceeded {}s", self.timeout.as_secs()),
                )
            })??;
        Ok(Box::pin(adk_rust::futures::stream::once(async move {
            Ok(resp)
        })))
    }

    fn schema_adapter(&self) -> &dyn adk_rust::SchemaAdapter {
        self.inner.schema_adapter()
    }

    fn uses_interactions_api(&self) -> bool {
        self.inner.uses_interactions_api()
    }
}

/// A ready-to-run agent over a live sandbox: the [`adk_rust::agent::LlmAgent`],
/// its session service (in-memory), and a session created in it. Drive turns
/// through [`SandboxAgent::run`] (one message in, the full event sequence
/// consumed, final text out) or use `runner` directly for streaming access.
pub struct SandboxAgent {
    pub runner: adk_rust::runner::Runner,
    pub tools: SandboxTools,
    /// The in-memory session service the agent's session lives in (exposed
    /// for callers that want direct session access).
    pub sessions: Arc<adk_rust::session::InMemorySessionService>,
    pub user_id: adk_rust::UserId,
    pub session_id: adk_rust::SessionId,
}

impl SandboxAgent {
    /// Drive one user message through the agent to completion, returning the
    /// assistant's final text plus the turn/tool-call counters.
    ///
    /// # Errors
    ///
    /// Returns `Err` when the model call or an agent turn fails.
    pub async fn run(&self, message: &str) -> adk_rust::Result<SandboxAgentReply> {
        use adk_rust::futures::StreamExt;
        let mut stream = self
            .runner
            .run(
                self.user_id.clone(),
                self.session_id.clone(),
                adk_rust::Content::new("user").with_text(message),
            )
            .await?;
        let mut text = String::new();
        let mut tool_calls = 0usize;
        let mut turns = 0usize;
        while let Some(event) = stream.next().await {
            let event = event?;
            tool_calls += event.tool_calls().len();
            if let Some(content) = event.content() {
                for part in &content.parts {
                    if let adk_rust::Part::Text { text: t } = part {
                        if !t.trim().is_empty() {
                            turns += 1;
                            text.push_str(t);
                        }
                    }
                }
            }
        }
        Ok(SandboxAgentReply {
            text,
            tool_calls,
            turns,
        })
    }
}

/// One completed agent turn-sequence.
#[derive(Debug, Clone)]
pub struct SandboxAgentReply {
    /// The assistant's accumulated text.
    pub text: String,
    /// How many tool calls the agent made.
    pub tool_calls: usize,
    /// How many non-empty text parts the model produced.
    pub turns: usize,
}

/// Assemble an ADK agent over a live sandbox with the default seven-tool
/// surface and an OpenAI-compatible model (any `chat/completions` gateway).
/// `instruction` is used verbatim (no session-state templating — the sandbox
/// tool docs carry literal braces).
///
/// # Errors
///
/// Returns `Err` when the model client or the runner cannot be built.
pub async fn sandbox_agent(
    name: &str,
    instruction: &str,
    sandbox: Arc<dyn Sandbox>,
    config: &SandboxAgentConfig,
) -> adk_rust::Result<SandboxAgent> {
    let model = adk_rust::model::openai::OpenAIClient::new(
        adk_rust::model::openai::OpenAIConfig::compatible(
            config.api_key.clone(),
            config.base_url.clone(),
            config.model.clone(),
        ),
    )?;
    let model: Arc<dyn adk_rust::Llm> =
        Arc::new(NonStreamingModel::new(model, config.model_timeout));
    sandbox_agent_with_model(name, instruction, sandbox, model, config.max_iterations).await
}

/// Assemble the same agent over a caller-supplied model — test doubles and
/// non-OpenAI providers plug in here without touching the tool surface.
///
/// # Errors
///
/// Returns `Err` when the runner cannot be built.
pub async fn sandbox_agent_with_model(
    name: &str,
    instruction: &str,
    sandbox: Arc<dyn Sandbox>,
    model: Arc<dyn adk_rust::Llm>,
    max_iterations: u32,
) -> adk_rust::Result<SandboxAgent> {
    // instruction_provider, not instruction: the latter runs the string
    // through adk's session-state templating, and tool docs / prompts carry
    // literal braces that would read as required state variables.
    let instruction_text = instruction.to_owned();
    let provider: adk_rust::InstructionProvider = Box::new(move |_ctx| {
        let t = instruction_text.clone();
        Box::pin(async move { Ok(t) })
    });
    let tools = sandbox_tools(sandbox);
    let mut builder = adk_rust::agent::LlmAgentBuilder::new(name.to_owned())
        .instruction_provider(provider)
        .model(model)
        .max_iterations(max_iterations);
    for tool in tools.tools() {
        builder = builder.tool(tool);
    }
    let agent = builder.build()?;
    let sessions = Arc::new(adk_rust::session::InMemorySessionService::new());
    let runner = adk_rust::runner::RunnerConfigBuilder::new()
        .app_name(name)
        .agent(Arc::new(agent))
        .session_service(sessions.clone())
        .build()?;
    // The runner's app_name must match the session's for run() to find it.
    use adk_rust::session::SessionService as _;
    let user_id = adk_rust::UserId::new("rfb-user")?;
    let session = sessions
        .create(adk_rust::session::CreateRequest {
            app_name: name.to_owned(),
            user_id: "rfb-user".to_owned(),
            session_id: None,
            state: Default::default(),
        })
        .await?;
    let session_id = adk_rust::SessionId::new(session.id())?;
    Ok(SandboxAgent {
        runner,
        tools,
        sessions,
        user_id,
        session_id,
    })
}
