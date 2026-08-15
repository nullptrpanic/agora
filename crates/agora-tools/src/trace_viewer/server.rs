use crate::trace_viewer::audit::{CursorItem, LogCursor, TraceEvent};
use crate::trace_viewer::protocol::{
    AccessToken, ClientControl, ServerControl, SessionStatus, parse_control, validate_auth,
};
use crate::trace_viewer::terminal::{TerminalEvent, TerminalSession, TerminalSize, TerminalSpec};
use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

const TERMINAL_REPLAY_LIMIT: usize = 1024 * 1024;
const TRACE_LIMIT: usize = 5_000;
const DIAGNOSTIC_LIMIT: usize = 100;
const HUB_CHANNEL_CAPACITY: usize = 256;
const TAIL_INTERVAL: Duration = Duration::from_millis(100);
const AUTH_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_WEBSOCKET_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(super) enum HubEvent {
    Terminal(Vec<u8>),
    Trace(TraceEvent),
    Status {
        status: SessionStatus,
        exit_code: Option<i32>,
        message: Option<String>,
    },
    Diagnostic(String),
    TraceCleared,
}

#[derive(Clone, Debug)]
pub(super) struct HubSnapshot {
    pub(super) terminal_replay: Vec<u8>,
    pub(super) terminal_truncated: bool,
    pub(super) traces: Vec<TraceEvent>,
    pub(super) trace_truncated: bool,
    pub(super) diagnostics: Vec<String>,
    pub(super) active_root_trace_id: Option<String>,
    pub(super) status: SessionStatus,
    pub(super) exit_code: Option<i32>,
    pub(super) status_message: Option<String>,
}

#[derive(Clone)]
pub(super) struct EventHub {
    inner: Arc<EventHubInner>,
}

struct EventHubInner {
    state: Mutex<HubState>,
    sender: broadcast::Sender<HubEvent>,
    terminal_limit: usize,
    trace_limit: usize,
    diagnostic_limit: usize,
}

struct HubState {
    terminal_replay: VecDeque<u8>,
    terminal_truncated: bool,
    traces: VecDeque<TraceEvent>,
    trace_truncated: bool,
    diagnostics: VecDeque<String>,
    active_root_trace_id: Option<String>,
    status: SessionStatus,
    exit_code: Option<i32>,
    status_message: Option<String>,
    next_trace_id: u64,
}

impl EventHub {
    pub(super) fn new() -> Self {
        Self::with_limits(TERMINAL_REPLAY_LIMIT, TRACE_LIMIT, DIAGNOSTIC_LIMIT)
    }

    fn with_limits(terminal_limit: usize, trace_limit: usize, diagnostic_limit: usize) -> Self {
        let (sender, _) = broadcast::channel(HUB_CHANNEL_CAPACITY);
        Self {
            inner: Arc::new(EventHubInner {
                state: Mutex::new(HubState {
                    terminal_replay: VecDeque::new(),
                    terminal_truncated: false,
                    traces: VecDeque::new(),
                    trace_truncated: false,
                    diagnostics: VecDeque::new(),
                    active_root_trace_id: None,
                    status: SessionStatus::Idle,
                    exit_code: None,
                    status_message: None,
                    next_trace_id: 1,
                }),
                sender,
                terminal_limit,
                trace_limit,
                diagnostic_limit,
            }),
        }
    }

    pub(super) fn begin_session(&self) {
        let mut state = self.lock_state();
        state.terminal_replay.clear();
        state.terminal_truncated = false;
        state.active_root_trace_id = None;
        state.status = SessionStatus::Starting;
        state.exit_code = None;
        state.status_message = None;
        let _ = self.inner.sender.send(HubEvent::Status {
            status: SessionStatus::Starting,
            exit_code: None,
            message: None,
        });
    }

    pub(super) fn push_terminal(&self, bytes: &[u8]) {
        let mut state = self.lock_state();
        state.terminal_replay.extend(bytes.iter().copied());
        while state.terminal_replay.len() > self.inner.terminal_limit {
            state.terminal_replay.pop_front();
            state.terminal_truncated = true;
        }
        let _ = self.inner.sender.send(HubEvent::Terminal(bytes.to_vec()));
    }

