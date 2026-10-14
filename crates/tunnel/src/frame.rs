//! Frames: the UDP datagrams exchanged between clients, entry relays and
//! exit relays (docs/decisions/004-relay-wire-protocol.md).

use thiserror::Error;

pub const MAGIC: u8 = 0x50; // 'P'
pub const SESSION_ID_LEN: usize = 8;
/// Magic, type and session ID.
pub const SESSION_HEADER_LEN: usize = 2 + SESSION_ID_LEN;
/// PROBE bodies are the 8-byte nonce plus at most 56 bytes of padding.
pub const MAX_PROBE_BODY: usize = 64;
/// An entry relay's return-routability cookie (HELLO_ACK `Verify`).
pub const COOKIE_LEN: usize = 16;

/// The largest IP packet carried inside the tunnel. With the 10-byte frame
/// header, WireGuard's 32 bytes and IPv6 outer headers (48 bytes) a full
/// packet is 1470 bytes, which fits a 1492-byte PPPoE link.
pub const TUNNEL_MTU: usize = 1380;
/// The largest WireGuard message a relay accepts in a DATA frame.
pub const MAX_WG_MESSAGE: usize = TUNNEL_MTU + 32;
/// The largest frame a relay accepts.
pub const MAX_FRAME: usize = SESSION_HEADER_LEN + MAX_WG_MESSAGE;

pub type SessionId = [u8; SESSION_ID_LEN];
pub type Cookie = [u8; COOKIE_LEN];

/// Shows a session ID the way logs and metrics print it.
pub fn session_hex(id: &SessionId) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Hello = 1,
    HelloAck = 2,
    Data = 3,
    Probe = 4,
    ProbeReply = 5,
}

/// HELLO_ACK status codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HelloStatus {
    /// Accepted; the exit already has this session's tunnel.
    Ok = 0,
    /// The token verified but doesn't match the session it names.
    BadToken = 1,
    Expired = 2,
    WrongExit = 3,
    GameNotRoutable = 4,
    RelayFull = 5,
    RateLimited = 6,
    /// Accepted, with no tunnel yet (new session, or the exit restarted):
    /// the client must start a WireGuard handshake now.
    OkNewSession = 7,
    /// The exit doesn't serve the token's region (rule 3).
    WrongRegion = 8,
    /// An entry relay asks the client to send HELLO again with the cookie
    /// in this frame, proving it receives traffic at its address.
    Verify = 9,
}

impl HelloStatus {
    fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Ok,
            1 => Self::BadToken,
            2 => Self::Expired,
            3 => Self::WrongExit,
            4 => Self::GameNotRoutable,
            5 => Self::RelayFull,
            6 => Self::RateLimited,
            7 => Self::OkNewSession,
            8 => Self::WrongRegion,
            9 => Self::Verify,
            _ => return None,
        })
    }

    pub fn is_ok(self) -> bool {
        matches!(self, Self::Ok | Self::OkNewSession)
    }
}

/// A parsed frame, borrowing from the datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    Hello {
        session: SessionId,
        token: &'a [u8],
        cookie: Option<Cookie>,
    },
    HelloAck {
        session: SessionId,
        status: HelloStatus,
        cookie: Option<Cookie>,
    },
    Data {
        session: SessionId,
        message: &'a [u8],
    },
    Probe {
        body: &'a [u8],
    },
    ProbeReply {
        body: &'a [u8],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FrameError {
    #[error("not a PingPath frame")]
    BadMagic,
    #[error("frame too short or malformed")]
    Truncated,
    #[error("unknown frame type {0}")]
    UnknownType(u8),
    #[error("unknown HELLO_ACK status {0}")]
    UnknownStatus(u8),
    #[error("PROBE body must be 8 to {MAX_PROBE_BODY} bytes")]
    BadProbe,
}

