//! Session tokens: signed by the control plane, checked offline by every
//! relay (docs/decisions/004-relay-wire-protocol.md, PLAN.md rule 10).

use std::time::{SystemTime, UNIX_EPOCH};

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use thiserror::Error;

use crate::frame::{SESSION_ID_LEN, SessionId};

pub const VERSION: u8 = 1;
/// Signed before the token bytes, so a token signature can't be confused
/// with anything else the issuer key signs.
const CONTEXT: &[u8] = b"pingpath-session-token-v1";
pub const MAX_GAME_LEN: usize = 64;
pub const MAX_REGION_LEN: usize = 64;
pub const SIGNATURE_LEN: usize = 64;
/// version, session_id, client_key, exit_id, issued_at, expires_at
const FIXED_LEN: usize = 1 + SESSION_ID_LEN + 32 + 4 + 8 + 8;
/// The shortest valid token: one-character game and region IDs.
pub const MIN_LEN: usize = FIXED_LEN + 2 + 2 + SIGNATURE_LEN;
pub const MAX_LEN: usize = FIXED_LEN + 2 + MAX_GAME_LEN + MAX_REGION_LEN + SIGNATURE_LEN;
/// Tolerated clock difference between the control plane and a relay.
pub const CLOCK_SKEW_SECS: u64 = 60;

/// What a token grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub session_id: SessionId,
    /// The client's WireGuard public key: the only peer the exit accepts.
    pub client_key: [u8; 32],
    /// The one exit relay this session may use.
    pub exit_id: u32,
    pub issued_at: u64,
    pub expires_at: u64,
    /// The one game whose servers the exit may forward to.
    pub game: String,
    /// The one region of that game (rule 3: the exit serves its own region).
    pub region: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TokenError {
    #[error("malformed token")]
    Malformed,
    #[error("unsupported token version {0}")]
    UnsupportedVersion(u8),
    #[error("game and region IDs must be 1 to 64 bytes")]
    BadGame,
    #[error("signature does not verify")]
    BadSignature,
    #[error("token is not valid yet")]
    NotYetValid,
    #[error("token has expired")]
    Expired,
    #[error("invalid issuer key")]
    BadKey,
}

impl Claims {
    fn unsigned_bytes(&self) -> Result<Vec<u8>, TokenError> {
        let (game, region) = (self.game.as_bytes(), self.region.as_bytes());
        if game.is_empty()
            || game.len() > MAX_GAME_LEN
            || region.is_empty()
            || region.len() > MAX_REGION_LEN
        {
            return Err(TokenError::BadGame);
        }
        let mut bytes =
            Vec::with_capacity(FIXED_LEN + 2 + game.len() + region.len() + SIGNATURE_LEN);
        bytes.push(VERSION);
        bytes.extend_from_slice(&self.session_id);
        bytes.extend_from_slice(&self.client_key);
        bytes.extend_from_slice(&self.exit_id.to_le_bytes());
        bytes.extend_from_slice(&self.issued_at.to_le_bytes());
        bytes.extend_from_slice(&self.expires_at.to_le_bytes());
        bytes.push(game.len() as u8);
        bytes.extend_from_slice(game);
        bytes.push(region.len() as u8);
        bytes.extend_from_slice(region);
        Ok(bytes)
    }
}

/// Splits a token into its claims and signed part without checking the
/// signature. Clients use it to read their token's expiry.
pub fn decode_unverified(token: &[u8]) -> Result<Claims, TokenError> {
    split(token).map(|(claims, _, _)| claims)
}

/// The length of the token at the start of `bytes` (a HELLO body may carry
/// a cookie after it), or `None` if it can't be a token.
pub fn encoded_len(bytes: &[u8]) -> Option<usize> {
    let game_len = usize::from(*bytes.get(FIXED_LEN)?);
    let region_len = usize::from(*bytes.get(FIXED_LEN + 1 + game_len)?);
    let len = FIXED_LEN + 2 + game_len + region_len + SIGNATURE_LEN;
    (len <= bytes.len()).then_some(len)
}

