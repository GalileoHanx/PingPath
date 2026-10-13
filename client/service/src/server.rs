//! Serves the UI over the IPC channel (src/ipc.rs in the client core):
//! one task per connection reads request lines, answers each and forwards
//! the engine's events. The transport (the named pipe on Windows, a Unix
//! socket on Linux) is in the platform modules; this part is the same
//! everywhere, so it is tested on Linux.
//!
//! The pipe is open to every signed-in user (docs/decisions/007), so the
//! service protects users from each other:
//!
//! - an optimized session belongs to the user who turned it on; only they
//!   can turn it off, change the game or dismiss its notices, until it
//!   ends or they sign out;
//! - players can change only verbose logging and the improvement
//!   thresholds; the control plane URL and extra game definitions come
//!   from the admin-only settings file;
//! - each user may hold a few connections, each connection a few requests
//!   in flight, lines are capped at [`MAX_LINE`] and requests are rate
//!   limited, so one user can't starve the service.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pingpath_client_core::config::ClientConfig;
use pingpath_client_core::engine::{EngineEvent, EngineHandle, Mode, Status};
use pingpath_client_core::ipc::{
    self, ErrorBody, GameInfo, LogPaths, MAX_LINE, Message, PROTOCOL_VERSION, Request,
    SettingsChange, SettingsView, Welcome,
};
use pingpath_diagnostics::LogLevel;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::time::Instant;

/// Who is on the other end of a connection: the Windows logon session
/// (each signed-in user has their own) or the Unix user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Caller {
    WindowsSession(u32),
    Uid(u32),
}

impl fmt::Display for Caller {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Caller::WindowsSession(id) => write!(f, "session {id}"),
            Caller::Uid(uid) => write!(f, "uid {uid}"),
        }
    }
}

/// Resource limits for connections.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Connections open at once, all users together.
    pub connections: usize,
    /// Connections one user may hold (the UI needs one).
    pub per_user: usize,
    /// Slow requests (measure, enable, disable) in flight per connection.
    pub in_flight: usize,
    /// Requests per second per connection, on average ...
    pub rate: f64,
    /// ... with bursts up to this many.
    pub burst: f64,
    /// The first request must be `hello`, within this time.
    pub hello_timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connections: 16,
            per_user: 4,
            in_flight: 4,
            rate: 20.0,
            burst: 40.0,
            hello_timeout: Duration::from_secs(10),
        }
    }
}

/// Responses and events waiting to be written to one connection. Events
/// are dropped when it is full (a UI that doesn't read loses live samples,
/// not the service's memory).
const OUTBOX: usize = 256;
/// A connection that keeps exceeding the rate limit is closed.
const MAX_STRIKES: u32 = 200;

/// What the service shares between connections.
pub struct Service {
    engine: EngineHandle,
    games: Vec<GameInfo>,
    settings: tokio::sync::Mutex<ClientConfig>,
    config_path: PathBuf,
    log_level: LogLevel,
    log_dir: PathBuf,
    owner: Mutex<Option<Caller>>,
    connections: Arc<Semaphore>,
    per_user: Mutex<HashMap<Caller, usize>>,
    limits: Limits,
}

/// Everything [`Service::new`] needs.
pub struct ServiceParts {
    pub engine: EngineHandle,
    pub games: Vec<GameInfo>,
    pub settings: ClientConfig,
    pub config_path: PathBuf,
    pub log_level: LogLevel,
    pub log_dir: PathBuf,
    pub limits: Limits,
}

/// A connection slot, released when dropped.
pub struct Slot {
    service: Arc<Service>,
    caller: Caller,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut counts = self
            .service
            .per_user
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counts.get_mut(&self.caller) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.caller);
            }
        }
    }
}

fn active(mode: Mode) -> bool {
    matches!(mode, Mode::Connecting | Mode::Optimized | Mode::FailingOver)
}

/// Whether `caller` may measure, turn Optimize on or off: anyone while no
/// session runs, only its owner while one does.
fn may_control(mode: Mode, owner: Option<Caller>, caller: Caller) -> bool {
    !active(mode) || owner.is_none_or(|owner| owner == caller)
}