    pub(super) fn push_trace(&self, mut event: TraceEvent) {
        let mut state = self.lock_state();
        event.id = state.next_trace_id;
        state.next_trace_id = state.next_trace_id.saturating_add(1);
        if state.active_root_trace_id.is_none() {
            state.active_root_trace_id = Some(event.root_trace_id.clone());
        }
        state.traces.push_back(event.clone());
        while state.traces.len() > self.inner.trace_limit {
            state.traces.pop_front();
            state.trace_truncated = true;
        }
        let _ = self.inner.sender.send(HubEvent::Trace(event));
    }

    pub(super) fn push_diagnostic(&self, message: String) {
        let mut state = self.lock_state();
        state.diagnostics.push_back(message.clone());
        while state.diagnostics.len() > self.inner.diagnostic_limit {
            state.diagnostics.pop_front();
        }
        let _ = self.inner.sender.send(HubEvent::Diagnostic(message));
    }

    pub(super) fn set_status(
        &self,
        status: SessionStatus,
        exit_code: Option<i32>,
        message: Option<String>,
    ) {
        let mut state = self.lock_state();
        state.status = status;
        state.exit_code = exit_code;
        state.status_message.clone_from(&message);
        let _ = self.inner.sender.send(HubEvent::Status {
            status,
            exit_code,
            message,
        });
    }

    pub(super) fn clear_trace(&self) {
        let mut state = self.lock_state();
        state.traces.clear();
        state.trace_truncated = false;
        state.diagnostics.clear();
        state.active_root_trace_id = None;
        let _ = self.inner.sender.send(HubEvent::TraceCleared);
    }

    pub(super) fn snapshot(&self) -> HubSnapshot {
        let state = self.lock_state();
        Self::snapshot_from(&state)
    }

    fn subscribe_with_snapshot(&self) -> (broadcast::Receiver<HubEvent>, HubSnapshot) {
        let state = self.lock_state();
        let receiver = self.inner.sender.subscribe();
        let snapshot = Self::snapshot_from(&state);
        (receiver, snapshot)
    }