/// Returns the claims, the bytes covered by the signature, and the signature.
fn split(token: &[u8]) -> Result<(Claims, &[u8], &[u8]), TokenError> {
    let &version = token.first().ok_or(TokenError::Malformed)?;
    if version != VERSION {
        return Err(TokenError::UnsupportedVersion(version));
    }
    if token.len() < MIN_LEN || token.len() > MAX_LEN || encoded_len(token) != Some(token.len()) {
        return Err(TokenError::Malformed);
    }
    let game_len = usize::from(token[FIXED_LEN]);
    let region_at = FIXED_LEN + 1 + game_len;
    let region_len = usize::from(token[region_at]);
    if game_len == 0 || game_len > MAX_GAME_LEN || region_len == 0 || region_len > MAX_REGION_LEN {
        return Err(TokenError::Malformed);
    }
    let signed_len = region_at + 1 + region_len;
    let signed = &token[..signed_len];
    let signature = &token[signed_len..];
    let u32_at = |at: usize| u32::from_le_bytes(token[at..at + 4].try_into().expect("4 bytes"));
    let u64_at = |at: usize| u64::from_le_bytes(token[at..at + 8].try_into().expect("8 bytes"));
    let text = |from: usize, len: usize| {
        std::str::from_utf8(&token[from..from + len])
            .map(str::to_string)
            .map_err(|_| TokenError::Malformed)
    };
    let game = text(FIXED_LEN + 1, game_len)?;
    let region = text(region_at + 1, region_len)?;
    let claims = Claims {
        session_id: token[1..9].try_into().expect("8 bytes"),
        client_key: token[9..41].try_into().expect("32 bytes"),
        exit_id: u32_at(41),
        issued_at: u64_at(45),
        expires_at: u64_at(53),
        game,
        region,
    };
    Ok((claims, signed, signature))
}

fn message(signed: &[u8]) -> Vec<u8> {
    [CONTEXT, signed].concat()
}

/// The control plane's signing side.
pub struct Issuer {
    key_pair: Ed25519KeyPair,
}

impl Issuer {
    /// From the 32-byte Ed25519 seed (the private key).
    pub fn from_seed(seed: &[u8; 32]) -> Result<Self, TokenError> {
        Ed25519KeyPair::from_seed_unchecked(seed)
            .map(|key_pair| Self { key_pair })
            .map_err(|_| TokenError::BadKey)
    }

    /// The public key relays verify with.
    pub fn public_key(&self) -> [u8; 32] {
        self.key_pair
            .public_key()
            .as_ref()
            .try_into()
            .expect("Ed25519 public keys are 32 bytes")
    }

    pub fn sign(&self, claims: &Claims) -> Result<Vec<u8>, TokenError> {
        let mut token = claims.unsigned_bytes()?;
        let signature = self.key_pair.sign(&message(&token));
        token.extend_from_slice(signature.as_ref());
        Ok(token)
    }
}

/// The relays' checking side. Holds every issuer key currently trusted, so
/// keys can be rotated without downtime.
#[derive(Debug, Clone)]
pub struct Verifier {
    keys: Vec<[u8; 32]>,
}

impl Verifier {
    pub fn new(keys: Vec<[u8; 32]>) -> Self {
        Self { keys }
    }