impl<'a> Frame<'a> {
    pub fn parse(datagram: &'a [u8]) -> Result<Self, FrameError> {
        let (&magic, rest) = datagram.split_first().ok_or(FrameError::Truncated)?;
        if magic != MAGIC {
            return Err(FrameError::BadMagic);
        }
        let (&kind, body) = rest.split_first().ok_or(FrameError::Truncated)?;
        let session = || -> Result<(SessionId, &'a [u8]), FrameError> {
            if body.len() < SESSION_ID_LEN {
                return Err(FrameError::Truncated);
            }
            let (id, rest) = body.split_at(SESSION_ID_LEN);
            Ok((id.try_into().expect("length checked"), rest))
        };
        let cookie = |bytes: &[u8]| -> Result<Option<Cookie>, FrameError> {
            match bytes.len() {
                0 => Ok(None),
                COOKIE_LEN => Ok(Some(bytes.try_into().expect("length checked"))),
                _ => Err(FrameError::Truncated),
            }
        };
        match kind {
            1 => {
                let (session, rest) = session()?;
                let token_len = crate::token::encoded_len(rest).ok_or(FrameError::Truncated)?;
                let (token, trailer) = rest.split_at(token_len);
                Ok(Frame::Hello {
                    session,
                    token,
                    cookie: cookie(trailer)?,
                })
            }
            2 => {
                let (session, rest) = session()?;
                let (&status, trailer) = rest.split_first().ok_or(FrameError::Truncated)?;
                let status =
                    HelloStatus::from_u8(status).ok_or(FrameError::UnknownStatus(status))?;
                let cookie = cookie(trailer)?;
                if cookie.is_some() != (status == HelloStatus::Verify) {
                    return Err(FrameError::Truncated);
                }
                Ok(Frame::HelloAck {
                    session,
                    status,
                    cookie,
                })
            }
            3 => {
                let (session, message) = session()?;
                Ok(Frame::Data { session, message })
            }
            4 | 5 => {
                if !(8..=MAX_PROBE_BODY).contains(&body.len()) {
                    return Err(FrameError::BadProbe);
                }
                Ok(if kind == 4 {
                    Frame::Probe { body }
                } else {
                    Frame::ProbeReply { body }
                })
            }
            other => Err(FrameError::UnknownType(other)),
        }
    }

    /// The session a frame belongs to, if any.
    pub fn session(&self) -> Option<SessionId> {
        match self {
            Frame::Hello { session, .. }
            | Frame::HelloAck { session, .. }
            | Frame::Data { session, .. } => Some(*session),
            Frame::Probe { .. } | Frame::ProbeReply { .. } => None,
        }
    }
}

fn header(kind: FrameType, session: Option<&SessionId>, extra: usize) -> Vec<u8> {
    let mut frame = Vec::with_capacity(SESSION_HEADER_LEN + extra);
    frame.push(MAGIC);
    frame.push(kind as u8);
    if let Some(session) = session {
        frame.extend_from_slice(session);
    }
    frame
}

/// A HELLO, with the entry's cookie when answering a `Verify`.
pub fn hello(session: &SessionId, token: &[u8], cookie: Option<&Cookie>) -> Vec<u8> {
    let mut frame = header(FrameType::Hello, Some(session), token.len() + COOKIE_LEN);
    frame.extend_from_slice(token);
    if let Some(cookie) = cookie {
        frame.extend_from_slice(cookie);
    }
    frame
}

pub fn hello_ack(session: &SessionId, status: HelloStatus) -> Vec<u8> {
    debug_assert!(status != HelloStatus::Verify, "use hello_verify");
    let mut frame = header(FrameType::HelloAck, Some(session), 1);
    frame.push(status as u8);
    frame
}

/// HELLO_ACK `Verify` carrying the cookie the client must echo.
pub fn hello_verify(session: &SessionId, cookie: &Cookie) -> Vec<u8> {
    let mut frame = header(FrameType::HelloAck, Some(session), 1 + COOKIE_LEN);
    frame.push(HelloStatus::Verify as u8);
    frame.extend_from_slice(cookie);
    frame
}

/// Whether `datagram` is a DATA frame carrying a WireGuard data message
/// (not a handshake, cookie or other frame).
pub fn is_wg_data(datagram: &[u8]) -> bool {
    matches!(Frame::parse(datagram), Ok(Frame::Data { message, .. }) if message.first() == Some(&4))
}

pub fn data(session: &SessionId, message: &[u8]) -> Vec<u8> {
    let mut frame = header(FrameType::Data, Some(session), message.len());
    frame.extend_from_slice(message);
    frame
}

/// A PROBE with an 8-byte nonce and up to 56 bytes of zero padding.
pub fn probe(nonce: u64, padding: usize) -> Vec<u8> {
    let padding = padding.min(MAX_PROBE_BODY - 8);
    let mut frame = header(FrameType::Probe, None, 8 + padding);
    frame.extend_from_slice(&nonce.to_le_bytes());
    frame.resize(frame.len() + padding, 0);
    frame
}

/// The answer to a PROBE: the same body, so never larger than the request.
pub fn probe_reply(body: &[u8]) -> Vec<u8> {
    let mut frame = header(FrameType::ProbeReply, None, body.len());
    frame.extend_from_slice(body);
    frame
}