impl Service {
    pub fn new(parts: ServiceParts) -> Arc<Self> {
        Arc::new(Self {
            engine: parts.engine,
            games: parts.games,
            settings: tokio::sync::Mutex::new(parts.settings),
            config_path: parts.config_path,
            log_level: parts.log_level,
            log_dir: parts.log_dir,
            owner: Mutex::new(None),
            connections: Arc::new(Semaphore::new(parts.limits.connections)),
            per_user: Mutex::new(HashMap::new()),
            limits: parts.limits,
        })
    }

    pub fn engine(&self) -> &EngineHandle {
        &self.engine
    }

    /// A free connection slot, if any. Listeners accept first and then
    /// refuse when the service is full, so a listening pipe instance always
    /// exists and nobody else can take the pipe's name.
    pub fn try_slot(self: &Arc<Self>) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.connections.clone().try_acquire_owned().ok()
    }

    /// The answer when every slot is taken.
    pub fn full() -> ErrorBody {
        ErrorBody::new(
            "too_many_connections",
            "the service has too many connections",
        )
    }

    /// Claims a slot for `caller`, or says why not.
    pub fn claim(
        self: &Arc<Self>,
        caller: Caller,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<Slot, ErrorBody> {
        let mut counts = self.per_user.lock().unwrap_or_else(|e| e.into_inner());
        let count = counts.entry(caller).or_insert(0);
        if *count >= self.limits.per_user {
            return Err(ErrorBody::new(
                "too_many_connections",
                "too many connections from this user",
            ));
        }
        *count += 1;
        Ok(Slot {
            service: self.clone(),
            caller,
            _permit: permit,
        })
    }

    /// The user who owns the current (or last) session.
    pub fn owner(&self) -> Option<Caller> {
        *self.owner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The signed-in user `caller` signed out: their session must not
    /// outlive them, and its notices are nobody's now.
    pub async fn signed_out(&self, caller: Caller) {
        if self.owner() != Some(caller) {
            return;
        }
        if active(self.engine.status().mode) {
            tracing::info!(%caller, "the user who turned Optimize on signed out; turning it off");
            if let Err(error) = self.engine.disable().await {
                tracing::warn!(%error, "cannot turn Optimize off after sign-out");
            }
        }
        let mut owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        if *owner == Some(caller) {
            *owner = None;
        }
    }

    /// Changing the session is for its owner while it runs; claiming it
    /// for `caller` when it doesn't, so notices from turning it on are
    /// theirs too.
    fn take_control(&self, caller: Caller, claim: bool) -> Result<(), ErrorBody> {
        let mut owner = self.owner.lock().unwrap_or_else(|e| e.into_inner());
        if !may_control(self.engine.status().mode, *owner, caller) {
            return Err(not_yours());
        }
        if claim {
            *owner = Some(caller);
        }
        Ok(())
    }

    /// Status as `caller` sees it: who owns the session, if anyone.
    fn status_value(&self, status: &Status, caller: Caller) -> Value {
        let mut value = serde_json::to_value(status).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut value {
            let owner = match self.owner() {
                None => Value::Null,
                Some(owner) if owner == caller => json!("you"),
                Some(_) => json!("another_user"),
            };
            map.insert("owner".into(), owner);
        }
        value
    }

    fn event_message(&self, event: &EngineEvent, caller: Caller) -> Message {
        match event {
            EngineEvent::Status(status) => Message::Event(json!({
                "event": "status",
                "data": self.status_value(status, caller),
            })),
            other => Message::event(other),
        }
    }

    async fn settings_view(&self) -> SettingsView {
        let settings = self.settings.lock().await;
        SettingsView {
            control_url: settings.control_url.clone(),
            verbose_logging: settings.verbose_logging,
            min_improvement_ms: settings.min_improvement_ms,
            min_improvement_pct: settings.min_improvement_pct,
        }
    }

    async fn set_settings(&self, change: SettingsChange) -> Result<Value, ErrorBody> {
        let mut settings = self.settings.lock().await;
        let mut next = settings.clone();
        if let Some(verbose) = change.verbose_logging {
            next.verbose_logging = verbose;
        }
        if let Some(ms) = change.min_improvement_ms {
            next.min_improvement_ms = ms;
        }
        if let Some(pct) = change.min_improvement_pct {
            next.min_improvement_pct = pct;
        }
        next.validate()
            .map_err(|e| ErrorBody::new("invalid_settings", e))?;
        self.engine
            .set_config(next.clone())
            .await
            .map_err(|e| ErrorBody::from(&e))?;
        if next.verbose_logging != settings.verbose_logging {
            self.log_level.set_verbose(next.verbose_logging);
        }
        let (path, to_save) = (self.config_path.clone(), next.clone());
        let saved = tokio::task::spawn_blocking(move || to_save.save(&path))
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        *settings = next;
        drop(settings);
        match saved {
            Ok(()) => Ok(serde_json::to_value(self.settings_view().await).unwrap_or_default()),
            Err(error) => {
                tracing::warn!(%error, "settings applied but not saved");
                Err(ErrorBody::new(
                    "cannot_save",
                    format!("applied until the service restarts, but not saved: {error}"),
                ))
            }
        }
    }

    /// Answers one request.
    async fn handle(&self, request: Request, caller: Caller) -> Result<Value, ErrorBody> {
        let to_value = |v: &dyn erased::Serialize| v.to_json();
        match request {
            Request::Hello { .. } => Ok(to_value(&welcome())),
            Request::Games => Ok(to_value(&self.games)),
            Request::Status => Ok(self.status_value(&self.engine.status(), caller)),
            Request::Measure { game, region } => {
                self.take_control(caller, false)?;
                let measurement = self
                    .engine
                    .measure(&game, &region)
                    .await
                    .map_err(|e| ErrorBody::from(&e))?;
                Ok(to_value(&measurement))
            }
            Request::Enable {
                acknowledged_warning,
            } => {
                self.take_control(caller, true)?;
                tracing::info!(%caller, acknowledged_warning, "turning Optimize on");
                let status = self
                    .engine
                    .enable(acknowledged_warning)
                    .await
                    .map_err(|e| ErrorBody::from(&e))?;
                Ok(self.status_value(&status, caller))
            }
            Request::Disable => {
                self.take_control(caller, false)?;
                tracing::info!(%caller, "turning Optimize off");
                let status = self
                    .engine
                    .disable()
                    .await
                    .map_err(|e| ErrorBody::from(&e))?;
                Ok(self.status_value(&status, caller))
            }
            Request::Dismiss { id } => {
                if self.owner().is_some_and(|o| o != caller) {
                    return Err(not_yours());
                }
                self.engine.dismiss(id).await;
                Ok(Value::Null)
            }
            Request::Settings => Ok(to_value(&self.settings_view().await)),
            Request::SetSettings(change) => {
                tracing::info!(%caller, ?change, "settings change requested");
                self.set_settings(change).await
            }
            Request::LogPaths => Ok(to_value(&LogPaths {
                service_logs: self.log_dir.display().to_string(),
            })),
        }
    }

    /// Serves one connection until the UI disconnects, misbehaves or the
    /// service stops.
    pub async fn serve<S>(
        self: Arc<Self>,
        stream: S,
        slot: Slot,
        mut stop: watch::Receiver<bool>,
    ) -> io::Result<()>
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let caller = slot.caller;
        let (reader, writer) = tokio::io::split(stream);
        let (outbox, pending) = mpsc::channel::<Message>(OUTBOX);
        let writer = tokio::spawn(write_lines(writer, pending));
        let mut reader = BufReader::new(reader);
        let mut partial = Vec::new();
        let mut events: Option<broadcast::Receiver<EngineEvent>> = None;
        let mut bucket = Bucket::new(self.limits.rate, self.limits.burst);
        let mut strikes = 0;
        let in_flight = Arc::new(Semaphore::new(self.limits.in_flight));
        let hello_deadline = Instant::now() + self.limits.hello_timeout;
        tracing::debug!(%caller, "UI connected");

        let result = loop {
            let greeted = events.is_some();
            let hello_timeout = async move {
                if greeted {
                    std::future::pending::<()>().await;
                } else {
                    tokio::time::sleep_until(hello_deadline).await;
                }
            };
            tokio::select! {
                line = read_line(&mut reader, &mut partial) => {
                    let line = match line {
                        Ok(Some(line)) => line,
                        Ok(None) => break Ok(()),
                        Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                            let _ = outbox.send(Message::err(0, ErrorBody::bad_request("request too long"))).await;
                            break Err(e);
                        }
                        Err(e) => break Err(e),
                    };
                    if !bucket.take() {
                        strikes += 1;
                        if strikes > MAX_STRIKES {
                            tracing::warn!(%caller, "closing a connection that ignores the rate limit");
                            break Ok(());
                        }
                        let _ = outbox.try_send(Message::err(request_id(&line), ErrorBody::new("rate_limited", "too many requests")));
                        continue;
                    }
                    let envelope = match std::str::from_utf8(&line).map_err(|e| (None, ErrorBody::bad_request(e.to_string()))).and_then(ipc::parse_request) {
                        Ok(envelope) => envelope,
                        Err((id, error)) => {
                            let _ = outbox.send(Message::err(id.unwrap_or(0), error)).await;
                            continue;
                        }
                    };
                    let id = envelope.id;
                    if events.is_none() {
                        match envelope.request {
                            Request::Hello { version, client } if version == PROTOCOL_VERSION => {
                                let client = client_name(&client);
                                tracing::info!(%caller, client, "UI connected");
                                events = Some(self.engine.subscribe());
                                let _ = outbox.send(Message::ok(id, welcome())).await;
                            }
                            Request::Hello { version, client } => {
                                let client = client_name(&client);
                                tracing::warn!(%caller, client, version, "UI speaks a different protocol version");
                                let _ = outbox.send(Message::err(id, ErrorBody::new(
                                    "version_mismatch",
                                    format!("the service speaks protocol {PROTOCOL_VERSION}, the UI {version}; reinstall PingPath"),
                                ))).await;
                                break Ok(());
                            }
                            _ => {
                                let _ = outbox.send(Message::err(id, ErrorBody::new("hello_required", "send hello first"))).await;
                            }
                        }
                        continue;
                    }
                    let slow = matches!(envelope.request, Request::Measure { .. } | Request::Enable { .. } | Request::Disable);
                    if slow {
                        let Ok(permit) = in_flight.clone().try_acquire_owned() else {
                            let _ = outbox.send(Message::err(id, ErrorBody::new("too_many_requests", "wait for the previous requests to finish"))).await;
                            continue;
                        };
                        let (service, outbox) = (self.clone(), outbox.clone());
                        tokio::spawn(async move {
                            let reply = service.handle(envelope.request, caller).await;
                            let _ = outbox.send(respond(id, reply)).await;
                            drop(permit);
                        });
                    } else {
                        let reply = self.handle(envelope.request, caller).await;
                        let _ = outbox.send(respond(id, reply)).await;
                    }
                }
                event = next_event(&mut events) => match event {
                    Ok(event) => {
                        let _ = outbox.try_send(self.event_message(&event, caller));
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::debug!(%caller, missed, "UI fell behind; sending the current status");
                        let status = EngineEvent::Status(Box::new(self.engine.status()));
                        let _ = outbox.try_send(self.event_message(&status, caller));
                    }
                    Err(broadcast::error::RecvError::Closed) => break Ok(()),
                },
                _ = hello_timeout => {
                    tracing::debug!(%caller, "no hello in time; closing");
                    break Ok(());
                }
                _ = stop.changed() => break Ok(()),
            }
        };
        drop(outbox);
        let _ = writer.await;
        drop(slot);
        tracing::debug!(%caller, "UI disconnected");
        result
    }
}