    fn snapshot_from(state: &HubState) -> HubSnapshot {
        HubSnapshot {
            terminal_replay: state.terminal_replay.iter().copied().collect(),
            terminal_truncated: state.terminal_truncated,
            traces: state.traces.iter().cloned().collect(),
            trace_truncated: state.trace_truncated,
            diagnostics: state.diagnostics.iter().cloned().collect(),
            active_root_trace_id: state.active_root_trace_id.clone(),
            status: state.status,
            exit_code: state.exit_code,
            status_message: state.status_message.clone(),
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, HubState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct TailWorker {
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TailWorker {
    fn spawn(mut cursor: LogCursor, hub: EventHub) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("agora-trace-log-tail".to_string())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match cursor.poll() {
                        Ok(items) => {
                            for item in items {
                                match item {
                                    CursorItem::Event(event) => hub.push_trace(event),
                                    CursorItem::Diagnostic(message) => hub.push_diagnostic(message),
                                }
                            }
                        }
                        Err(error) => {
                            hub.push_diagnostic(format!("audit log tail failed: {error}"))
                        }
                    }
                    thread::sleep(TAIL_INTERVAL);
                }
            })?;
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for TailWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SessionState {
    terminal: Option<TerminalSession>,
    tail: Option<TailWorker>,
}

pub(super) struct SessionManager {
    terminal_spec: TerminalSpec,
    log_path: PathBuf,
    hub: EventHub,
    state: Mutex<SessionState>,
}

impl SessionManager {
    pub(super) fn new(terminal_spec: TerminalSpec, log_path: PathBuf, hub: EventHub) -> Self {
        Self {
            terminal_spec,
            log_path,
            hub,
            state: Mutex::new(SessionState {
                terminal: None,
                tail: None,
            }),
        }
    }

    pub(super) fn start(&self, size: TerminalSize) -> Result<()> {
        let mut state = self.lock_state();
        if state
            .terminal
            .as_ref()
            .is_some_and(|terminal| !terminal.is_exited())
        {
            return Ok(());
        }
        if let Some(mut tail) = state.tail.take() {
            tail.stop();
        }
        let cursor = LogCursor::at_end(self.log_path.clone())
            .with_context(|| format!("failed to open audit log {}", self.log_path.display()))?;
        self.hub.begin_session();
        let (terminal, mut events) = TerminalSession::spawn(self.terminal_spec.clone(), size)?;
        let event_hub = self.hub.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                match event {
                    TerminalEvent::Output(bytes) => event_hub.push_terminal(&bytes),
                    TerminalEvent::Exited { exit_code, signal } => {
                        event_hub.set_status(SessionStatus::Exited, exit_code, signal)
                    }
                    TerminalEvent::Error(message) => {
                        event_hub.push_diagnostic(message.clone());
                        event_hub.set_status(SessionStatus::Error, None, Some(message));
                    }
                }
            }
        });
        let tail = TailWorker::spawn(cursor, self.hub.clone())?;
        state.terminal = Some(terminal);
        state.tail = Some(tail);
        drop(state);
        self.hub.set_status(SessionStatus::Running, None, None);
        Ok(())
    }

    pub(super) fn input(&self, bytes: &[u8]) -> io::Result<()> {
        self.with_terminal(|terminal| terminal.input(bytes))
    }

    pub(super) fn resize(&self, size: TerminalSize) -> io::Result<()> {
        self.with_terminal(|terminal| terminal.resize(size))
    }

    pub(super) fn stop(&self) -> io::Result<()> {
        self.with_terminal(TerminalSession::request_stop)
    }

    pub(super) fn shutdown(&self) -> io::Result<()> {
        let mut state = self.lock_state();
        let terminal_result = state
            .terminal
            .take()
            .map_or(Ok(()), |terminal| terminal.stop_and_wait());
        if let Some(mut tail) = state.tail.take() {
            tail.stop();
        }
        terminal_result
    }

    fn with_terminal<T>(
        &self,
        operation: impl FnOnce(&TerminalSession) -> io::Result<T>,
    ) -> io::Result<T> {
        let state = self.lock_state();
        let terminal = state.terminal.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotConnected, "terminal is not running")
        })?;
        operation(terminal)
    }

    fn lock_state(&self) -> MutexGuard<'_, SessionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for SessionManager {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(terminal) = state.terminal.take() {
                let _ = terminal.stop_and_wait();
            }
            if let Some(mut tail) = state.tail.take() {
                tail.stop();
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct AccessGuard {
    pub(super) expected_host: String,
    pub(super) expected_origin: String,
}

pub(super) fn validate_upgrade(headers: &HeaderMap, guard: &AccessGuard) -> Result<(), StatusCode> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if host != Some(guard.expected_host.as_str()) || origin != Some(guard.expected_origin.as_str())
    {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct AppState {
    guard: AccessGuard,
    token: AccessToken,
    manager: Arc<SessionManager>,
    hub: EventHub,
    controller: Arc<AtomicBool>,
    shutdown: broadcast::Sender<()>,
}

impl AppState {
    pub(super) fn new(
        guard: AccessGuard,
        token: AccessToken,
        manager: Arc<SessionManager>,
        hub: EventHub,
    ) -> Self {
        let (shutdown, _) = broadcast::channel(1);
        Self {
            guard,
            token,
            manager,
            hub,
            controller: Arc::new(AtomicBool::new(false)),
            shutdown,
        }
    }

    pub(super) fn shutdown_clients(&self) {
        let _ = self.shutdown.send(());
    }

    #[cfg(test)]
    fn controller_active(&self) -> bool {
        self.controller.load(Ordering::Acquire)
    }
}

pub(super) async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
        .context("trace viewer HTTP server failed")
}

fn router(state: AppState) -> Router {
    Router::new()
        .merge(crate::trace_viewer::assets::routes())
        .route("/ws", get(websocket_upgrade))
        .with_state(state)
}

async fn websocket_upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    websocket: WebSocketUpgrade,
) -> Result<Response, StatusCode> {
    validate_upgrade(&headers, &state.guard)?;
    Ok(websocket
        .max_message_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .max_frame_size(MAX_WEBSOCKET_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state))
        .into_response())
}

