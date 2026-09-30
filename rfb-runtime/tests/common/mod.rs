// Shared helpers for the rfb-runtime test binaries. Not a test target on its
// own. Each test target embeds this module whole, so helpers unused by one
// target are expected here (feature-gate anything that needs a non-default
// crate feature — see the per-item cfgs below).
#![allow(dead_code)]

/// A unique, existing directory under the system temp dir: pid + nanos + a
/// process-local sequence counter, so two tests in the same binary can never
/// share a path even when they call this in the same nanosecond. Absorbs the
/// per-file `workspace()` / `temp_workspace()` / `unique_temp_dir()` copies
/// (`host_vsock`, `zeroboot_connection`, `zeroboot_v1_contract`,
/// `runtime_service`), parameterized by the readable prefix.
pub fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "{prefix}-{}-{nanos}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Build a ZBRT frame with explicit flags-free defaults (shared by
/// `zeroboot_connection::new_frame` and `zeroboot_protocol_wire::frame`).
#[cfg(feature = "core")]
pub fn frame(
    kind: rfb_runtime::zeroboot_protocol::Kind,
    request_id: [u8; 16],
    payload: Vec<u8>,
) -> rfb_runtime::zeroboot_protocol::Frame {
    rfb_runtime::zeroboot_protocol::Frame {
        kind,
        flags: 0,
        request_id,
        payload,
    }
}

#[cfg(feature = "guest")]
mod guest {
    use rfb_runtime::runtime_service::RuntimeService;
    use rfb_runtime::session::{ControlMessage, RuntimeMessage, SessionRequest};

    /// Perform the wire Hello handshake only; the returned messages are
    /// asserted to be exactly one HelloAck (the `runtime_service` /
    /// `core_gaps` ready helper).
    pub fn ready(service: &mut RuntimeService) {
        assert!(matches!(
            service
                .handle(ControlMessage::Hello {
                    protocol_version: 1
                })
                .as_slice(),
            [RuntimeMessage::HelloAck { .. }]
        ));
    }

    /// [`ready`] plus the Capabilities negotiation (the
    /// `zeroboot_v1_contract::hello` helper — the strict variant that pins
    /// the full V1 readiness sequence, echoed back verbatim).
    pub fn ready_strict(service: &mut RuntimeService) {
        assert!(matches!(
            service
                .handle(ControlMessage::Hello {
                    protocol_version: 1
                })
                .as_slice(),
            [RuntimeMessage::HelloAck {
                protocol_version: 1
            }]
        ));
        assert!(matches!(
            service
                .handle(ControlMessage::Capabilities {
                    session_per_vm: true,
                    writable_workspace: true
                })
                .as_slice(),
            [RuntimeMessage::Capabilities {
                session_per_vm: true,
                writable_workspace: true
            }]
        ));
    }

    /// Build a `SessionRequest` for `StartTurn` (the `core_gaps::turn` /
    /// `runtime_service::request` helpers, prompt parameterized).
    pub fn turn_request(session_id: &str, request_id: &str, prompt: &str) -> SessionRequest {
        SessionRequest {
            session_id: session_id.into(),
            request_id: request_id.into(),
            prompt: prompt.into(),
        }
    }

    /// [`turn_request`] wrapped in its `StartTurn` control message.
    pub fn start_turn(session_id: &str, request_id: &str, prompt: &str) -> ControlMessage {
        ControlMessage::StartTurn(turn_request(session_id, request_id, prompt))
    }
}

// common/mod.rs is embedded WHOLE into every test binary of this crate; any
// given binary uses only part of the surface, so per-binary unused-import
// warnings are inherent — allow them at the re-export seams.
#[allow(unused_imports)]
#[cfg(feature = "guest")]
pub use guest::{ready, ready_strict, start_turn, turn_request};

/// Parameterizable fake [`RuntimeWorkerAdapter`] absorbing the five plain
/// fakes of `orchestration.rs` / `orchestration_lifecycle.rs` (NoopAdapter,
/// FailingProvision, FlakyCancel, FailingCleanup, FlakyDestroy). The two
/// timing-sensitive fakes (GatedProvision's Notify gate, HangingProvision's
/// pending future) keep bespoke implementations.
#[cfg(feature = "core")]
pub mod fake_adapter {
    use async_trait::async_trait;
    use rfb_runtime::orchestration::{
        RuntimeError, RuntimeSpec, RuntimeWorkerAdapter, WorkerHandle,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    pub struct FakeWorkerAdapter {
        provisions: Arc<AtomicUsize>,
        cancels: Arc<AtomicUsize>,
        destroys: Arc<AtomicUsize>,
        provision_error: Option<String>,
        cancel_error: Option<String>,
        destroy_error: Option<String>,
        /// Fail the first N calls, then succeed (`usize::MAX` = always fail).
        cancel_fail_first: usize,
        destroy_fail_first: usize,
    }

    impl Default for FakeWorkerAdapter {
        fn default() -> Self {
            Self {
                provisions: Arc::new(AtomicUsize::new(0)),
                cancels: Arc::new(AtomicUsize::new(0)),
                destroys: Arc::new(AtomicUsize::new(0)),
                provision_error: None,
                cancel_error: None,
                destroy_error: None,
                cancel_fail_first: 0,
                destroy_fail_first: 0,
            }
        }
    }

    impl FakeWorkerAdapter {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn provisions(&self) -> Arc<AtomicUsize> {
            self.provisions.clone()
        }

        pub fn cancels(&self) -> Arc<AtomicUsize> {
            self.cancels.clone()
        }

        pub fn destroys(&self) -> Arc<AtomicUsize> {
            self.destroys.clone()
        }

        /// Every provision fails with a `Provisioning` error.
        pub fn provision_fails(mut self, message: &str) -> Self {
            self.provision_error = Some(message.to_owned());
            self
        }

        /// The first `fail_first` cancels fail with an `Adapter` error.
        pub fn cancel_fails(mut self, fail_first: usize, message: &str) -> Self {
            self.cancel_error = Some(message.to_owned());
            self.cancel_fail_first = fail_first;
            self
        }

        /// The first `fail_first` destroys fail with an `Adapter` error.
        pub fn destroy_fails(mut self, fail_first: usize, message: &str) -> Self {
            self.destroy_error = Some(message.to_owned());
            self.destroy_fail_first = fail_first;
            self
        }
    }

    #[async_trait]
    impl RuntimeWorkerAdapter for FakeWorkerAdapter {
        async fn provision(
            &self,
            _spec: RuntimeSpec,
            worker_id: String,
        ) -> Result<WorkerHandle, RuntimeError> {
            self.provisions.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = &self.provision_error {
                return Err(RuntimeError::Provisioning(message.clone()));
            }
            Ok(WorkerHandle {
                worker_id,
                workspace: None,
                guest_workspace: None,
                guest_address: None,
                sandbox_id: None,
            })
        }

        async fn cancel(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
            let attempt = self.cancels.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = &self.cancel_error {
                if attempt < self.cancel_fail_first {
                    return Err(RuntimeError::Adapter(message.clone()));
                }
            }
            Ok(())
        }

        async fn destroy(&self, _worker: &WorkerHandle) -> Result<(), RuntimeError> {
            let attempt = self.destroys.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = &self.destroy_error {
                if attempt < self.destroy_fail_first {
                    return Err(RuntimeError::Adapter(message.clone()));
                }
            }
            Ok(())
        }
    }
}

#[allow(unused_imports)]
#[cfg(feature = "core")]
pub use fake_adapter::FakeWorkerAdapter;