/// Lets `handle` serialise its different answer types without a generic
/// closure.
mod erased {
    pub trait Serialize {
        fn to_json(&self) -> serde_json::Value;
    }

    impl<T: serde::Serialize> Serialize for T {
        fn to_json(&self) -> serde_json::Value {
            serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
        }
    }
}

fn welcome() -> Welcome {
    Welcome {
        version: PROTOCOL_VERSION,
        service: env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn not_yours() -> ErrorBody {
    ErrorBody::new(
        "owned_by_another_user",
        "Optimize was turned on by another user of this PC; only they can change it",
    )
}

fn respond(id: u64, reply: Result<Value, ErrorBody>) -> Message {
    match reply {
        Ok(result) => Message::Ok { id, result },
        Err(error) => Message::err(id, error),
    }
}

/// The `id` of a request that won't be parsed, for its error.
fn request_id(line: &[u8]) -> u64 {
    serde_json::from_slice::<Value>(line)
        .ok()
        .and_then(|v| v.get("id").and_then(Value::as_u64))
        .unwrap_or(0)
}

/// One line without its newline, `None` at the end of the stream. Lines
/// longer than [`MAX_LINE`] fail with `InvalidData` before they are read
/// whole. Cancel-safe: a partial line waits in `partial`.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    partial: &mut Vec<u8>,
) -> io::Result<Option<Vec<u8>>> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(None);
        }
        let (taken, done) = match available.iter().position(|&b| b == b'\n') {
            Some(end) => (end + 1, true),
            None => (available.len(), false),
        };
        partial.extend_from_slice(&available[..taken]);
        reader.consume(taken);
        if partial.len() > MAX_LINE + 1 {
            partial.clear();
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        if done {
            let mut line = std::mem::take(partial);
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

async fn next_event(
    events: &mut Option<broadcast::Receiver<EngineEvent>>,
) -> Result<EngineEvent, broadcast::error::RecvError> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}

async fn write_lines<W: AsyncWrite>(writer: W, mut pending: mpsc::Receiver<Message>) {
    tokio::pin!(writer);
    while let Some(message) = pending.recv().await {
        if writer
            .write_all(message.to_line().as_bytes())
            .await
            .is_err()
        {
            break;
        }
        // Flush when the queue is drained, not after every line.
        if pending.is_empty() && writer.flush().await.is_err() {
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// A token bucket: `rate` requests a second on average, bursts of `burst`.
struct Bucket {
    tokens: f64,
    rate: f64,
    burst: f64,
    last: Instant,
}

impl Bucket {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            tokens: burst,
            rate,
            burst,
            last: Instant::now(),
        }
    }

    fn take(&mut self) -> bool {
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate).min(self.burst);
        self.last = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The name a UI gives in its Hello, as logged: any signed-in user can
/// send up to a whole line, so at most 64 characters, none of them control
/// characters (which could fake log lines).
fn client_name(client: &str) -> String {
    client
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_lines_and_refuses_long_ones_before_reading_them_whole() {
        let mut input: &[u8] = b"{\"a\":1}\r\n{\"b\":2}\npartial";
        let mut reader = BufReader::new(&mut input);
        let mut partial = Vec::new();
        assert_eq!(
            read_line(&mut reader, &mut partial).await.unwrap().unwrap(),
            b"{\"a\":1}"
        );
        assert_eq!(
            read_line(&mut reader, &mut partial).await.unwrap().unwrap(),
            b"{\"b\":2}"
        );
        // An unterminated line at the end is dropped.
        assert!(
            read_line(&mut reader, &mut partial)
                .await
                .unwrap()
                .is_none()
        );

        let long = vec![b'x'; MAX_LINE * 2];
        let mut input: &[u8] = &long;
        let mut reader = BufReader::new(&mut input);
        let error = read_line(&mut reader, &mut Vec::new()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn only_the_owner_controls_a_running_session() {
        let (me, other) = (Caller::WindowsSession(1), Caller::WindowsSession(2));
        for mode in [Mode::Connecting, Mode::Optimized, Mode::FailingOver] {
            assert!(may_control(mode, Some(me), me));
            assert!(!may_control(mode, Some(other), me), "{mode:?}");
            assert!(may_control(mode, None, me));
        }
        for mode in [Mode::Idle, Mode::Measuring, Mode::Ready] {
            assert!(may_control(mode, Some(other), me), "{mode:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_bucket_allows_bursts_then_the_average_rate() {
        let mut bucket = Bucket::new(10.0, 5.0);
        assert!((0..5).all(|_| bucket.take()));
        assert!(!bucket.take());
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(bucket.take());
        assert!(!bucket.take());
    }
}