struct ControllerLease(Arc<AtomicBool>);

impl Drop for ControllerLease {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let authentication = tokio::time::timeout(AUTH_TIMEOUT, socket.recv()).await;
    let authenticated = matches!(
        authentication,
        Ok(Some(Ok(ref message))) if validate_auth(message, &state.token).is_ok()
    );
    if !authenticated {
        close_socket(&mut socket, "authentication required").await;
        return;
    }
    if state
        .controller
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        close_socket(&mut socket, "another viewer is already connected").await;
        return;
    }
    let _lease = ControllerLease(state.controller.clone());

    if state.hub.snapshot().status == SessionStatus::Idle
        && let Err(error) = state.manager.start(TerminalSize::default())
    {
        let message = format!("failed to start sandbox terminal: {error:#}");
        state.hub.push_diagnostic(message.clone());
        state
            .hub
            .set_status(SessionStatus::Error, None, Some(message));
    }

    let (mut events, snapshot) = state.hub.subscribe_with_snapshot();
    if send_replay_socket(&mut socket, &snapshot).await.is_err() {
        return;
    }
    let (mut sender, mut receiver) = socket.split();
    let mut terminal_size = TerminalSize::default();
    let mut shutdown = state.shutdown.subscribe();

    loop {
        tokio::select! {
            incoming = receiver.next() => {
                let Some(Ok(message)) = incoming else {
                    break;
                };
                match message {
                    Message::Binary(bytes) => {
                        if let Err(error) = state.manager.input(&bytes)
                            && send_diagnostic(&mut sender, format!("terminal input failed: {error}")).await.is_err()
                        {
                            break;
                        }
                    }
                    Message::Text(text) => {
                        let result = handle_control(&state, &mut terminal_size, &text);
                        if let Err(error) = result
                            && send_diagnostic(&mut sender, error).await.is_err()
                        {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(_) | Message::Pong(_) => {}
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        if send_hub_event(&mut sender, event).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if send_replay_sink(&mut sender, &state.hub.snapshot()).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = shutdown.recv() => {
                let _ = sender.send(Message::Close(Some(CloseFrame {
                    code: 1001,
                    reason: "viewer shutting down".into(),
                }))).await;
                break;
            }
        }
    }
}

fn handle_control(
    state: &AppState,
    terminal_size: &mut TerminalSize,
    text: &str,
) -> std::result::Result<(), String> {
    match parse_control(text).map_err(|error| error.to_string())? {
        ClientControl::Auth { .. } => {
            Err("authentication is only accepted as the first message".to_string())
        }
        ClientControl::Resize { cols, rows } => {
            *terminal_size = TerminalSize { cols, rows };
            state
                .manager
                .resize(*terminal_size)
                .map_err(|error| format!("terminal resize failed: {error}"))
        }
        ClientControl::Stop => state
            .manager
            .stop()
            .map_err(|error| format!("terminal stop failed: {error}")),
        ClientControl::Start => state.manager.start(*terminal_size).map_err(|error| {
            let message = format!("failed to start sandbox terminal: {error:#}");
            state
                .hub
                .set_status(SessionStatus::Error, None, Some(message.clone()));
            message
        }),
        ClientControl::ClearTrace => {
            state.hub.clear_trace();
            Ok(())
        }
    }
}

async fn close_socket(socket: &mut WebSocket, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code: 1008,
            reason: reason.into(),
        })))
        .await;
}

type SocketSink = futures_util::stream::SplitSink<WebSocket, Message>;

async fn send_diagnostic(sender: &mut SocketSink, message: String) -> Result<()> {
    send_control(sender, ServerControl::Diagnostic { message }).await
}

