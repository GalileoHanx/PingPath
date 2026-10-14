//! Doctor tickets: what lets a client draw a bufferbloat test's load from
//! a relay (docs/decisions/011-home-network-doctor.md). Signed by the
//! control plane with the key that signs session tokens, under a signing
//! context of their own so neither can pass for the other, and checked
//! offline by the relay.

use ring::signature::{ED25519, UnparsedPublicKey};

use crate::token::{CLOCK_SKEW_SECS, Issuer, SIGNATURE_LEN, TokenError, Verifier};

pub const VERSION: u8 = 1;
/// Signed before the ticket's bytes (a session token's context differs).
const CONTEXT: &[u8] = b"pingpath-doctor-ticket-v1";
pub const ID_LEN: usize = 16;
/// version, id, relay_id, issued_at, expires_at, max_bytes
const UNSIGNED_LEN: usize = 1 + ID_LEN + 4 + 8 + 8 + 8;
/// Every ticket is this long.
pub const LEN: usize = UNSIGNED_LEN + SIGNATURE_LEN;

/// The doctor listener's protocol (relay/src/doctor.rs): after the
/// length-prefixed ticket, the client sends one of these...
pub const DOWN: u8 = b'D';
pub const UP: u8 = b'U';
/// ...and the relay answers one of these.
pub const ACCEPTED: u8 = 0;
pub const REFUSED: u8 = 1;
/// The largest ping its UDP echo sends back.
pub const MAX_ECHO: usize = 64;

/// What a ticket grants: load from one relay, for a short time, up to a
/// number of bytes each way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    /// Random; the relay counts a ticket's use by it.
    pub id: [u8; ID_LEN],
    /// The one relay that honours it.
    pub relay_id: u32,
    pub issued_at: u64,
    pub expires_at: u64,
    /// Bytes the relay may send, and may take, under it.
    pub max_bytes: u64,
}

impl Ticket {
    fn unsigned_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(LEN);
        bytes.push(VERSION);
        bytes.extend_from_slice(&self.id);
        bytes.extend_from_slice(&self.relay_id.to_le_bytes());
        bytes.extend_from_slice(&self.issued_at.to_le_bytes());
        bytes.extend_from_slice(&self.expires_at.to_le_bytes());
        bytes.extend_from_slice(&self.max_bytes.to_le_bytes());
        bytes
    }

    /// The ticket's claims without checking the signature.
    pub fn decode_unverified(bytes: &[u8]) -> Result<Self, TokenError> {
        let &version = bytes.first().ok_or(TokenError::Malformed)?;
        if version != VERSION {
            return Err(TokenError::UnsupportedVersion(version));
        }
        if bytes.len() != LEN {
            return Err(TokenError::Malformed);
        }
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        let u64_at = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        Ok(Self {
            id: bytes[1..1 + ID_LEN].try_into().expect("16 bytes"),
            relay_id: u32_at(17),
            issued_at: u64_at(21),
            expires_at: u64_at(29),
            max_bytes: u64_at(37),
        })
    }
}

fn message(unsigned: &[u8]) -> Vec<u8> {
    [CONTEXT, unsigned].concat()
}

impl Issuer {
    pub fn sign_ticket(&self, ticket: &Ticket) -> Vec<u8> {
        let mut bytes = ticket.unsigned_bytes();
        let signature = self.key_pair.sign(&message(&bytes));
        bytes.extend_from_slice(signature.as_ref());
        bytes
    }
}

impl Verifier {
    /// Checks a ticket's signature against every trusted key, and its
    /// validity window (with [`CLOCK_SKEW_SECS`] of slack) at `now`.
    pub fn verify_ticket(&self, bytes: &[u8], now: u64) -> Result<Ticket, TokenError> {
        let ticket = Ticket::decode_unverified(bytes)?;
        let (unsigned, signature) = bytes.split_at(UNSIGNED_LEN);
        let message = message(unsigned);
        let trusted = self.keys.iter().any(|key| {
            UnparsedPublicKey::new(&ED25519, key)
                .verify(&message, signature)
                .is_ok()
        });
        if !trusted {
            return Err(TokenError::BadSignature);
        }
        if now.saturating_add(CLOCK_SKEW_SECS) < ticket.issued_at {
            return Err(TokenError::NotYetValid);
        }
        if now >= ticket.expires_at.saturating_add(CLOCK_SKEW_SECS) {
            return Err(TokenError::Expired);
        }
        Ok(ticket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::Claims;

    fn ticket() -> Ticket {
        Ticket {
            id: [5; ID_LEN],
            relay_id: 7,
            issued_at: 1_000,
            expires_at: 1_060,
            max_bytes: 250_000_000,
        }
    }

    fn issuer(seed: u8) -> Issuer {
        Issuer::from_seed(&[seed; 32]).unwrap()
    }

    #[test]
    fn signs_and_verifies() {
        let (signer, other) = (issuer(1), issuer(2));
        let bytes = signer.sign_ticket(&ticket());
        assert_eq!(bytes.len(), LEN);
        let verifier = Verifier::new(vec![other.public_key(), signer.public_key()]);
        assert_eq!(verifier.verify_ticket(&bytes, 1_010), Ok(ticket()));
        assert_eq!(Ticket::decode_unverified(&bytes), Ok(ticket()));
    }

    #[test]
    fn refuses_other_keys_tampering_and_the_wrong_time() {
        let bytes = issuer(1).sign_ticket(&ticket());
        let verifier = Verifier::new(vec![issuer(1).public_key()]);
        assert_eq!(
            Verifier::new(vec![issuer(2).public_key()]).verify_ticket(&bytes, 1_010),
            Err(TokenError::BadSignature)
        );
        let mut more = bytes.clone();
        // A bigger byte budget.
        more[44] ^= 1;
        assert_eq!(
            verifier.verify_ticket(&more, 1_010),
            Err(TokenError::BadSignature)
        );
        assert_eq!(
            verifier.verify_ticket(&bytes[..LEN - 1], 1_010),
            Err(TokenError::Malformed)
        );
        assert_eq!(
            verifier.verify_ticket(&bytes, 1_000 - CLOCK_SKEW_SECS - 1),
            Err(TokenError::NotYetValid)
        );
        assert_eq!(
            verifier.verify_ticket(&bytes, 1_060 + CLOCK_SKEW_SECS),
            Err(TokenError::Expired)
        );
        let mut version = bytes;
        version[0] = 9;
        assert_eq!(
            verifier.verify_ticket(&version, 1_010),
            Err(TokenError::UnsupportedVersion(9))
        );
    }

    /// The same key signs both, so each must fail as the other.
    #[test]
    fn a_ticket_is_not_a_session_token_nor_the_reverse() {
        let issuer = issuer(1);
        let verifier = Verifier::new(vec![issuer.public_key()]);
        let ticket = issuer.sign_ticket(&ticket());
        assert!(verifier.verify(&ticket, 1_010).is_err());
        let token = issuer
            .sign(&Claims {
                session_id: [1; 8],
                client_key: [2; 32],
                exit_id: 7,
                issued_at: 1_000,
                expires_at: 1_060,
                game: "g".into(),
                region: "r".into(),
            })
            .unwrap();
        assert!(verifier.verify_ticket(&token, 1_010).is_err());
        // Even when the bytes after the version happen to be the right
        // length: the signing context differs.
        let mut forged = token.clone();
        forged.resize(LEN, 0);
        assert!(verifier.verify_ticket(&forged, 1_010).is_err());
    }
}
