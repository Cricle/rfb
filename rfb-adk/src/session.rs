//! 按需沙箱会话（默认 session 零沙箱）。
//!
//! 工具面初始只有 [`SANDBOX_START_NAME`]（和 [`SANDBOX_STOP_NAME`]）；
//! agent 主动要求沙箱（调 start）才引导 Firecracker VM 并把七个沙箱工具
//! 注册进 [`Toolset`]——之后 read/write/edit/bash/grep/find/ls 才对模型
//! 可见。`stop` 退役 VM 并注销工具。宿主进程在默认状态下零 VM、零成本。
//!
//! 设计要点：
//! - 一个会话至多一个沙箱：start 在启动互斥锁内完成 check+boot+注册，
//!   并发 start 不会双启动（第二个等到锁后走 reuse 幂等返回）；
//! - start 失败（坏路径/无 KVM）= 错误数据回给模型，循环不中断；
//! - 沙箱工具每次 boot 后重新构建（它们捕获沙箱句柄）；stop 后这些工具
//!   即刻注销——但在**同轮内**已被模型拿到的分发表快照仍持有旧句柄，
//!   旧沙箱要等该轮结束才真正 drop（延迟一个轮内窗口，文档化语义）。

use crate::loop_agent::Toolset;
use rfb::zeroboot::{Config, ZeroBootProvider};
use rfb::{Capability, Sandbox, SandboxProvider, SandboxSpec};
use serde_json::{json, Value};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
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

/// 沙箱引导工厂：返回活的沙箱或失败原因（失败作为错误数据回给模型）。
pub type SandboxBoot = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<Arc<dyn Sandbox>, String>> + Send>>
        + Send
        + Sync,
>;

/// 会话的沙箱状态：`None` = 没有沙箱（默认）。`boot_lock` 串行化
/// check+boot+注册的临界区——两个并发 start 只会引导一个 VM。
#[derive(Default)]
pub struct SandboxState {
    sandbox: Mutex<Option<Arc<dyn Sandbox>>>,
    boot_lock: tokio::sync::Mutex<()>,
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

fn default_boot(setup: &SandboxSetup) -> SandboxBoot {
    let setup = setup.clone();
    Arc::new(move || {
        let provider = ZeroBootProvider::new(Config {
            kernel: Some(setup.kernel.clone()),
            rootfs: Some(setup.rootfs.clone()),
            firecracker: Some(setup.firecracker.clone()),
            guest_port: setup.guest_port,
            timeout: setup.boot_timeout,
        });
        Box::pin(async move {
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
                .map_err(|e| format!("sandbox boot failed: {e}"))?;
            let sandbox: Arc<dyn Sandbox> = boxed.into();
            if !sandbox.ping().await.map(|h| h.healthy).unwrap_or(false) {
                return Err("sandbox booted but the guest did not answer ping".into());
            }
            Ok(sandbox)
        })
    })
}

/// 把「按需沙箱」的两个元工具装进工具集。调用后：模型只看到
/// `sandbox_start`/`sandbox_stop`；start 成功后七个沙箱工具即时可见。
pub fn install_session_tools(setup: SandboxSetup, toolset: &Toolset, state: Arc<SandboxState>) {
    install_session_tools_with_boot(default_boot(&setup), toolset, state);
}

/// 同 [`install_session_tools`]，但引导工厂由调用方注入（测试用假工厂；
/// 生产用 [`SandboxSetup`] 走 ZeroBootProvider）。
pub fn install_session_tools_with_boot(
    boot: SandboxBoot,
    toolset: &Toolset,
    state: Arc<SandboxState>,
) {
    toolset.register(Arc::new(SandboxStartTool {
        boot,
        toolset: toolset.clone(),
        state: state.clone(),
    }));
    toolset.register(Arc::new(SandboxStopTool {
        state,
        toolset: toolset.clone(),
    }));
}

struct SandboxStartTool {
    boot: SandboxBoot,
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
        // 启动互斥：check（reuse）与 boot+注册在同一个临界区里，并发
        // start 不会双引导 VM，也不会出现「沙箱在而工具未注册」的中间态。
        let _guard = self.state.boot_lock.lock().await;
        if self.state.current().is_some() {
            return Ok(json!({"ok": true, "reused": true, "tools": SANDBOX_TOOL_NAMES}));
        }
        let sandbox = (self.boot)().await.map_err(|message| {
            adk_rust::AdkError::new(
                adk_rust::ErrorComponent::Tool,
                adk_rust::ErrorCategory::Unavailable,
                "rfb.session.boot_failed",
                message,
            )
        })?;
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
        "退役当前沙箱（kill + reap，无孤儿进程）并注销其工具。没有沙箱时幂等成功。\
         注意：本轮已被模型看到的工具分发表仍持有旧句柄，旧 VM 在本轮结束后才真正退出。"
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