async fn send_hub_event(sender: &mut SocketSink, event: HubEvent) -> Result<()> {
    match event {
        HubEvent::Terminal(bytes) => sender
            .send(Message::Binary(bytes.into()))
            .await
            .context("failed to send terminal output"),
        HubEvent::Trace(event) => send_control(sender, ServerControl::Trace { event }).await,
        HubEvent::Status {
            status,
            exit_code,
            message,
        } => {
            send_control(
                sender,
                ServerControl::Status {
                    status,
                    exit_code,
                    message,
                },
            )
            .await
        }
        HubEvent::Diagnostic(message) => {
            send_control(sender, ServerControl::Diagnostic { message }).await
        }
        HubEvent::TraceCleared => send_control(sender, ServerControl::TraceCleared).await,
    }
}

async fn send_replay_socket(socket: &mut WebSocket, snapshot: &HubSnapshot) -> Result<()> {
    socket
        .send(control_message(ServerControl::ReplayStart {
            truncated: snapshot.terminal_truncated,
        })?)
        .await
        .context("failed to begin terminal replay")?;
    if !snapshot.terminal_replay.is_empty() {
        socket
            .send(Message::Binary(snapshot.terminal_replay.clone().into()))
            .await
            .context("failed to send terminal replay")?;
    }
    socket
        .send(control_message(ServerControl::ReplayEnd)?)
        .await
        .context("failed to finish terminal replay")?;
    socket
        .send(control_message(snapshot_control(snapshot))?)
        .await
        .context("failed to send trace snapshot")
}

async fn send_replay_sink(sender: &mut SocketSink, snapshot: &HubSnapshot) -> Result<()> {
    send_control(
        sender,
        ServerControl::ReplayStart {
            truncated: snapshot.terminal_truncated,
        },
    )
    .await?;
    if !snapshot.terminal_replay.is_empty() {
        sender
            .send(Message::Binary(snapshot.terminal_replay.clone().into()))
            .await
            .context("failed to send terminal replay")?;
    }
    send_control(sender, ServerControl::ReplayEnd).await?;
    send_control(sender, snapshot_control(snapshot)).await
}

fn snapshot_control(snapshot: &HubSnapshot) -> ServerControl {
    ServerControl::Snapshot {
        traces: snapshot.traces.clone(),
        diagnostics: snapshot.diagnostics.clone(),
        active_root_trace_id: snapshot.active_root_trace_id.clone(),
        terminal_truncated: snapshot.terminal_truncated,
        trace_truncated: snapshot.trace_truncated,
        status: snapshot.status,
        exit_code: snapshot.exit_code,
        message: snapshot.status_message.clone(),
    }
}

async fn send_control(sender: &mut SocketSink, control: ServerControl) -> Result<()> {
    sender
        .send(control_message(control)?)
        .await
        .context("failed to send viewer control message")
}

fn control_message(control: ServerControl) -> Result<Message> {
    Ok(Message::Text(serde_json::to_string(&control)?.into()))
}

