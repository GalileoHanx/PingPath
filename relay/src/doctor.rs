//! The doctor listener: load for clients' bufferbloat tests
//! (docs/decisions/011-home-network-doctor.md), on a TCP and UDP port of
//! its own.
//!
//! TCP: the client sends a 2-byte length (big-endian), a doctor ticket and
//! one byte, [`DOWN`] or [`UP`]; the relay answers one byte, [`ACCEPTED`]
//! or [`REFUSED`]. Then for down the relay writes until the ticket's byte
//! budget or time runs out (or the client hangs up); for up it reads and
//! drops what the client sends, within the same budget.
//!
//! UDP: a datagram of at most [`MAX_ECHO`] bytes is sent back as it came,
//! but only to an address that has a load connection open: TCP's handshake
//! proved that address, so the echo can't be aimed at someone else (rule
//! 10). Each address gets at most [`ECHOES_PER_SECOND`].
//!
//! Nothing here keeps a client's address once its connections close, and
//! nothing logs one (rule 9).

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pingpath_tunnel::ticket::{self, ACCEPTED, DOWN, MAX_ECHO, REFUSED, Ticket, UP};
use pingpath_tunnel::token::now_unix;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::ratelimit::TokenBucket;
use crate::relay::Shared;

/// Connections one ticket may hold open at once.
pub const MAX_CONNECTIONS_PER_TICKET: usize = 16;
/// Load connections the relay holds open at once, all tickets together.
pub const MAX_CONNECTIONS: usize = 256;
/// Tickets in use at once: tests running side by side on this relay. A
/// test that would make one more is refused ("relay busy").
pub const MAX_TESTS: usize = 8;
/// Connections that haven't shown a ticket yet, all sources together;
/// more are closed at once.
pub const MAX_PENDING: usize = 512;
/// New connections one source network (a /24, or a /64) may open: a test
/// opens 17 (a keeper and eight each way) within seconds.
const CONNECTS_PER_SECOND: u64 = 4;
const CONNECTS_BURST: u64 = 40;
/// Source networks remembered for that budget before idle ones are dropped.
const MAX_SOURCES: usize = 4096;
/// A write that makes no progress for this long ends the connection: the
/// client stopped reading.
pub const ECHOES_PER_SECOND: u64 = 50;
/// The request must arrive within this.
const HEADER_TIMEOUT: Duration = Duration::from_secs(5);
/// An upload connection that sends nothing for this long is closed, and
/// so is a download whose client stops reading.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// A load connection ends this long after its ticket expires at the latest.
const EXPIRY_SLACK_SECS: u64 = 2;
const CHUNK: usize = 64 * 1024;

/// What a ticket has used so far.
struct Use {
    open: usize,
    sent: u64,
    received: u64,
    expires_at: u64,
    started: Instant,
}

#[derive(Default)]
struct State {
    tickets: HashMap<[u8; ticket::ID_LEN], Use>,
    /// Load connections open per client address.
    open: HashMap<IpAddr, usize>,
    total_open: usize,
    echoes: HashMap<IpAddr, TokenBucket>,
    /// New connections per source network, and when each was last used.
    connects: HashMap<IpAddr, (TokenBucket, Instant)>,
}

pub(crate) struct Doctor {
    shared: Arc<Shared>,
    state: Mutex<State>,
    /// Connections that haven't shown a ticket yet.
    pending: Arc<tokio::sync::Semaphore>,
    /// Bytes the relay writes: random, so nothing along the way can
    /// compress them.
    filler: Vec<u8>,
}

/// Binds the TCP and UDP sockets on `addr` (the same port for both) and
/// serves them.
pub(crate) async fn serve(
    shared: Arc<Shared>,
    addr: SocketAddr,
) -> io::Result<(SocketAddr, Vec<JoinHandle<()>>)> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let udp = UdpSocket::bind(bound).await?;
    let mut filler = Vec::with_capacity(CHUNK);
    while filler.len() < CHUNK {
        filler.extend_from_slice(
            &pingpath_tunnel::keys::random_32().map_err(|e| io::Error::other(e.to_string()))?,
        );
    }
    let doctor = Arc::new(Doctor {
        shared,
        state: Mutex::new(State::default()),
        pending: Arc::new(tokio::sync::Semaphore::new(MAX_PENDING)),
        filler,
    });
    let accept = {
        let doctor = doctor.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        // Before any work for it: within its network's
                        // budget, and room among the connections waiting
                        // to show a ticket. Otherwise it is just closed.
                        let permit = match doctor.pending.clone().try_acquire_owned() {
                            Ok(permit) if doctor.may_connect(peer.ip()) => permit,
                            _ => {
                                doctor
                                    .metrics()
                                    .doctor_refused
                                    .fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        };
                        let doctor = doctor.clone();
                        tokio::spawn(async move { doctor.handle(stream, peer, permit).await });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "doctor listener: accept failed");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        })
    };
    let echo = tokio::spawn(async move { doctor.echo(udp).await });
    Ok((bound, vec![accept, echo]))
}

