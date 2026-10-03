//! 回调式工具：把宿主已有的「名字 + 描述 + JSON schema + 异步回调」形状
//! 直接变成 [`adk_rust::Tool`]——承接 xpi 等宿主既有的工具桥（几十个查询
//! 工具），不需要每个都手写 trait impl。

use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

type Callback = Arc<
    dyn Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
        + Send
        + Sync,
>;

/// 一个回调式工具。回调返回模型要读的 JSON 结果；`Err(文本)` = 工具错误，
/// 由调用方决定怎么呈现（[`crate::loop_agent::agent_loop`] 会把它作为
/// `{"error": ...}` 数据回填给模型）。
#[derive(Clone)]
pub struct ClosureTool {
    name: String,
    description: String,
    parameters: Value,
    callback: Callback,
}

impl std::fmt::Debug for ClosureTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClosureTool")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl ClosureTool {
    /// 从「名字 + 描述 + 参数 schema + 异步回调」构造。
    pub fn new<F>(name: impl Into<String>, description: impl Into<String>, parameters: Value, callback: F) -> Self
    where
        F: Fn(Value) -> Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            callback: Arc::new(callback),
        }
    }

    /// 从同步返回结果的回调构造（内部升格为立即完成的 future）。
    pub fn new_sync<F>(name: impl Into<String>, description: impl Into<String>, parameters: Value, callback: F) -> Self
    where
        F: Fn(Value) -> Result<Value, String> + Send + Sync + Clone + 'static,
    {
        Self::new(
            name,
            description,
            parameters,
            move |args| {
                let callback = callback.clone();
                Box::pin(async move { callback(args) })
            },
        )
    }

    /// 直接调用回调（测试与宿主侧直接取用）。
    pub async fn call(&self, arguments: Value) -> Result<Value, String> {
        (self.callback)(arguments).await
    }
}

#[adk_rust::async_trait]
impl adk_rust::Tool for ClosureTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Option<Value> {
        Some(self.parameters.clone())
    }

    async fn execute(
        &self,
        _ctx: Arc<dyn adk_rust::ToolContext>,
        args: Value,
    ) -> adk_rust::Result<Value> {
        match (self.callback)(args).await {
            Ok(value) => Ok(value),
            Err(message) => Err(adk_rust::AdkError::new(
                adk_rust::ErrorComponent::Tool,
                adk_rust::ErrorCategory::Unavailable,
                "rfb.tool.callback_failed",
                message,
            )),
        }
    }
}
