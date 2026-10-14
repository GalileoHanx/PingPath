//! The service's IPC (client/core/src/ipc.rs) from the window's side: one
//! connection, kept open and reopened with backoff while the service isn't
//! there, requests matched to their answers by `id`, and the service's
//! pushed events handed to the app.
//!
//! I/O is blocking, on one thread per connection, and that thread never
//! waits inside a read. A Windows pipe handle opened without
//! `FILE_FLAG_OVERLAPPED` serialises every read and write on it, so a read
//! waiting for the service's next event would hold back a request until the
//! service happened to send something. The thread reads only what has
//! already arrived ([`Transport::read_now`]) and otherwise waits on its
//! queue of requests: a request goes out at once, and an answer is picked
//! up within [`POLL`].

use std::collections::HashMap;
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use pingpath_client_core::ipc::{Envelope, ErrorBody, Message, PROTOCOL_VERSION, Request, Welcome};
use serde::Serialize;
use serde_json::Value;

/// How long an answer may wait before the I/O thread picks it up.
pub const POLL: Duration = Duration::from_millis(20);
/// How long the service has to answer `hello`.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// A connection that lasted this long worked, so the wait before the next
/// attempt starts short again. One that ends sooner (a service that
/// answers hello and then hangs up while it stops, say) keeps the backoff
/// growing; otherwise the window would reconnect four times a second, with
/// a log line at both ends each time.
const STABLE: Duration = Duration::from_secs(10);
/// Measuring takes a few seconds and turning Optimize on a few more
/// (docs/decisions/009); anything slower than this has gone wrong.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
/// The service's lines are small (a status is a few kilobytes); a longer
/// one means something other than the service is on the other end.
const MAX_INCOMING_LINE: usize = 4 * 1024 * 1024;

/// A connection to the service.
pub trait Transport: Send {
    /// Reads what has already arrived, without waiting: `Ok(0)` when
    /// nothing has. A closed connection is an error.
    fn read_now(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    fn write_all(&mut self, data: &[u8]) -> io::Result<()>;
}

/// Opens a new connection to the service.
pub type Connector = Box<dyn Fn() -> io::Result<Box<dyn Transport>> + Send>;

/// Why the service can't be reached, in the banner's terms. The page says
/// each in words (`connection.<reason>` in its translations), and
/// tools/check-i18n.py reads this list to make sure it can. The service
/// can also refuse the connection with one of its own error codes
/// (`version_mismatch`, `too_many_connections`, ...), which is passed on
/// as the reason.
pub mod reason {
    /// Nothing listens on the pipe: the service is stopped or not installed.
    pub const NOT_RUNNING: &str = "not_running";
    /// Windows refused to open the pipe for this user.
    pub const ACCESS_DENIED: &str = "access_denied";
    /// Every instance of the pipe is in use for a moment.
    pub const BUSY: &str = "busy";
    /// A working connection closed.
    pub const LOST: &str = "lost";
    /// Anything else, with the system's words in `detail`.
    pub const FAILED: &str = "failed";
}

/// Where the connection stands, for the banner in the window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Connection {
    /// `connecting`, `connected` or `unavailable`.
    pub state: &'static str,
    /// Why it is unavailable: one of [`reason`], or the error code the
    /// service refused us with.
    pub reason: Option<String>,
    /// The system's or the service's own words, for the log and the
    /// details line.
    pub detail: Option<String>,
    pub service_version: Option<String>,
    /// When the next attempt is due.
    pub retry_in_ms: Option<u64>,
}

impl Connection {
    fn connecting() -> Self {
        Self {
            state: "connecting",
            reason: None,
            detail: None,
            service_version: None,
            retry_in_ms: None,
        }
    }

    fn connected(service_version: String) -> Self {
        Self {
            state: "connected",
            service_version: Some(service_version),
            ..Self::connecting()
        }
    }