impl Doctor {
    fn metrics(&self) -> &crate::metrics::Metrics {
        &self.shared.metrics
    }

    /// A new connection from `ip`, within its source network's budget.
    fn may_connect(&self, ip: IpAddr) -> bool {
        let network = crate::relay::source_network(ip);
        let now = Instant::now();
        let mut state = self.state.lock().expect("doctor state lock");
        if state.connects.len() >= MAX_SOURCES && !state.connects.contains_key(&network) {
            // A full bucket refills in CONNECTS_BURST / CONNECTS_PER_SECOND
            // seconds: one idle that long has nothing to remember.
            let full = Duration::from_secs(CONNECTS_BURST / CONNECTS_PER_SECOND);
            state.connects.retain(|_, (_, used)| now - *used < full);
            if state.connects.len() >= MAX_SOURCES {
                return false;
            }
        }
        let (bucket, used) = state.connects.entry(network).or_insert_with(|| {
            (
                TokenBucket::new_at(CONNECTS_PER_SECOND, CONNECTS_BURST, now),
                now,
            )
        });
        *used = now;
        bucket.take_at(1, now)
    }

    async fn handle(
        &self,
        mut stream: TcpStream,
        peer: SocketAddr,
        pending: tokio::sync::OwnedSemaphorePermit,
    ) {
        let _ = stream.set_nodelay(true);
        let request = match timeout(HEADER_TIMEOUT, read_request(&mut stream)).await {
            Ok(Ok(request)) => request,
            Ok(Err(_)) | Err(_) => {
                self.metrics()
                    .doctor_refused
                    .fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        let admitted = self.admit(&request, peer.ip());
        drop(pending);
        let (ticket, direction) = match admitted {
            Ok(admitted) => admitted,
            Err(reason) => {
                self.metrics()
                    .doctor_refused
                    .fetch_add(1, Ordering::Relaxed);
                tracing::debug!(reason, "bufferbloat test connection refused");
                let _ = stream.write_all(&[REFUSED]).await;
                return;
            }
        };
        // However the client behaves, the connection ends with its ticket.
        let left = ticket
            .expires_at
            .saturating_sub(now_unix())
            .saturating_add(EXPIRY_SLACK_SECS);
        let serve = async {
            if stream.write_all(&[ACCEPTED]).await.is_ok() {
                match direction {
                    DOWN => self.send(&mut stream, &ticket).await,
                    _ => self.receive(&mut stream, &ticket).await,
                }
            }
        };
        let _ = timeout(Duration::from_secs(left), serve).await;
        self.release(&ticket, peer.ip());
    }

    /// Checks the ticket and counts the connection against it.
    fn admit(&self, request: &[u8], ip: IpAddr) -> Result<(Ticket, u8), &'static str> {
        let (bytes, direction) = request.split_at(request.len() - 1);
        let direction = direction[0];
        if direction != DOWN && direction != UP {
            return Err("unknown direction");
        }
        let now = now_unix();
        let ticket = self
            .shared
            .verifier
            .verify_ticket(bytes, now)
            .map_err(|_| "invalid ticket")?;
        if ticket.relay_id != self.shared.config.id {
            return Err("ticket for another relay");
        }
        let mut state = self.state.lock().expect("doctor state lock");
        state
            .tickets
            .retain(|_, used| used.open > 0 || used.expires_at >= now);
        if state.total_open >= MAX_CONNECTIONS {
            return Err("relay busy");
        }
        let first = !state.tickets.contains_key(&ticket.id);
        if first && state.tickets.values().filter(|used| used.open > 0).count() >= MAX_TESTS {
            return Err("relay busy");
        }
        let used = state.tickets.entry(ticket.id).or_insert(Use {
            open: 0,
            sent: 0,
            received: 0,
            expires_at: ticket.expires_at,
            started: Instant::now(),
        });
        if used.open >= MAX_CONNECTIONS_PER_TICKET {
            return Err("too many connections for one ticket");
        }
        used.open += 1;
        state.total_open += 1;
        *state.open.entry(ip).or_default() += 1;
        if first {
            self.metrics().doctor_tests.fetch_add(1, Ordering::Relaxed);
            tracing::info!(ticket = %hex_prefix(&ticket.id), "bufferbloat test started");
        }
        Ok((ticket, direction))
    }

    fn release(&self, ticket: &Ticket, ip: IpAddr) {
        let mut state = self.state.lock().expect("doctor state lock");
        state.total_open = state.total_open.saturating_sub(1);
        if let Some(count) = state.open.get_mut(&ip) {
            *count -= 1;
            if *count == 0 {
                state.open.remove(&ip);
                state.echoes.remove(&ip);
            }
        }
        if let Some(used) = state.tickets.get_mut(&ticket.id) {
            used.open -= 1;
            if used.open == 0 {
                tracing::info!(
                    ticket = %hex_prefix(&ticket.id),
                    sent = used.sent,
                    received = used.received,
                    secs = used.started.elapsed().as_secs(),
                    "bufferbloat test connections closed"
                );
            }
        }
    }

    /// Takes up to `want` bytes from the ticket's budget in one direction;
    /// 0 when it is used up or has expired.
    fn take(&self, ticket: &Ticket, want: usize, sending: bool) -> usize {
        if now_unix() >= ticket.expires_at {
            return 0;
        }
        let mut state = self.state.lock().expect("doctor state lock");
        let Some(used) = state.tickets.get_mut(&ticket.id) else {
            return 0;
        };
        let spent = if sending {
            &mut used.sent
        } else {
            &mut used.received
        };
        let allowed = ticket.max_bytes.saturating_sub(*spent).min(want as u64);
        *spent += allowed;
        allowed as usize
    }

    async fn send(&self, stream: &mut TcpStream, ticket: &Ticket) {
        loop {
            let n = self.take(ticket, CHUNK, true);
            if n == 0 {
                break;
            }
            match timeout(IDLE_TIMEOUT, stream.write_all(&self.filler[..n])).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => return,
            }
            self.metrics()
                .doctor_bytes_sent
                .fetch_add(n as u64, Ordering::Relaxed);
        }
        let _ = stream.shutdown().await;
    }

