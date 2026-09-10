//! Guest-side runtime state machine: identity, sequencing, lifecycle, and
//! protocol safety for the RFB1 framed runtime.

mod executor;
mod identity;
mod lifecycle;
mod response;

pub use executor::{GuestEvent, GuestExecutor};
pub use lifecycle::{parse_runtime_backend, RuntimeBackend};

use crate::resources::RuntimeLimits;
use crate::session::{ControlMessage, RuntimeMessage, SessionEvent, SessionRequest};
use identity::{request_id, validate_identity};
use response::response_completes_turn;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Completed turns retained for duplicate detection and replay. Beyond this
/// bound the oldest results are evicted so a long-lived daemon's bookkeeping
/// stays bounded; evicted keys move to a bounded tombstone queue so a late
/// replay fails explicitly instead of silently re-executing the turn.
const COMPLETED_LIMIT: usize = 256;

/// Cancelled turns retained for duplicate-cancel detection and replay, bounded
/// like [`COMPLETED_LIMIT`]. An entry evicted from here degrades gracefully:
/// a late cancel answers "request is not active" and a late completion falls
/// back to the worker's own terminal result.
const CANCELLED_LIMIT: usize = 256;

/// Guest runtime state machine: identity, sequencing, lifecycle, and protocol
/// safety for RFB1 control messages.
pub struct RuntimeService {
    sequence: u64,
    protocol_ready: bool,
    completed_requests: HashMap<(String, String), Vec<RuntimeMessage>>,
    /// Insertion order of `completed_requests` keys, for oldest-first eviction
    /// past [`COMPLETED_LIMIT`].
    completed_order: VecDeque<(String, String)>,
    /// Bounded tombstones for completed turns whose result was evicted: a
    /// late replay gets an explicit "no longer available" error instead of a
    /// silent re-execution.
    evicted_requests: VecDeque<(String, String)>,
    active_sessions: HashMap<String, String>,
    cancelled_sessions: HashMap<(String, String), Vec<RuntimeMessage>>,
    /// Insertion order of `cancelled_sessions` keys, for oldest-first eviction
    /// past [`CANCELLED_LIMIT`].
    cancelled_order: VecDeque<(String, String)>,
    shutdown: bool,
    limits: RuntimeLimits,
    /// `None` while an in-flight turn has taken the executor out to run it on
    /// a worker thread. During that window file RPCs and new turns are
    /// rejected; `Cancel` only touches the shared cancellation flag.
    executor: Option<Box<dyn GuestExecutor>>,
    turn_cancel: Option<Arc<AtomicBool>>,
}

impl RuntimeService {
    fn executor_mut(&mut self) -> &mut dyn GuestExecutor {
        self.executor
            .as_mut()
            .map(|boxed| boxed.as_mut())
            .expect("executor is present outside an in-flight turn")
    }

    /// Record a completed turn's responses, evicting the oldest entry past
    /// [`COMPLETED_LIMIT`]. Evicted keys move to the bounded tombstone queue
    /// (`evicted_requests`) so late replays can be rejected explicitly.
    fn record_completed(&mut self, key: (String, String), responses: Vec<RuntimeMessage>) {
        if !self.completed_requests.contains_key(&key) {
            self.completed_order.push_back(key.clone());
        }
        self.completed_requests.insert(key, responses);
        while self.completed_order.len() > COMPLETED_LIMIT {
            if let Some(oldest) = self.completed_order.pop_front() {
                self.completed_requests.remove(&oldest);
                self.evicted_requests.push_back(oldest);
            }
            while self.evicted_requests.len() > COMPLETED_LIMIT {
                self.evicted_requests.pop_front();
            }
        }
    }

    /// Record a cancelled turn's terminal responses, evicting the oldest
    /// entry past [`CANCELLED_LIMIT`] (no tombstone: a late cancel for an
    /// evicted request already fails with "request is not active").
    fn record_cancelled(&mut self, key: (String, String), responses: Vec<RuntimeMessage>) {
        if !self.cancelled_sessions.contains_key(&key) {
            self.cancelled_order.push_back(key.clone());
        }
        self.cancelled_sessions.insert(key, responses);
        while self.cancelled_order.len() > CANCELLED_LIMIT {
            if let Some(oldest) = self.cancelled_order.pop_front() {
                self.cancelled_sessions.remove(&oldest);
            }
        }
    }

    /// Explicit rejection for a request whose completed result was evicted.
    fn evicted_replay_error(request_id: &str) -> RuntimeMessage {
        RuntimeMessage::Error {
            request_id: request_id.to_string(),
            message: "request completed earlier; result no longer available".into(),
        }
    }