    fn unavailable(reason: &str, detail: Option<String>, retry_in: Duration) -> Self {
        Self {
            state: "unavailable",
            reason: Some(reason.to_string()),
            detail,
            service_version: None,
            retry_in_ms: Some(retry_in.as_millis() as u64),
        }
    }

    pub fn is_connected(&self) -> bool {
        self.state == "connected"
    }
}

/// What the service (or the connection) tells the app.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Connection(Connection),
    /// A status object, `owner` included.
    Status(Value),
    /// One point for the live graph.
    Sample(Value),
}

pub type Sink = Arc<dyn Fn(Event) + Send + Sync>;

/// The app's handle on the connection; cheap to clone.
#[derive(Clone)]
pub struct Client {
    shared: Arc<Shared>,
}

struct Shared {
    inner: Mutex<Inner>,
    /// Cuts a wait between attempts short (the banner's "Try again").
    retry: Condvar,
    request_timeout: Duration,
}

struct Inner {
    /// The current connection's queue of request lines; `None` while there
    /// is no connection.
    queue: Option<Sender<String>>,
    pending: HashMap<u64, SyncSender<Result<Value, ErrorBody>>>,
    next_id: u64,
    connection: Connection,
    retry_now: bool,
}

/// How a connection attempt or a connection ended.
struct Ended {
    reason: String,
    detail: Option<String>,
    wait: Duration,
    /// The connection worked for at least [`STABLE`], so start the backoff
    /// over.
    stable: bool,
}

impl Client {
    /// A client that isn't connecting yet: [`Client::run`] starts it, once
    /// whatever its events need is in place.
    pub fn new() -> Self {
        Self::with_timeout(REQUEST_TIMEOUT)
    }

    /// Starts connecting in the background, and reconnecting whenever the
    /// connection is lost; events go to `sink`, from the I/O thread.
    pub fn run(&self, connect: Connector, client_name: String, sink: Sink) -> io::Result<()> {
        let shared = self.shared.clone();
        thread::Builder::new()
            .name("service-connection".into())
            .spawn(move || supervise(&shared, &connect, &client_name, &sink))
            .map(drop)
    }

