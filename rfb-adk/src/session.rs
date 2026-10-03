//! 按需沙箱会话（默认 session 零沙箱）。
//!
//! 工具面初始只有 [`SANDBOX_START_NAME`]（和 [`SANDBOX_STOP_NAME`]）；
//! agent 主动要求沙箱（调 start）才引导 Firecracker VM 并把七个沙箱工具
//! 注册进 [`Toolset`]——之后 read/write/edit/bash/grep/find/ls 才对模型
//! 可见。`stop` 退役 VM 并注销工具。宿主进程在默认状态下零 VM、零成本。
//!
//! 设计要点：
//! - 一个会话至多一个沙箱（重复 start 返回现有句柄的描述——幂等）；
//! - start 失败（坏路径/无 KVM）= 错误数据回给模型，循环不中断；
//! - 沙箱工具每次 boot 后重新构建（它们捕获沙箱句柄）。

use crate::loop_agent::Toolset;
use rfb::zeroboot::{Config, ZeroBootProvider};
use rfb::{Capability, Sandbox, SandboxProvider, SandboxSpec};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 引导沙箱的工具名（默认工具面上唯一可见的沙箱相关工具）。
pub const SANDBOX_START_NAME: &str = "sandbox_start";
/// 退役沙箱的工具名。
pub const SANDBOX_STOP_NAME: &str = "sandbox_stop";

/// 沙箱引导配置（镜像/内核/Firecracker 路径与形状）。
#[derive(Clone, Debug)]
pub struct SandboxSetup {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub firecracker: PathBuf,
    pub guest_port: u32,
    pub boot_timeout: Duration,
}

impl SandboxSetup {
    /// 从仓库默认布局取路径（resx/ 与 examples 资产，相对 cwd）。
    pub fn from_repo_defaults() -> Self {
        Self {
            kernel: PathBuf::from("resx/kernel/vmlinux-arcbox-0.0.24"),
            rootfs: PathBuf::from("resx/rootfs/zeroboot-zbrt.ext4"),
            firecracker: PathBuf::from("sdk/examples/python/assets/firecracker"),
            guest_port: 5000,
            boot_timeout: Duration::from_secs(30),
        }
    }
}

/// 会话的沙箱状态：`None` = 没有沙箱（默认）。
#[derive(Default)]
pub struct SandboxState {
    sandbox: Mutex<Option<Arc<dyn Sandbox>>>,
}

impl SandboxState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    /// 当前沙箱（没有则 None）。
    pub fn current(&self) -> Option<Arc<dyn Sandbox>> {
        self.sandbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

/// 把「按需沙箱」的两个元工具装进工具集。调用后：模型只看到
/// `sandbox_start`/`sandbox_stop`；start 成功后七个沙箱工具即时可见。
pub fn install_session_tools(setup: SandboxSetup, toolset: &Toolset, state: Arc<SandboxState>) {
    toolset.register(Arc::new(SandboxStartTool {
        setup,
        toolset: toolset.clone(),
        state: state.clone(),
    }));
    toolset.register(Arc::new(SandboxStopTool {
        state,
        toolset: toolset.clone(),
    }));
}

struct SandboxStartTool {
    setup: SandboxSetup,
    toolset: Toolset,
    state: Arc<SandboxState>,
}

struct SandboxStopTool {
    state: Arc<SandboxState>,
    toolset: Toolset,
}

const START_DESCRIPTION: &str = "启动一个隔离的 Firecracker microVM 沙箱（约 1-2s），\
 并解锁 read/write/edit/bash/grep/find/ls 七个工具。只有需要在隔离环境里\
 执行命令或读写文件时才需要它；重复调用幂等返回现有沙箱。";

#[adk_rust::async_trait]
impl adk_rust::Tool for SandboxStartTool {
    fn name(&self) -> &str {
        SANDBOX_START_NAME
    }
    fn description(&self) -> &str {
        START_DESCRIPTION
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({"type":"object","additionalProperties":false}))
    }
    async fn execute(
        &self,
        _ctx: Arc<dyn adk_rust::ToolContext>,
        _args: Value,
    ) -> adk_rust::Result<Value> {
        if self.state.current().is_some() {
            return Ok(json!({"ok": true, "reused": true, "tools": SANDBOX_TOOL_NAMES}));
        }
        let provider = ZeroBootProvider::new(Config {
            kernel: Some(self.setup.kernel.clone()),
            rootfs: Some(self.setup.rootfs.clone()),
            firecracker: Some(self.setup.firecracker.clone()),
            guest_port: self.setup.guest_port,
            timeout: self.setup.boot_timeout,
        });
        let boxed = provider
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
            .map_err(|e| {
                adk_rust::AdkError::new(
                    adk_rust::ErrorComponent::Tool,
                    adk_rust::ErrorCategory::Unavailable,
                    "rfb.session.boot_failed",
                    format!("sandbox boot failed: {e}"),
                )
            })?;
        let sandbox: Arc<dyn Sandbox> = boxed.into();
        if !sandbox.ping().await.map(|h| h.healthy).unwrap_or(false) {
            return Err(adk_rust::AdkError::new(
                adk_rust::ErrorComponent::Tool,
                adk_rust::ErrorCategory::Unavailable,
                "rfb.session.boot_unhealthy",
                "sandbox booted but the guest did not answer ping",
            ));
        }
        // 解锁七个沙箱工具（每次 boot 后重建——它们捕获沙箱句柄）。
        for tool in crate::sandbox_tools(sandbox.clone()).tools() {
            self.toolset.register(tool);
        }
        *self.state.sandbox.lock().unwrap_or_else(|p| p.into_inner()) = Some(sandbox);
        Ok(json!({"ok": true, "reused": false, "tools": SANDBOX_TOOL_NAMES}))
    }
}

#[adk_rust::async_trait]
impl adk_rust::Tool for SandboxStopTool {
    fn name(&self) -> &str {
        SANDBOX_STOP_NAME
    }
    fn description(&self) -> &str {
        "退役当前沙箱（kill + reap，无孤儿进程）并注销其工具。没有沙箱时幂等成功。"
    }
    fn parameters_schema(&self) -> Option<Value> {
        Some(json!({"type":"object","additionalProperties":false}))
    }
    async fn execute(
        &self,
        _ctx: Arc<dyn adk_rust::ToolContext>,
        _args: Value,
    ) -> adk_rust::Result<Value> {
        let dropped = self
            .state
            .sandbox
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        for name in SANDBOX_TOOL_NAMES {
            self.toolset.unregister(name);
        }
        let was_live = dropped.is_some();
        drop(dropped); // Drop = kill + reap
        Ok(json!({"ok": true, "stopped": was_live}))
    }
}

/// start 解锁的七个沙箱工具（与 [`crate::adapter::TOOL_NAMES`] 一致）。
pub const SANDBOX_TOOL_NAMES: [&str; 7] = crate::adapter::TOOL_NAMES;

#[cfg(test)]
mod tests {
    use super::*;

    /// 未 start 时状态为空；start 幂等（无 VM 的纯状态机路径不走这里——
    /// 引导路径需要真机，见 tests/agent_eval.rs）。
    #[test]
    fn default_session_has_no_sandbox() {
        let state = SandboxState::new();
        assert!(state.current().is_none());
    }
}