    /// Handle one control message and return the responses to send back.
    pub fn handle(&mut self, request: ControlMessage) -> Vec<RuntimeMessage> {
        let _limits = &self.limits;
        if self.shutdown && !matches!(request, ControlMessage::Shutdown) {
            return vec![RuntimeMessage::Error {
                request_id: request_id(&request),
                message: "runtime has shut down".into(),
            }];
        }
        // While an in-flight turn owns the executor (moved to a worker), only
        // control messages that do not need it remain servable. Cancel routes
        // to the shared flag; it must never call `handle_cancel` because the
        // executor is absent.
        if self.executor.is_none() {
            return match &request {
                ControlMessage::Hello { protocol_version } => {
                    if *protocol_version != 1 {
                        vec![RuntimeMessage::Error {
                            request_id: String::new(),
                            message: format!("unsupported protocol version: {protocol_version}"),
                        }]
                    } else {
                        self.protocol_ready = true;
                        vec![RuntimeMessage::HelloAck {
                            protocol_version: 1,
                        }]
                    }
                }
                ControlMessage::Capabilities { .. } => vec![RuntimeMessage::Capabilities {
                    session_per_vm: true,
                    writable_workspace: true,
                }],
                ControlMessage::Cancel {
                    session_id,
                    request_id,
                } => self.cancel_active(session_id, request_id),
                ControlMessage::Shutdown => {
                    if let Some(flag) = &self.turn_cancel {
                        flag.store(true, Ordering::SeqCst);
                    }
                    self.shutdown = true;
                    vec![RuntimeMessage::ShutdownAck]
                }
                ControlMessage::StartTurn(_) => vec![RuntimeMessage::Error {
                    request_id: request_id(&request),
                    message: "session already has an active request".into(),
                }],
                _ => vec![RuntimeMessage::Error {
                    request_id: request_id(&request),
                    message: "turn in progress; control RPC unavailable".into(),
                }],
            };
        }
        match request {
            ControlMessage::Hello { protocol_version } => {
                if protocol_version != 1 {
                    self.protocol_ready = false;
                    vec![RuntimeMessage::Error {
                        request_id: String::new(),
                        message: format!("unsupported protocol version: {protocol_version}"),
                    }]
                } else {
                    self.protocol_ready = true;
                    vec![RuntimeMessage::HelloAck {
                        protocol_version: 1,
                    }]
                }
            }
            request if !self.protocol_ready => vec![RuntimeMessage::Error {
                request_id: request_id(&request),
                message: "protocol handshake required before control messages".into(),
            }],
            ControlMessage::Capabilities { .. } => vec![RuntimeMessage::Capabilities {
                session_per_vm: true,
                writable_workspace: true,
            }],
            ControlMessage::StartTurn(request) => self.handle_start(request),
            ControlMessage::Cancel {
                session_id,
                request_id,
            } => self.handle_cancel(session_id, request_id),
            ControlMessage::ReadWorkspaceFile(request) => {
                match self.executor_mut().read_workspace_file(&request) {
                    Ok(content) => vec![RuntimeMessage::FileContent {
                        request_id: request.request_id,
                        path: request.path,
                        content,
                    }],
                    Err(message) => vec![RuntimeMessage::Error {
                        request_id: request.request_id,
                        message,
                    }],
                }
            }
            ControlMessage::ReadHostFile(request) => vec![RuntimeMessage::Error {
                request_id: request.request_id,
                message: "read-only host source RPC is not supported".into(),
            }],
            ControlMessage::WriteWorkspaceFile(request) => {
                match self.executor_mut().write_workspace_file(&request) {
                    Ok(()) => vec![RuntimeMessage::WriteAck {
                        request_id: request.request_id,
                        path: request.path,
                    }],
                    Err(message) => vec![RuntimeMessage::Error {
                        request_id: request.request_id,
                        message,
                    }],
                }
            }
            ControlMessage::Shutdown => {
                // If a turn is running on a worker, ask it to stop.
                if let Some(flag) = &self.turn_cancel {
                    flag.store(true, Ordering::SeqCst);
                }
                let result = if let Some(executor) = self.executor.as_mut() {
                    executor.shutdown()
                } else {
                    Ok(())
                };
                self.active_sessions.clear();
                self.completed_requests.clear();
                self.completed_order.clear();
                self.evicted_requests.clear();
                self.cancelled_sessions.clear();
                self.cancelled_order.clear();
                self.shutdown = true;
                match result {
                    Ok(()) => vec![RuntimeMessage::ShutdownAck],
                    Err(message) => vec![RuntimeMessage::Error {
                        request_id: String::new(),
                        message,
                    }],
                }
            }
        }
    }