/// The nonce at the start of a PROBE or PROBE_REPLY body.
pub fn probe_nonce(body: &[u8]) -> Option<u64> {
    body.get(..8)
        .map(|n| u64::from_le_bytes(n.try_into().expect("8 bytes")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{Claims, Issuer};

    const ID: SessionId = [1, 2, 3, 4, 5, 6, 7, 8];

    fn token() -> Vec<u8> {
        Issuer::from_seed(&[3; 32])
            .unwrap()
            .sign(&Claims {
                session_id: ID,
                client_key: [0; 32],
                exit_id: 1,
                issued_at: 0,
                expires_at: 1,
                game: "g".into(),
                region: "r".into(),
            })
            .unwrap()
    }

    #[test]
    fn round_trips_every_frame() {
        let token = token();
        assert_eq!(
            Frame::parse(&hello(&ID, &token, None)),
            Ok(Frame::Hello {
                session: ID,
                token: &token,
                cookie: None
            })
        );
        let cookie = [9; COOKIE_LEN];
        assert_eq!(
            Frame::parse(&hello(&ID, &token, Some(&cookie))),
            Ok(Frame::Hello {
                session: ID,
                token: &token,
                cookie: Some(cookie)
            })
        );
        assert_eq!(
            Frame::parse(&hello_ack(&ID, HelloStatus::Expired)),
            Ok(Frame::HelloAck {
                session: ID,
                status: HelloStatus::Expired,
                cookie: None
            })
        );
        assert_eq!(
            Frame::parse(&hello_verify(&ID, &cookie)),
            Ok(Frame::HelloAck {
                session: ID,
                status: HelloStatus::Verify,
                cookie: Some(cookie)
            })
        );
        assert_eq!(
            Frame::parse(&data(&ID, b"wg")),
            Ok(Frame::Data {
                session: ID,
                message: b"wg"
            })
        );
        let p = probe(42, 100);
        let Ok(Frame::Probe { body }) = Frame::parse(&p) else {
            panic!("not a probe")
        };
        assert_eq!(probe_nonce(body), Some(42));
        assert_eq!(body.len(), MAX_PROBE_BODY, "padding is capped");
        let reply = probe_reply(body);
        assert_eq!(
            reply.len(),
            p.len(),
            "a reply is never larger than the request"
        );
        assert_eq!(Frame::parse(&reply), Ok(Frame::ProbeReply { body }));
    }

    #[test]
    fn rejects_malformed_frames() {
        assert_eq!(Frame::parse(&[]), Err(FrameError::Truncated));
        assert_eq!(Frame::parse(&[0x51, 3]), Err(FrameError::BadMagic));
        assert_eq!(Frame::parse(&[MAGIC]), Err(FrameError::Truncated));
        assert_eq!(Frame::parse(&[MAGIC, 9]), Err(FrameError::UnknownType(9)));
        assert_eq!(Frame::parse(&[MAGIC, 3, 1, 2]), Err(FrameError::Truncated));
        assert_eq!(
            Frame::parse(&[MAGIC, 2, 1, 2, 3, 4, 5, 6, 7, 8, 99]),
            Err(FrameError::UnknownStatus(99))
        );
        assert_eq!(
            Frame::parse(&[MAGIC, 4, 1, 2, 3]),
            Err(FrameError::BadProbe)
        );
        // A HELLO whose token is cut short, or has a stray trailer.
        let token = token();
        let mut short = hello(&ID, &token, None);
        short.pop();
        assert_eq!(Frame::parse(&short), Err(FrameError::Truncated));
        let mut trailer = hello(&ID, &token, None);
        trailer.extend_from_slice(&[0; 5]);
        assert_eq!(Frame::parse(&trailer), Err(FrameError::Truncated));
        // Verify must carry a cookie and nothing else may.
        let mut ok_with_cookie = hello_ack(&ID, HelloStatus::Ok);
        ok_with_cookie.extend_from_slice(&[0; COOKIE_LEN]);
        assert_eq!(Frame::parse(&ok_with_cookie), Err(FrameError::Truncated));
    }

    #[test]
    fn no_answer_is_larger_than_the_hello_it_answers() {
        let shortest_hello = hello(&ID, &[0; crate::token::MIN_LEN], None);
        assert!(hello_ack(&ID, HelloStatus::Ok).len() < shortest_hello.len());
        assert!(hello_verify(&ID, &[0; COOKIE_LEN]).len() < shortest_hello.len());
    }
}