    fn with_timeout(request_timeout: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    queue: None,
                    pending: HashMap::new(),
                    next_id: 1,
                    connection: Connection::connecting(),
                    retry_now: false,
                }),
                retry: Condvar::new(),
                request_timeout,
            }),
        }
    }

    pub fn connection(&self) -> Connection {
        self.shared.lock().connection.clone()
    }

    /// Tries again now instead of waiting out the backoff.
    pub fn retry_now(&self) {
        self.shared.lock().retry_now = true;
        self.shared.retry.notify_all();
    }

    /// Sends a request and waits for its answer. Blocks: call it off the
    /// UI thread.
    pub fn call(&self, request: Request) -> Result<Value, ErrorBody> {
        let (reply, answer) = mpsc::sync_channel(1);
        let id = {
            let mut inner = self.shared.lock();
            let Some(queue) = inner.queue.clone() else {
                return Err(ErrorBody::new(
                    "service_unavailable",
                    "not connected to the PingPath service",
                ));
            };
            let id = inner.next_id;
            inner.next_id += 1;
            inner.pending.insert(id, reply);
            if queue.send(encode(id, request)).is_err() {
                inner.pending.remove(&id);
                return Err(ErrorBody::new(
                    "service_unavailable",
                    "not connected to the PingPath service",
                ));
            }
            id
        };
        match answer.recv_timeout(self.shared.request_timeout) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                self.shared.lock().pending.remove(&id);
                Err(ErrorBody::new(
                    "timeout",
                    "the service didn't answer in time",
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(ErrorBody::new(
                "disconnected",
                "the connection to the service closed before it answered",
            )),
        }
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_id(&self) -> u64 {
        let mut inner = self.lock();
        let id = inner.next_id;
        inner.next_id += 1;
        id
    }

    fn set_connection(&self, connection: Connection, sink: &Sink) {
        let changed = {
            let mut inner = self.lock();
            let changed = inner.connection != connection;
            inner.connection = connection.clone();
            changed
        };
        if changed {
            sink(Event::Connection(connection));
        }
    }

    /// Waits before the next attempt, unless asked to try again now.
    fn wait_retry(&self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let mut inner = self.lock();
        while !inner.retry_now {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            inner = self
                .retry
                .wait_timeout(inner, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        inner.retry_now = false;
    }

    /// Answers a waiting request. Unknown ids (a request that timed out)
    /// are dropped.
    fn complete(&self, id: u64, result: Result<Value, ErrorBody>) {
        let waiting = self.lock().pending.remove(&id);
        match waiting {
            Some(reply) => {
                let _ = reply.try_send(result);
            }
            None => match result {
                // The service answers lines it can't parse with id 0.
                Err(error) => tracing::warn!(
                    id,
                    code = error.code,
                    message = error.message,
                    "the service refused a request"
                ),
                Ok(_) => tracing::debug!(id, "answer to a request nobody is waiting for"),
            },
        }
    }

    /// The connection is gone: no new requests, and every waiting one
    /// fails (dropping its reply channel wakes it with "disconnected").
    fn disconnected(&self) {
        let mut inner = self.lock();
        inner.queue = None;
        inner.pending.clear();
    }
}

fn encode(id: u64, request: Request) -> String {
    let mut line =
        serde_json::to_string(&Envelope { id, request }).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    line
}

/// Connects, serves the connection until it ends, waits, and again.
fn supervise(shared: &Shared, connect: &Connector, client_name: &str, sink: &Sink) {
    let mut backoff = Backoff::default();
    let mut last_reason = None;
    loop {
        let ended = match connect() {
            Ok(transport) => serve(shared, transport, client_name, sink),
            Err(error) => connect_failed(&error),
        };
        if ended.stable {
            backoff = Backoff::default();
        }
        let wait = ended.wait.max(backoff.next());
        // Logged when the reason changes, not at every attempt.
        if last_reason.as_ref() != Some(&ended.reason) {
            tracing::warn!(
                reason = ended.reason,
                detail = ended.detail.as_deref().unwrap_or(""),
                "the PingPath service is unavailable; retrying"
            );
        }
        last_reason = Some(ended.reason.clone());
        shared.set_connection(
            Connection::unavailable(&ended.reason, ended.detail, wait),
            sink,
        );
        shared.wait_retry(wait);
    }
}

/// Why opening the pipe (or socket) failed, in the banner's terms.
fn connect_failed(error: &io::Error) -> Ended {
    /// All instances of the pipe are in use; one frees up quickly.
    const ERROR_PIPE_BUSY: i32 = 231;
    let (reason, wait) = match error.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => {
            (reason::NOT_RUNNING, Duration::ZERO)
        }
        io::ErrorKind::PermissionDenied => (reason::ACCESS_DENIED, Duration::from_secs(10)),
        _ if cfg!(windows) && error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
            (reason::BUSY, Duration::from_millis(200))
        }
        _ => (reason::FAILED, Duration::ZERO),
    };
    Ended {
        reason: reason.to_string(),
        detail: Some(error.to_string()),
        wait,
        stable: false,
    }
}

/// Says hello, then moves requests out and answers and events in until the
/// connection fails.
fn serve(
    shared: &Shared,
    mut transport: Box<dyn Transport>,
    client_name: &str,
    sink: &Sink,
) -> Ended {
    let mut lines = Lines::default();
    let (welcome, early) = match hello(shared, transport.as_mut(), &mut lines, client_name) {
        Ok(answer) => answer,
        Err(ended) => return ended,
    };
    tracing::info!(
        service = welcome.service,
        "connected to the PingPath service"
    );
    let since = Instant::now();
    let (queue, requests) = mpsc::channel();
    shared.lock().queue = Some(queue);
    shared.set_connection(Connection::connected(welcome.service), sink);
    for line in early {
        dispatch(shared, &line, sink);
    }
    let error = pump(shared, transport.as_mut(), &requests, &mut lines, sink);
    shared.disconnected();
    let lasted = since.elapsed();
    tracing::warn!(
        %error,
        lasted_ms = lasted.as_millis() as u64,
        "lost the connection to the PingPath service"
    );
    Ended {
        reason: reason::LOST.to_string(),
        detail: Some(error.to_string()),
        wait: Duration::ZERO,
        stable: lasted >= STABLE,
    }
}

/// Says hello and waits for the answer. Returns the welcome and whatever
/// lines came after it in the same read (the service starts sending events
/// right after it answers), so none is lost.
fn hello(
    shared: &Shared,
    transport: &mut dyn Transport,
    lines: &mut Lines,
    client_name: &str,
) -> Result<(Welcome, Vec<Vec<u8>>), Ended> {
    let failed = |error: io::Error| Ended {
        reason: reason::FAILED.to_string(),
        detail: Some(error.to_string()),
        wait: Duration::ZERO,
        stable: false,
    };
    let id = shared.next_id();
    let request = Request::Hello {
        version: PROTOCOL_VERSION,
        client: client_name.to_string(),
    };
    transport
        .write_all(encode(id, request).as_bytes())
        .map_err(failed)?;
    let deadline = Instant::now() + HELLO_TIMEOUT;
    let mut buf = [0u8; 8192];
    loop {
        let n = transport.read_now(&mut buf).map_err(failed)?;
        let mut received = lines.push(&buf[..n]).map_err(failed)?.into_iter();
        while let Some(line) = received.next() {
            match serde_json::from_slice::<Message>(&line) {
                Ok(Message::Ok {
                    id: answered,
                    result,
                }) if answered == id => {
                    let welcome = serde_json::from_value::<Welcome>(result)
                        .map_err(|e| failed(io::Error::new(io::ErrorKind::InvalidData, e)))?;
                    return Ok((welcome, received.collect()));
                }
                // Refused before or at hello: too many connections, a
                // different protocol version, ...
                Ok(Message::Err { error, .. }) => {
                    let wait = match error.code.as_str() {
                        // Only a reinstall fixes it; keep checking now and
                        // then in case that happens.
                        "version_mismatch" => Duration::from_secs(30),
                        _ => Duration::from_secs(5),
                    };
                    return Err(Ended {
                        reason: error.code,
                        detail: Some(error.message),
                        wait,
                        stable: false,
                    });
                }
                // Nothing else should come first; ignore it.
                Ok(_) | Err(_) => {}
            }
        }
        if Instant::now() >= deadline {
            return Err(failed(io::Error::new(
                io::ErrorKind::TimedOut,
                "the service didn't answer hello",
            )));
        }
        if n == 0 {
            thread::sleep(POLL);
        }
    }
}

/// Runs a connection until it fails, returning why.
fn pump(
    shared: &Shared,
    transport: &mut dyn Transport,
    requests: &Receiver<String>,
    lines: &mut Lines,
    sink: &Sink,
) -> io::Error {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // Waiting here, not in a read, keeps the handle free for writes.
        match requests.recv_timeout(POLL) {
            Ok(line) => {
                if let Err(error) = transport.write_all(line.as_bytes()) {
                    return error;
                }
                while let Ok(line) = requests.try_recv() {
                    if let Err(error) = transport.write_all(line.as_bytes()) {
                        return error;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            // `shared` holds the sender for as long as this runs.
            Err(RecvTimeoutError::Disconnected) => {
                return io::Error::other("the request queue closed");
            }
        }
        loop {
            let n = match transport.read_now(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(error) => return error,
            };
            let received = match lines.push(&buf[..n]) {
                Ok(received) => received,
                Err(error) => return error,
            };
            for line in received {
                dispatch(shared, &line, sink);
            }
        }
    }
}

fn dispatch(shared: &Shared, line: &[u8], sink: &Sink) {
    match serde_json::from_slice::<Message>(line) {
        Ok(Message::Ok { id, result }) => shared.complete(id, Ok(result)),
        Ok(Message::Err { id, error }) => shared.complete(id, Err(error)),
        Ok(Message::Event(mut event)) => {
            let data = event
                .get_mut("data")
                .map(Value::take)
                .unwrap_or(Value::Null);
            match event.get("event").and_then(Value::as_str) {
                Some("status") => sink(Event::Status(data)),
                Some("sample") => sink(Event::Sample(data)),
                other => {
                    tracing::debug!(event = other, "ignoring an unknown event from the service")
                }
            }
        }
        Err(error) => tracing::warn!(%error, "the service sent a line that isn't a message"),
    }
}

/// Splits what arrives into lines.
#[derive(Default)]
struct Lines {
    partial: Vec<u8>,
}

impl Lines {
    fn push(&mut self, mut data: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        let mut lines = Vec::new();
        while let Some(end) = data.iter().position(|&b| b == b'\n') {
            self.partial.extend_from_slice(&data[..end]);
            data = &data[end + 1..];
            let mut line = std::mem::take(&mut self.partial);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            if !line.is_empty() {
                lines.push(line);
            }
        }
        self.partial.extend_from_slice(data);
        if self.partial.len() > MAX_INCOMING_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the service sent a line that is far too long",
            ));
        }
        Ok(lines)
    }
}