    /// Checks the signature against every trusted key and the validity
    /// window (with [`CLOCK_SKEW_SECS`] of slack) at `now` (Unix seconds).
    pub fn verify(&self, token: &[u8], now: u64) -> Result<Claims, TokenError> {
        let (claims, signed, signature) = split(token)?;
        let message = message(signed);
        let trusted = self.keys.iter().any(|key| {
            UnparsedPublicKey::new(&ED25519, key)
                .verify(&message, signature)
                .is_ok()
        });
        if !trusted {
            return Err(TokenError::BadSignature);
        }
        if now.saturating_add(CLOCK_SKEW_SECS) < claims.issued_at {
            return Err(TokenError::NotYetValid);
        }
        if now >= claims.expires_at.saturating_add(CLOCK_SKEW_SECS) {
            return Err(TokenError::Expired);
        }
        Ok(claims)
    }
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        Claims {
            session_id: [9; 8],
            client_key: [7; 32],
            exit_id: 3,
            issued_at: 1_000,
            expires_at: 1_900,
            game: "counter-strike-2".into(),
            region: "eu-central".into(),
        }
    }

    fn issuer(seed: u8) -> Issuer {
        Issuer::from_seed(&[seed; 32]).unwrap()
    }

    #[test]
    fn signs_and_verifies() {
        let issuer = issuer(1);
        let token = issuer.sign(&claims()).unwrap();
        assert_eq!(
            token.len(),
            FIXED_LEN + 2 + "counter-strike-2".len() + "eu-central".len() + SIGNATURE_LEN
        );
        assert_eq!(
            encoded_len(&[token.clone(), vec![0; 16]].concat()),
            Some(token.len())
        );
        let verifier = Verifier::new(vec![issuer.public_key()]);
        assert_eq!(verifier.verify(&token, 1_500), Ok(claims()));
        assert_eq!(decode_unverified(&token), Ok(claims()));
    }

    #[test]
    fn accepts_any_trusted_key_for_rotation() {
        let (old, new) = (issuer(1), issuer(2));
        let verifier = Verifier::new(vec![old.public_key(), new.public_key()]);
        assert!(
            verifier
                .verify(&old.sign(&claims()).unwrap(), 1_500)
                .is_ok()
        );
        assert!(
            verifier
                .verify(&new.sign(&claims()).unwrap(), 1_500)
                .is_ok()
        );
    }

    #[test]
    fn rejects_forged_or_tampered_tokens() {
        let verifier = Verifier::new(vec![issuer(1).public_key()]);
        assert_eq!(
            verifier.verify(&issuer(2).sign(&claims()).unwrap(), 1_500),
            Err(TokenError::BadSignature)
        );

        let token = issuer(1).sign(&claims()).unwrap();
        for i in 1..token.len() {
            let mut tampered = token.clone();
            tampered[i] ^= 0x01;
            assert!(
                verifier.verify(&tampered, 1_500).is_err(),
                "byte {i} flipped but still verified"
            );
        }
        assert_eq!(
            verifier.verify(&token[..token.len() - 1], 1_500),
            Err(TokenError::Malformed)
        );
        let mut other_version = token.clone();
        other_version[0] = 2;
        assert_eq!(
            verifier.verify(&other_version, 1_500),
            Err(TokenError::UnsupportedVersion(2))
        );
    }

    #[test]
    fn enforces_the_validity_window_with_skew() {
        let issuer = issuer(1);
        let verifier = Verifier::new(vec![issuer.public_key()]);
        let token = issuer.sign(&claims()).unwrap();
        assert_eq!(
            verifier.verify(&token, 1_000 - CLOCK_SKEW_SECS - 1),
            Err(TokenError::NotYetValid)
        );
        assert!(verifier.verify(&token, 1_000 - CLOCK_SKEW_SECS).is_ok());
        assert!(verifier.verify(&token, 1_900 + CLOCK_SKEW_SECS - 1).is_ok());
        assert_eq!(
            verifier.verify(&token, 1_900 + CLOCK_SKEW_SECS),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn game_id_length_is_bounded() {
        let issuer = issuer(1);
        assert_eq!(
            issuer.sign(&Claims {
                game: String::new(),
                ..claims()
            }),
            Err(TokenError::BadGame)
        );
        assert_eq!(
            issuer.sign(&Claims {
                game: "g".repeat(65),
                ..claims()
            }),
            Err(TokenError::BadGame)
        );
        assert_eq!(
            issuer.sign(&Claims {
                region: String::new(),
                ..claims()
            }),
            Err(TokenError::BadGame)
        );
        let shortest = issuer
            .sign(&Claims {
                game: "g".into(),
                region: "r".into(),
                ..claims()
            })
            .unwrap();
        assert_eq!(shortest.len(), MIN_LEN);
    }
}