    fn memory_sandbox() -> Arc<dyn Sandbox> {
        use rfb::{BackendKind, BoxFuture, ExecResult, ExecSpec, SandboxError, TransportKind};
        #[derive(Default)]
        struct Mem;
        impl Sandbox for Mem {
            fn backend(&self) -> BackendKind {
                BackendKind::InMemory
            }
            fn transport(&self) -> TransportKind {
                TransportKind::InProcess
            }
            fn capabilities(&self) -> &[Capability] {
                static CAPS: [Capability; 7] = [
                    Capability::ReadFile,
                    Capability::WriteFile,
                    Capability::Execute,
                    Capability::Grep,
                    Capability::Find,
                    Capability::Ls,
                    Capability::Health,
                ];
                &CAPS
            }
            fn exec<'a>(&'a self, _: ExecSpec) -> BoxFuture<'a, Result<ExecResult, SandboxError>> {
                Box::pin(async { Err(SandboxError::NotReady) })
            }
        }
        Arc::new(Mem)
    }

    fn boot_ok(counter: Arc<std::sync::atomic::AtomicUsize>) -> SandboxBoot {
        Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let sandbox = memory_sandbox();
            Box::pin(async move { Ok(sandbox) })
        })
    }

    fn boot_failing() -> SandboxBoot {
        Arc::new(|| Box::pin(async { Err("no kvm".into()) }))
    }

    #[tokio::test]
    async fn default_session_has_no_sandbox() {
        let state = SandboxState::new();
        assert!(state.current().is_none());
    }

    #[tokio::test]
    async fn start_unlocks_tools_and_stop_removes_them() {
        let toolset = Toolset::new();
        let state = SandboxState::new();
        install_session_tools_with_boot(boot_ok(Arc::default()), &toolset, state.clone());
        assert_eq!(toolset.names(), vec![SANDBOX_START_NAME, SANDBOX_STOP_NAME]);

        let start = toolset
            .snapshot()
            .into_iter()
            .find(|t| t.name() == SANDBOX_START_NAME)
            .unwrap();
        let ctx: Arc<dyn adk_rust::ToolContext> = Arc::new(crate::NoOpToolContext);
        let out = start.execute(ctx.clone(), json!({})).await.unwrap();
        assert_eq!(out["reused"], json!(false));
        assert!(state.current().is_some());
        let names = toolset.names();
        for name in SANDBOX_TOOL_NAMES {
            assert!(names.contains(&name.to_owned()), "{name} must be unlocked");
        }

        // 幂等 start
        let out = start.execute(ctx.clone(), json!({})).await.unwrap();
        assert_eq!(out["reused"], json!(true));

        // stop 注销 + 状态清空
        let stop = toolset
            .snapshot()
            .into_iter()
            .find(|t| t.name() == SANDBOX_STOP_NAME)
            .unwrap();
        let out = stop.execute(ctx.clone(), json!({})).await.unwrap();
        assert_eq!(out["stopped"], json!(true));
        assert!(state.current().is_none());
        for name in SANDBOX_TOOL_NAMES {
            assert!(!toolset.names().contains(&name.to_owned()));
        }
        // 幂等 stop
        let out = stop.execute(ctx, json!({})).await.unwrap();
        assert_eq!(out["stopped"], json!(false));
    }

    #[tokio::test]
    async fn concurrent_starts_boot_exactly_one_vm() {
        let toolset = Toolset::new();
        let state = SandboxState::new();
        let boots = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        install_session_tools_with_boot(boot_ok(boots.clone()), &toolset, state.clone());
        let start = toolset
            .snapshot()
            .into_iter()
            .find(|t| t.name() == SANDBOX_START_NAME)
            .unwrap();
        let ctx: Arc<dyn adk_rust::ToolContext> = Arc::new(crate::NoOpToolContext);
        let mut handles = Vec::new();
        for _ in 0..5 {
            let start = start.clone();
            let ctx = ctx.clone();
            handles.push(tokio::spawn(
                async move { start.execute(ctx, json!({})).await },
            ));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert_eq!(
            boots.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "并发 start 只能引导一个 VM"
        );
    }

    #[tokio::test]
    async fn boot_failure_is_error_data_and_tools_stay_hidden() {
        let toolset = Toolset::new();
        let state = SandboxState::new();
        install_session_tools_with_boot(boot_failing(), &toolset, state.clone());
        let start = toolset
            .snapshot()
            .into_iter()
            .find(|t| t.name() == SANDBOX_START_NAME)
            .unwrap();
        let ctx: Arc<dyn adk_rust::ToolContext> = Arc::new(crate::NoOpToolContext);
        let error = start.execute(ctx, json!({})).await.unwrap_err();
        assert!(error.message.contains("no kvm"));
        assert!(state.current().is_none());
        assert_eq!(
            toolset.names(),
            vec![SANDBOX_START_NAME, SANDBOX_STOP_NAME],
            "失败后沙箱工具不得出现"
        );
    }
}