#[cfg(test)]
mod access_tests {
    use super::{AccessGuard, validate_upgrade};
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};

    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_str(host).unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, HeaderValue::from_str(origin).unwrap());
        }
        headers
    }

    fn guard() -> AccessGuard {
        AccessGuard {
            expected_host: "127.0.0.1:43123".to_string(),
            expected_origin: "http://127.0.0.1:43123".to_string(),
        }
    }

    #[test]
    fn websocket_upgrade_requires_exact_host_and_same_origin() {
        assert!(
            validate_upgrade(
                &headers("127.0.0.1:43123", Some("http://127.0.0.1:43123")),
                &guard()
            )
            .is_ok()
        );

        assert_eq!(
            validate_upgrade(
                &headers("localhost:43123", Some("http://127.0.0.1:43123")),
                &guard()
            ),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            validate_upgrade(
                &headers("127.0.0.1:43123", Some("https://evil.example")),
                &guard()
            ),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            validate_upgrade(&headers("127.0.0.1:43123", None), &guard()),
            Err(StatusCode::FORBIDDEN)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{EventHub, SessionManager};
    use crate::trace_viewer::audit::{TraceEvent, TraceKind};
    use crate::trace_viewer::protocol::SessionStatus;
    use crate::trace_viewer::terminal::{TerminalSize, TerminalSpec};
    use serde_json::json;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Duration;

    fn event(id: u64, root: &str, title: &str) -> TraceEvent {
        TraceEvent {
            id,
            root_trace_id: root.to_string(),
            kind: TraceKind::Exec,
            occurred_at: format!("t{id}"),
            title: title.to_string(),
            detail: json!({ "id": id }),
        }
    }

    #[test]
    fn hub_bounds_replay_trace_and_diagnostics_without_changing_active_root() {
        let hub = EventHub::with_limits(5, 2, 2);
        hub.push_terminal(b"abc");
        hub.push_terminal(b"def");
        hub.push_trace(event(1, "root-a", "one"));
        hub.push_trace(event(2, "root-b", "two"));
        hub.push_trace(event(3, "root-c", "three"));
        hub.push_diagnostic("first".to_string());
        hub.push_diagnostic("second".to_string());
        hub.push_diagnostic("third".to_string());

        let snapshot = hub.snapshot();

        assert_eq!(snapshot.terminal_replay, b"bcdef");
        assert!(snapshot.terminal_truncated);
        assert_eq!(
            snapshot
                .traces
                .iter()
                .map(|event| event.title.as_str())
                .collect::<Vec<_>>(),
            ["two", "three"]
        );
        assert!(snapshot.trace_truncated);
        assert_eq!(snapshot.active_root_trace_id.as_deref(), Some("root-a"));
        assert_eq!(snapshot.diagnostics, ["second", "third"]);
    }

    #[test]
    fn clear_trace_does_not_clear_terminal_replay_or_status() {
        let hub = EventHub::with_limits(64, 4, 4);
        hub.push_terminal(b"terminal");
        hub.push_trace(event(1, "root", "one"));
        hub.set_status(SessionStatus::Running, None, None);

        hub.clear_trace();
        let snapshot = hub.snapshot();

        assert_eq!(snapshot.terminal_replay, b"terminal");
        assert!(snapshot.traces.is_empty());
        assert_eq!(snapshot.active_root_trace_id, None);
        assert_eq!(snapshot.status, SessionStatus::Running);
    }

    #[test]
    fn hub_assigns_stable_event_ids_across_restarted_log_cursors() {
        let hub = EventHub::with_limits(64, 10, 4);
        hub.push_trace(event(1, "first-root", "one"));
        hub.begin_session();
        hub.push_trace(event(1, "second-root", "two"));

        assert_eq!(
            hub.snapshot()
                .traces
                .iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
    }

    fn fake_sandbox(path: &std::path::Path) {
        fs::write(
            path,
            r#"#!/bin/sh
exec /bin/bash --noprofile --norc
"#,
        )
        .unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[tokio::test]
    async fn session_streams_terminal_and_appended_audit_records_into_one_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = root.path().join("fake-sandbox");
        let config = root.path().join("sandbox.json");
        let log = root.path().join("sandbox.log");
        fake_sandbox(&sandbox);
        fs::write(&config, "{}").unwrap();
        fs::write(&log, b"historical\n").unwrap();
        let hub = EventHub::with_limits(64 * 1024, 100, 20);
        let manager = SessionManager::new(
            TerminalSpec {
                sandbox_binary: sandbox,
                config_path: config,
                shell: PathBuf::from("/bin/bash"),
            },
            log.clone(),
            hub.clone(),
        );

        manager.start(TerminalSize::default()).unwrap();
        manager.input(b"printf 'SESSION_READY\\n'\r").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !String::from_utf8_lossy(&hub.snapshot().terminal_replay)
                .contains("SESSION_READY")
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        let mut file = OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(
            file,
            "{}",
            json!({
                "audit": {
                    "type": "network",
                    "access_time": "now",
                    "trace_id": "session-root, child",
                    "pid": 10,
                    "destination_ip": "127.0.0.1",
                    "destination_port": 8080,
                    "domain": "local.example"
                }
            })
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while hub.snapshot().traces.is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();

        let snapshot = hub.snapshot();
        assert_eq!(snapshot.traces.len(), 1);
        assert_eq!(snapshot.traces[0].title, "local.example:8080");
        assert_eq!(
            snapshot.active_root_trace_id.as_deref(),
            Some("session-root")
        );
        assert!(
            snapshot
                .diagnostics
                .iter()
                .all(|message| !message.contains("historical"))
        );
        manager.stop().unwrap();
        manager.shutdown().unwrap();
    }
}

#[cfg(test)]
mod websocket_tests {
    use super::{AccessGuard, AppState, EventHub, SessionManager, serve};
    use crate::trace_viewer::protocol::{AccessToken, SessionStatus};
    use crate::trace_viewer::terminal::TerminalSpec;
    use futures_util::{SinkExt, StreamExt};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

    type Client = WebSocketStream<MaybeTlsStream<TcpStream>>;

    fn fake_sandbox(path: &Path, marker: &Path) {
        fs::write(
            path,
            format!(
                "#!/bin/sh\nprintf 'spawn\\n' >> '{}'\nexec /bin/bash --noprofile --norc\n",
                marker.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    async fn connect(address: std::net::SocketAddr) -> Client {
        let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(&format!("http://{address}")).unwrap(),
        );
        connect_async(request).await.unwrap().0
    }

    async fn authenticate(client: &mut Client, token: &str) {
        client
            .send(Message::Text(
                format!(r#"{{"type":"auth","token":"{token}"}}"#).into(),
            ))
            .await
            .unwrap();
    }

    async fn read_binary_until(client: &mut Client, expected: &str) -> Vec<u8> {
        let mut output = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !String::from_utf8_lossy(&output).contains(expected) {
                match client.next().await.unwrap().unwrap() {
                    Message::Binary(bytes) => output.extend_from_slice(&bytes),
                    Message::Close(frame) => panic!("unexpected close: {frame:?}"),
                    _ => {}
                }
            }
        })
        .await
        .unwrap();
        output
    }

    #[tokio::test]
    async fn auth_owns_one_controller_and_reconnect_receives_terminal_replay() {
        let root = tempfile::tempdir().unwrap();
        let sandbox = root.path().join("fake-sandbox");
        let marker = root.path().join("spawns");
        let config = root.path().join("sandbox.json");
        let log = root.path().join("sandbox.log");
        fake_sandbox(&sandbox, &marker);
        fs::write(&config, "{}").unwrap();
        fs::write(&log, "").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let token = AccessToken::generate();
        let hub = EventHub::new();
        let manager = Arc::new(SessionManager::new(
            TerminalSpec {
                sandbox_binary: sandbox,
                config_path: config,
                shell: PathBuf::from("/bin/bash"),
            },
            log,
            hub.clone(),
        ));
        let state = AppState::new(
            AccessGuard {
                expected_host: address.to_string(),
                expected_origin: format!("http://{address}"),
            },
            token.clone(),
            manager.clone(),
            hub.clone(),
        );
        let observed_state = state.clone();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve(listener, state, async {
            let _ = shutdown_rx.await;
        }));

        let mut invalid = connect(address).await;
        authenticate(&mut invalid, "wrong-token").await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(message) = invalid.next().await {
                if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(!marker.exists());
        assert_eq!(hub.snapshot().status, SessionStatus::Idle);

        let mut primary = connect(address).await;
        authenticate(&mut primary, token.as_str()).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        primary
            .send(Message::Binary(
                b"printf 'REPLAY_TOKEN\\n'\r".to_vec().into(),
            ))
            .await
            .unwrap();
        read_binary_until(&mut primary, "REPLAY_TOKEN").await;

        let mut second = connect(address).await;
        authenticate(&mut second, token.as_str()).await;
        let second_result = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match second.next().await {
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    _ => {}
                }
            }
        })
        .await;
        assert!(second_result.is_ok());
        assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);

        primary.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while observed_state.controller_active() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();

        let mut reconnected = connect(address).await;
        authenticate(&mut reconnected, token.as_str()).await;
        read_binary_until(&mut reconnected, "REPLAY_TOKEN").await;
        assert_eq!(fs::read_to_string(&marker).unwrap().lines().count(), 1);
        reconnected
            .send(Message::Text(r#"{"type":"stop"}"#.into()))
            .await
            .unwrap();
        reconnected.close(None).await.unwrap();

        manager.shutdown().unwrap();
        let _ = shutdown_tx.send(());
        server.await.unwrap().unwrap();
    }
}