    /// Claim a turn for in-flight execution. Validates identity/activity, marks
    /// the session active, stores the executor's cancellation handle, and takes
    /// the executor out so the caller can run `start_turn` on a worker thread.
    /// Returns the executor on success, or the rejection response.
    /// Dispatch a typed filesystem operation to the active workspace executor.
    /// Fails closed while a turn has taken the executor out to a worker.
    pub fn filesystem_rpc(&mut self, op: u8, path: &str, data: &[u8]) -> Result<Vec<u8>, String> {
        if self.executor.is_none() {
            return Err("turn in progress; filesystem RPC unavailable".into());
        }
        self.executor_mut().filesystem_rpc(op, path, data)
    }

    /// Claim a session turn and return its executor for worker execution.
    ///
    /// The request identity is validated and duplicate/completed turns are
    /// rejected before the executor is removed from the service.
    pub fn spawn_turn(
        &mut self,
        turn: &SessionRequest,
    ) -> Result<Box<dyn GuestExecutor>, RuntimeMessage> {
        if let Some(error) = validate_identity(&turn.session_id, &turn.request_id) {
            return Err(error);
        }
        let key = (turn.session_id.clone(), turn.request_id.clone());
        if self.completed_requests.contains_key(&key) {
            return Err(RuntimeMessage::Error {
                request_id: turn.request_id.clone(),
                message: "request already completed".into(),
            });
        }
        if self.evicted_requests.contains(&key) {
            return Err(Self::evicted_replay_error(&turn.request_id));
        }
        if let Some(active) = self.active_sessions.get(&turn.session_id) {
            return Err(RuntimeMessage::Error {
                request_id: turn.request_id.clone(),
                message: format!("session already has an active request: {active}"),
            });
        }
        if !self.protocol_ready {
            return Err(RuntimeMessage::Error {
                request_id: turn.request_id.clone(),
                message: "protocol handshake required before control messages".into(),
            });
        }
        let executor = match self.executor.take() {
            Some(executor) => executor,
            None => {
                return Err(RuntimeMessage::Error {
                    request_id: turn.request_id.clone(),
                    message: "runtime executor is unavailable: a previous turn was lost and \
                              could not be restored; restart the runtime"
                        .into(),
                })
            }
        };
        let mut executor = executor;
        executor.reset_cancel();
        self.turn_cancel = executor.cancel_handle();
        self.active_sessions
            .insert(turn.session_id.clone(), turn.request_id.clone());
        Ok(executor)
    }

    /// Cancel an in-flight turn from the reader loop. Identity is validated
    /// against the active session; on success the shared cancellation flag is
    /// set and the worker emits the terminal `turn.cancelled` later. Returns
    /// the immediate responses (normally none) so a repeated cancel is
    /// idempotent.
    pub fn cancel_active(&mut self, session_id: &str, request_id: &str) -> Vec<RuntimeMessage> {
        if let Some(error) = validate_identity(session_id, request_id) {
            return vec![error];
        }
        let key = (session_id.to_string(), request_id.to_string());
        if self.cancelled_sessions.contains_key(&key) {
            return Vec::new();
        }
        match self.active_sessions.get(session_id) {
            Some(active) if active != request_id => {
                return vec![RuntimeMessage::Error {
                    request_id: request_id.to_string(),
                    message: format!("session has a different active request: {active}"),
                }]
            }
            None => {
                return vec![RuntimeMessage::Error {
                    request_id: request_id.to_string(),
                    message: "request is not active".into(),
                }]
            }
            _ => {}
        }
        if let Some(flag) = &self.turn_cancel {
            flag.store(true, Ordering::SeqCst);
        }
        self.record_cancelled(key, Vec::new());
        Vec::new()
    }

    /// Clear the in-flight turn when its worker died without delivering a
    /// result (panic): the executor cannot be restored, so subsequent turn
    /// claims fail with an actionable restart message instead of a phantom
    /// "session already has an active request".
    pub fn abandon_active_turn(&mut self) -> Vec<RuntimeMessage> {
        let abandoned: Vec<RuntimeMessage> = self
            .active_sessions
            .iter()
            .map(|(session_id, request_id)| RuntimeMessage::Error {
                request_id: request_id.clone(),
                message: format!("turn worker died without a result (session {session_id})"),
            })
            .collect();
        self.active_sessions.clear();
        self.turn_cancel = None;
        abandoned
    }

