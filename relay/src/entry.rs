//! The entry role: forwards a client's frames to its exit and back, by
//! session ID, without holding any tunnel keys.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use pingpath_tunnel::frame::{self, COOKIE_LEN, Cookie, HelloStatus, SessionId, session_hex};
use pingpath_tunnel::token::{CLOCK_SKEW_SECS, Claims, now_unix};
use ring::hmac;

use crate::metrics::DropReason;
use crate::ratelimit::Limiter;
use crate::relay::Shared;

/// Cookies are valid for the current and the previous window.
const COOKIE_WINDOW_SECS: u64 = 30;

/// A client's address. It prints as `<client>` so it can never end up in a
/// log line or a metric by accident (PLAN.md rule 9); sending uses
/// [`ClientAddr::socket_addr`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ClientAddr(SocketAddr);

impl ClientAddr {
    pub fn socket_addr(self) -> SocketAddr {
        self.0
    }
}

impl fmt::Debug for ClientAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<client>")
    }
}

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<client>")
    }
}

#[derive(Debug)]
pub struct EntrySession {
    /// The client's current, verified address.
    pub client: ClientAddr,
    pub exit: SocketAddr,
    pub exit_id: u32,
    pub client_key: [u8; 32],
    pub expires_at: u64,
    /// When the exit last sent anything for this session.
    pub last_exit_traffic: Instant,
    pub up: Limiter,
    pub down: Limiter,
}

/// What the receive loop should send after an entry decision.
pub type Outgoing = Option<(Vec<u8>, SocketAddr)>;

impl Shared {
    /// The cookie for `from` in the given window: proof that a HELLO's
    /// sender receives traffic at its address, so a copied token can't be
    /// replayed from a spoofed one.
    fn cookie(&self, session: &SessionId, from: SocketAddr, window: u64) -> Cookie {
        let mut message = Vec::with_capacity(8 + 18 + 8);
        message.extend_from_slice(session);
        match from.ip() {
            IpAddr::V4(ip) => message.extend_from_slice(&ip.octets()),
            IpAddr::V6(ip) => message.extend_from_slice(&ip.octets()),
        }
        message.extend_from_slice(&from.port().to_be_bytes());
        message.extend_from_slice(&window.to_le_bytes());
        let tag = hmac::sign(&self.cookie_key, &message);
        tag.as_ref()[..COOKIE_LEN]
            .try_into()
            .expect("HMAC-SHA256 is 32 bytes")
    }

    pub(crate) fn cookie_is_valid(
        &self,
        session: &SessionId,
        from: SocketAddr,
        cookie: &Cookie,
    ) -> bool {
        let window = now_unix() / COOKIE_WINDOW_SECS;
        [window, window.saturating_sub(1)].iter().any(|w| {
            let expected = self.cookie(session, from, *w);
            // Constant time: every byte is compared whatever the first difference.
            expected
                .iter()
                .zip(cookie)
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                == 0
        })
    }