/// The wait between attempts while the service isn't there: short at
/// first (it may be restarting), then every few seconds.
#[derive(Default)]
struct Backoff {
    attempts: u32,
}

impl Backoff {
    const STEPS_MS: [u64; 5] = [250, 500, 1000, 2000, 4000];
    const MAX: Duration = Duration::from_secs(5);

    fn next(&mut self) -> Duration {
        let step = Self::STEPS_MS
            .get(self.attempts as usize)
            .map(|&ms| Duration::from_millis(ms))
            .unwrap_or(Self::MAX);
        self.attempts = self.attempts.saturating_add(1);
        step
    }
}

/// A stream that reads without waiting: the Unix socket in development,
/// and loopback TCP in the tests.
#[cfg(any(unix, test))]
pub struct NonBlocking<S> {
    stream: S,
}

#[cfg(any(unix, test))]
impl<S> NonBlocking<S> {
    /// `stream` must already be in non-blocking mode.
    pub fn new(stream: S) -> Self {
        Self { stream }
    }
}

#[cfg(any(unix, test))]
impl<S: io::Read + io::Write + Send> Transport for NonBlocking<S> {
    fn read_now(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.stream.read(buf) {
            Ok(0) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the service closed the connection",
            )),
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn write_all(&mut self, mut data: &[u8]) -> io::Result<()> {
        // The service reads promptly; one that stops reading for this long
        // is stuck.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !data.is_empty() {
            match self.stream.write(data) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => data = &data[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};

    use pingpath_client_core::ipc::SettingsChange;
    use serde_json::json;

    use super::*;

    /// A fake service on loopback TCP: answers hello, then hands every
    /// request to `answer`, which writes what it likes.
    fn fake_service(
        answer: impl Fn(&Value, &mut TcpStream) + Send + 'static,
    ) -> (Connector, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { break };
                let request: Value = serde_json::from_str(&line).unwrap();
                if request["method"] == "hello" {
                    assert_eq!(request["params"]["version"], PROTOCOL_VERSION);
                    let welcome = json!({"id": request["id"], "result": {"version": PROTOCOL_VERSION, "service": "9.9.9"}});
                    writeln!(writer, "{welcome}").unwrap();
                    continue;
                }
                answer(&request, &mut writer);
            }
        });
        let connect: Connector = Box::new(move || {
            let stream = TcpStream::connect(addr)?;
            stream.set_nonblocking(true)?;
            Ok(Box::new(NonBlocking::new(stream)) as Box<dyn Transport>)
        });
        (connect, server)
    }

    fn start(connect: Connector, sink: Sink) -> Client {
        let client = Client::new();
        client.run(connect, "PingPath test".into(), sink).unwrap();
        client
    }

    fn collecting_sink() -> (Sink, Arc<Mutex<Vec<Event>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let seen = events.clone();
        (Arc::new(move |e| seen.lock().unwrap().push(e)), events)
    }

    fn wait_until(what: &str, check: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !check() {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn matches_answers_to_requests_and_forwards_events() {
        let (connect, _server) = fake_service(|request, out| {
            let id = &request["id"];
            match request["method"].as_str().unwrap() {
                // An event first, then the answer, as the service does
                // while it measures.
                "measure" => {
                    writeln!(
                        out,
                        "{}",
                        json!({"event": "status", "data": {"mode": "measuring", "owner": null}})
                    )
                    .unwrap();
                    writeln!(
                        out,
                        "{}",
                        json!({"id": id, "result": {"game": request["params"]["game"]}})
                    )
                    .unwrap();
                }
                "enable" => {
                    assert_eq!(request["params"]["acknowledged_warning"], false);
                    writeln!(out, "{}", json!({"id": id, "error": {"code": "warning_not_acknowledged", "message": "acknowledge first"}})).unwrap();
                }
                "set_settings" => {
                    assert_eq!(
                        request["params"],
                        json!({"verbose_logging": true, "min_improvement_ms": null, "min_improvement_pct": null, "stability_mode": null})
                    );
                    writeln!(
                        out,
                        "{}",
                        json!({"event": "sample", "data": {"ping_ms": 42.0}})
                    )
                    .unwrap();
                    writeln!(
                        out,
                        "{}",
                        json!({"id": id, "result": {"verbose_logging": true}})
                    )
                    .unwrap();
                }
                other => panic!("unexpected {other}"),
            }
        });
        let (sink, events) = collecting_sink();
        let client = start(connect, sink);
        wait_until("connected", || client.connection().is_connected());
        assert_eq!(
            client.connection().service_version.as_deref(),
            Some("9.9.9")
        );

        let measured = client
            .call(Request::Measure {
                game: "cs2".into(),
                region: "eu".into(),
            })
            .unwrap();
        assert_eq!(measured["game"], "cs2");
        let refused = client
            .call(Request::Enable {
                acknowledged_warning: false,
            })
            .unwrap_err();
        assert_eq!(refused.code, "warning_not_acknowledged");
        let change = SettingsChange {
            verbose_logging: Some(true),
            ..Default::default()
        };
        assert_eq!(
            client.call(Request::SetSettings(change)).unwrap()["verbose_logging"],
            true
        );

        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Status(s) if s["mode"] == "measuring"))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Sample(s) if s["ping_ms"] == 42.0))
        );
        assert!(matches!(&events[0], Event::Connection(c) if c.state == "connected"));
    }

    #[test]
    fn a_waiting_request_does_not_block_the_next_one() {
        // The service answers `disable` only after `status` arrives: a
        // client that blocked on the first answer would never send the
        // second request.
        let held = Arc::new(Mutex::new(None::<Value>));
        let (connect, _server) =
            fake_service(
                move |request, out| match request["method"].as_str().unwrap() {
                    "disable" => *held.lock().unwrap() = Some(request["id"].clone()),
                    "status" => {
                        writeln!(
                            out,
                            "{}",
                            json!({"id": request["id"], "result": {"mode": "idle"}})
                        )
                        .unwrap();
                        let id = held.lock().unwrap().take().unwrap();
                        writeln!(out, "{}", json!({"id": id, "result": {"mode": "ready"}}))
                            .unwrap();
                    }
                    other => panic!("unexpected {other}"),
                },
            );
        let (sink, _) = collecting_sink();
        let client = start(connect, sink);
        wait_until("connected", || client.connection().is_connected());
        let slow = {
            let client = client.clone();
            thread::spawn(move || client.call(Request::Disable))
        };
        thread::sleep(Duration::from_millis(100));
        assert_eq!(client.call(Request::Status).unwrap()["mode"], "idle");
        assert_eq!(slow.join().unwrap().unwrap()["mode"], "ready");
    }

    #[test]
    fn a_lost_connection_fails_waiting_requests_and_reconnects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for round in 0..2 {
                let (stream, _) = listener.accept().unwrap();
                let mut writer = stream.try_clone().unwrap();
                let mut lines = BufReader::new(stream).lines();
                let hello: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                writeln!(writer, "{}", json!({"id": hello["id"], "result": {"version": PROTOCOL_VERSION, "service": format!("{round}")}})).unwrap();
                // Read one request and hang up without answering it.
                let _ = lines.next();
                if round == 1 {
                    thread::sleep(Duration::from_millis(200));
                }
            }
        });
        let connect: Connector = Box::new(move || {
            let stream = TcpStream::connect(addr)?;
            stream.set_nonblocking(true)?;
            Ok(Box::new(NonBlocking::new(stream)) as Box<dyn Transport>)
        });
        let (sink, events) = collecting_sink();
        let client = start(connect, sink);
        wait_until("connected", || client.connection().is_connected());
        let error = client.call(Request::Games).unwrap_err();
        assert_eq!(error.code, "disconnected");
        wait_until("reconnected", || {
            client.connection().service_version.as_deref() == Some("1")
        });
        let _ = client.call(Request::Games);
        server.join().unwrap();
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Connection(c) if c.reason.as_deref() == Some("lost")))
        );
    }

    #[test]
    fn says_why_the_service_cannot_be_reached() {
        let connect: Connector = Box::new(|| Err(io::ErrorKind::NotFound.into()));
        let (sink, events) = collecting_sink();
        let client = start(connect, sink);
        wait_until("unavailable", || client.connection().state == "unavailable");
        let connection = client.connection();
        assert_eq!(connection.reason.as_deref(), Some("not_running"));
        assert!(connection.retry_in_ms.is_some());
        let error = client.call(Request::Status).unwrap_err();
        assert_eq!(error.code, "service_unavailable");
        assert!(!events.lock().unwrap().is_empty());

        let refused = connect_failed(&io::ErrorKind::PermissionDenied.into());
        assert_eq!(refused.reason, "access_denied");
    }

    #[test]
    fn a_refusal_at_hello_is_reported_with_its_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut writer = stream.try_clone().unwrap();
            let mut lines = BufReader::new(stream).lines();
            let hello: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
            writeln!(writer, "{}", json!({"id": hello["id"], "error": {"code": "version_mismatch", "message": "reinstall PingPath"}})).unwrap();
        });
        let connect: Connector = Box::new(move || {
            let stream = TcpStream::connect(addr)?;
            stream.set_nonblocking(true)?;
            Ok(Box::new(NonBlocking::new(stream)) as Box<dyn Transport>)
        });
        let (sink, _) = collecting_sink();
        let client = start(connect, sink);
        wait_until("refused", || client.connection().state == "unavailable");
        let connection = client.connection();
        assert_eq!(connection.reason.as_deref(), Some("version_mismatch"));
        assert_eq!(connection.retry_in_ms, Some(30_000));
    }

    #[test]
    fn a_service_that_hangs_up_after_hello_is_retried_ever_more_slowly() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(Mutex::new(0usize));
        let count = accepted.clone();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                *count.lock().unwrap() += 1;
                let mut writer = stream.try_clone().unwrap();
                let mut lines = BufReader::new(stream).lines();
                let Some(Ok(hello)) = lines.next() else {
                    continue;
                };
                let hello: Value = serde_json::from_str(&hello).unwrap();
                let welcome = json!({"id": hello["id"], "result": {"version": PROTOCOL_VERSION, "service": "0"}});
                let _ = writeln!(writer, "{welcome}");
                // Dropping both ends hangs up at once.
            }
        });
        let connect: Connector = Box::new(move || {
            let stream = TcpStream::connect(addr)?;
            stream.set_nonblocking(true)?;
            Ok(Box::new(NonBlocking::new(stream)) as Box<dyn Transport>)
        });
        let (sink, _) = collecting_sink();
        let _client = start(connect, sink);
        // Waits of 250, 500 and 1000 ms: attempts at about 0, 0.25 and
        // 0.75 s. Starting the backoff over after each hello would make it
        // one every 250 ms, 7 by now. A slow machine only makes fewer.
        thread::sleep(Duration::from_millis(1600));
        let attempts = *accepted.lock().unwrap();
        assert!(
            (1..=4).contains(&attempts),
            "{attempts} connections in 1.6 s"
        );
    }

    /// Hands out `incoming` in one read, then reports the connection
    /// closed; keeps what is written.
    struct Scripted {
        incoming: Vec<u8>,
        written: Arc<Mutex<Vec<u8>>>,
    }

    impl Transport for Scripted {
        fn read_now(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.incoming.is_empty() {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            let n = self.incoming.len().min(buf.len());
            buf[..n].copy_from_slice(&self.incoming[..n]);
            self.incoming.drain(..n);
            Ok(n)
        }

        fn write_all(&mut self, data: &[u8]) -> io::Result<()> {
            self.written.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
    }

    #[test]
    fn events_that_arrive_with_the_welcome_are_not_lost() {
        // A fresh client's hello has id 1; the service subscribes it to
        // events as it answers, so a status can follow in the same read.
        let welcome = json!({"id": 1, "result": {"version": PROTOCOL_VERSION, "service": "9.9.9"}});
        let status = json!({"event": "status", "data": {"mode": "optimized", "owner": "you"}});
        let written = Arc::new(Mutex::new(Vec::new()));
        let transport = Scripted {
            incoming: format!("{welcome}\n{status}\n").into_bytes(),
            written: written.clone(),
        };
        let client = Client::new();
        let (sink, events) = collecting_sink();
        let ended = serve(&client.shared, Box::new(transport), "PingPath test", &sink);
        assert_eq!(ended.reason, "lost");
        assert!(!ended.stable, "a connection that ended at once");
        let hello: Value =
            serde_json::from_slice(written.lock().unwrap().strip_suffix(b"\n").unwrap()).unwrap();
        assert_eq!(hello["method"], "hello");
        assert_eq!(hello["id"], 1);
        let events = events.lock().unwrap();
        assert!(matches!(&events[0], Event::Connection(c) if c.is_connected()));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Status(s) if s["mode"] == "optimized")),
            "{events:?}"
        );
    }

    #[test]
    fn lines_split_across_reads_and_long_lines_fail() {
        let mut lines = Lines::default();
        assert!(lines.push(b"{\"a\":").unwrap().is_empty());
        assert_eq!(
            lines.push(b"1}\r\n{\"b\":2}\n\n{").unwrap(),
            vec![b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]
        );
        assert_eq!(lines.partial, b"{");
        let long = vec![b'x'; MAX_INCOMING_LINE + 1];
        assert!(Lines::default().push(&long).is_err());
    }

    #[test]
    fn backoff_grows_to_a_ceiling() {
        let mut backoff = Backoff::default();
        let waits: Vec<u64> = (0..7).map(|_| backoff.next().as_millis() as u64).collect();
        assert_eq!(waits, [250, 500, 1000, 2000, 4000, 5000, 5000]);
    }

    #[test]
    fn a_request_times_out() {
        let client = Client::with_timeout(Duration::from_millis(50));
        let (queue, _requests) = mpsc::channel();
        client.shared.lock().queue = Some(queue);
        assert_eq!(client.call(Request::Status).unwrap_err().code, "timeout");
        assert!(client.shared.lock().pending.is_empty());
    }
}