    /// Restore the executor and produce the terminal responses for a turn that
    /// ran on a worker thread. Assigns sequences, records the completed or
    /// cancelled result, and clears the active session.
    pub fn complete_turn(
        &mut self,
        session_id: String,
        request_id: String,
        result: Result<Vec<GuestEvent>, String>,
        executor: Box<dyn GuestExecutor>,
    ) -> Vec<RuntimeMessage> {
        self.executor = Some(executor);
        self.turn_cancel = None;
        let key = (session_id.clone(), request_id.clone());
        let cancelled = self.cancelled_sessions.contains_key(&key);
        let responses = match result {
            Ok(events) => events
                .into_iter()
                .map(|event| {
                    RuntimeMessage::Event(SessionEvent {
                        session_id: session_id.clone(),
                        sequence: self.next(),
                        kind: event.kind,
                        payload: event.payload,
                    })
                })
                .collect(),
            Err(message) if message == "request cancelled" || cancelled => {
                vec![RuntimeMessage::Event(SessionEvent {
                    session_id: session_id.clone(),
                    sequence: self.next(),
                    kind: "turn.cancelled".into(),
                    payload: Vec::new(),
                })]
            }
            Err(message) => vec![RuntimeMessage::Error {
                request_id,
                message,
            }],
        };
        let finished = responses.iter().any(response_completes_turn);
        let failed = responses
            .iter()
            .any(|r| matches!(r, RuntimeMessage::Error { .. }));
        if finished {
            self.active_sessions.remove(&session_id);
        }
        if cancelled {
            self.record_cancelled(key, responses.clone());
        } else if !failed && finished {
            self.record_completed(key, responses.clone());
        }
        responses
    }

    fn handle_start(&mut self, request: SessionRequest) -> Vec<RuntimeMessage> {
        if let Some(error) = validate_identity(&request.session_id, &request.request_id) {
            return vec![error];
        }
        let key = (request.session_id.clone(), request.request_id.clone());
        if let Some(previous) = self.completed_requests.get(&key) {
            return previous.clone();
        }
        if self.evicted_requests.contains(&key) {
            return vec![Self::evicted_replay_error(&request.request_id)];
        }
        if let Some(active) = self.active_sessions.get(&request.session_id) {
            return vec![RuntimeMessage::Error {
                request_id: request.request_id,
                message: format!("session already has an active request: {active}"),
            }];
        }
        self.active_sessions
            .insert(request.session_id.clone(), request.request_id.clone());
        let session_id = request.session_id.clone();
        let request_id = request.request_id.clone();
        self.executor_mut().reset_cancel();
        let result = self.executor_mut().start_turn(&request);
        let responses = match result {
            Ok(events) => events
                .into_iter()
                .map(|event| {
                    RuntimeMessage::Event(SessionEvent {
                        session_id: session_id.clone(),
                        sequence: self.next(),
                        kind: event.kind,
                        payload: event.payload,
                    })
                })
                .collect(),
            Err(message) => vec![RuntimeMessage::Error {
                request_id: request_id.clone(),
                message,
            }],
        };
        let failed = responses
            .iter()
            .any(|r| matches!(r, RuntimeMessage::Error { .. }));
        let finished = responses.iter().any(response_completes_turn);
        if failed || finished {
            self.active_sessions.remove(&session_id);
        }
        if !failed && finished {
            self.record_completed(key, responses.clone());
        }
        responses
    }

    fn handle_cancel(&mut self, session_id: String, request_id: String) -> Vec<RuntimeMessage> {
        if let Some(error) = validate_identity(&session_id, &request_id) {
            return vec![error];
        }
        let key = (session_id.clone(), request_id.clone());
        if let Some(previous) = self.cancelled_sessions.get(&key) {
            return previous.clone();
        }
        match self.active_sessions.get(&session_id) {
            Some(active) if active != &request_id => {
                return vec![RuntimeMessage::Error {
                    request_id,
                    message: format!("session has a different active request: {active}"),
                }]
            }
            None => {
                return vec![RuntimeMessage::Error {
                    request_id,
                    message: "request is not active".into(),
                }]
            }
            _ => {}
        }
        if let Err(message) = self.executor_mut().cancel(&session_id, &request_id) {
            return vec![RuntimeMessage::Error {
                request_id,
                message,
            }];
        }
        self.active_sessions.remove(&session_id);
        let responses = vec![RuntimeMessage::Event(SessionEvent {
            session_id: session_id.clone(),
            sequence: self.next(),
            kind: "turn.cancelled".into(),
            payload: Vec::new(),
        })];
        self.record_cancelled(key, responses.clone());
        responses
    }

    fn next(&mut self) -> u64 {
        self.sequence = self.sequence.saturating_add(1);
        self.sequence
    }
}