    /// A HELLO with a verified token whose exit is another relay.
    pub(crate) fn entry_hello(
        &self,
        claims: &Claims,
        datagram: &[u8],
        cookie: Option<Cookie>,
        from: SocketAddr,
    ) -> Outgoing {
        let session = &claims.session_id;
        let Some(&exit) = self.exits_by_id.get(&claims.exit_id) else {
            self.metrics.count_drop(DropReason::UnknownExit);
            return Some((frame::hello_ack(session, HelloStatus::WrongExit), from));
        };
        if !cookie.is_some_and(|c| self.cookie_is_valid(session, from, &c)) {
            // Prove the address first; the answer is smaller than the HELLO.
            let fresh = self.cookie(session, from, now_unix() / COOKIE_WINDOW_SECS);
            return Some((frame::hello_verify(session, &fresh), from));
        }

        let mut sessions = self.entry.lock().expect("entry table lock");
        match sessions.get_mut(session) {
            Some(existing) => {
                if existing.exit_id != claims.exit_id || existing.client_key != claims.client_key {
                    self.metrics.count_drop(DropReason::BadToken);
                    return Some((frame::hello_ack(session, HelloStatus::BadToken), from));
                }
                existing.client = ClientAddr(from);
                existing.expires_at = existing.expires_at.max(claims.expires_at);
                tracing::info!(session = %session_hex(session), exit = claims.exit_id, "entry session renewed");
            }
            None => {
                if sessions.len() >= self.config.limits.max_sessions {
                    self.metrics.count_drop(DropReason::RelayFull);
                    return Some((frame::hello_ack(session, HelloStatus::RelayFull), from));
                }
                sessions.insert(
                    *session,
                    EntrySession {
                        client: ClientAddr(from),
                        exit,
                        exit_id: claims.exit_id,
                        client_key: claims.client_key,
                        expires_at: claims.expires_at,
                        last_exit_traffic: Instant::now(),
                        up: self.limiter(),
                        down: self.limiter(),
                    },
                );
                self.metrics.entry_sessions.fetch_add(1, Ordering::Relaxed);
                self.metrics
                    .sessions_started
                    .fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    session = %session_hex(session),
                    exit = claims.exit_id,
                    game = %claims.game,
                    region = %claims.region,
                    "entry session started"
                );
            }
        }
        Some((datagram.to_vec(), exit))
    }

    /// A frame from one of the exits this relay forwards to: send it to the
    /// session's client, but only if it comes from that session's exit.
    pub(crate) fn entry_from_exit(
        &self,
        session: &SessionId,
        datagram: &[u8],
        from: SocketAddr,
    ) -> Outgoing {
        let mut sessions = self.entry.lock().expect("entry table lock");
        let Some(s) = sessions.get_mut(session).filter(|s| s.exit == from) else {
            self.metrics.count_drop(DropReason::UnknownSession);
            return None;
        };
        s.last_exit_traffic = Instant::now();
        if !s.down.allow(datagram.len()) {
            self.metrics.count_drop(DropReason::RateLimited);
            return None;
        }
        Some((datagram.to_vec(), s.client.socket_addr()))
    }

    /// DATA from a client: send it to the session's exit.
    pub(crate) fn entry_from_client(
        &self,
        session: &SessionId,
        datagram: &[u8],
        from: SocketAddr,
    ) -> Outgoing {
        let now = now_unix();
        let mut sessions = self.entry.lock().expect("entry table lock");
        let Some(s) = sessions.get_mut(session) else {
            self.metrics.count_drop(DropReason::UnknownSession);
            return None;
        };
        if s.client.socket_addr() != from {
            self.metrics.count_drop(DropReason::WrongSource);
            return None;
        }
        if now >= s.expires_at.saturating_add(CLOCK_SKEW_SECS) {
            self.metrics.count_drop(DropReason::ExpiredToken);
            return None;
        }
        if !s.up.allow(datagram.len()) {
            self.metrics.count_drop(DropReason::RateLimited);
            return None;
        }
        Some((datagram.to_vec(), s.exit))
    }

    /// Removes entry sessions whose token has run out, or whose exit has
    /// gone quiet (the client's keepalives would have been answered).
    pub(crate) fn expire_entry_sessions(&self, now: u64) {
        let idle = Duration::from_secs(self.config.limits.entry_idle_secs);
        let mut sessions = self.entry.lock().expect("entry table lock");
        let before = sessions.len();
        sessions.retain(|id, s| {
            let expired = now >= s.expires_at.saturating_add(CLOCK_SKEW_SECS);
            let quiet = s.last_exit_traffic.elapsed() >= idle;
            if expired || quiet {
                let reason = if expired {
                    "token expired"
                } else {
                    "no traffic from the exit"
                };
                tracing::info!(session = %session_hex(id), reason, "entry session ended");
            }
            !(expired || quiet)
        });
        let removed = (before - sessions.len()) as i64;
        if removed > 0 {
            self.metrics
                .entry_sessions
                .fetch_sub(removed, Ordering::Relaxed);
            self.metrics
                .sessions_ended
                .fetch_add(removed as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn limiter(&self) -> Limiter {
        Limiter::new(
            self.config.limits.packets_per_second,
            self.config.limits.bits_per_second,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_addresses_never_print() {
        let addr = ClientAddr("203.0.113.9:4444".parse().unwrap());
        assert_eq!(format!("{addr} {addr:?}"), "<client> <client>");
    }
}