    async fn receive(&self, stream: &mut TcpStream, ticket: &Ticket) {
        let mut buffer = vec![0u8; CHUNK];
        loop {
            let n = match timeout(IDLE_TIMEOUT, stream.read(&mut buffer)).await {
                Ok(Ok(n)) if n > 0 => n,
                _ => break,
            };
            self.metrics()
                .doctor_bytes_received
                .fetch_add(n as u64, Ordering::Relaxed);
            if self.take(ticket, n, false) < n {
                break;
            }
        }
    }

    async fn echo(&self, socket: UdpSocket) {
        let mut buffer = [0u8; MAX_ECHO + 1];
        loop {
            let (n, from) = match socket.recv_from(&mut buffer).await {
                Ok(received) => received,
                Err(e) => {
                    // ICMP errors from earlier sends surface here on some
                    // systems; they say nothing about this socket.
                    tracing::debug!(error = %e, "doctor echo: receive failed");
                    continue;
                }
            };
            if n == 0 || n > MAX_ECHO || !self.may_echo(from.ip()) {
                continue;
            }
            let _ = socket.send_to(&buffer[..n], from).await;
        }
    }

    fn may_echo(&self, ip: IpAddr) -> bool {
        let mut state = self.state.lock().expect("doctor state lock");
        if !state.open.contains_key(&ip) {
            return false;
        }
        state
            .echoes
            .entry(ip)
            .or_insert_with(|| TokenBucket::new(ECHOES_PER_SECOND, ECHOES_PER_SECOND))
            .take(1)
    }
}

/// The length-prefixed ticket and the direction byte after it.
async fn read_request(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
    let len = usize::from(stream.read_u16().await?);
    if len != ticket::LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad ticket length",
        ));
    }
    let mut request = vec![0u8; len + 1];
    stream.read_exact(&mut request).await?;
    Ok(request)
}

/// The first bytes of a ticket ID, to tell tests apart in the log.
fn hex_prefix(id: &[u8]) -> String {
    id[..4].iter().map(|b| format!("{b:02x}")).collect()
}
