//! Keys in their text form: standard base64 of the raw 32 bytes, like
//! WireGuard keys. See the key table in docs/decisions/004-relay-wire-protocol.md.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use boringtun::x25519::{PublicKey, StaticSecret};
use ring::rand::{SecureRandom, SystemRandom};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum KeyError {
    #[error("key is not valid base64")]
    Base64,
    #[error("key must be 32 bytes, found {0}")]
    Length(usize),
    #[error("the system random number generator failed")]
    Random,
}

/// 32 bytes from the operating system's secure random number generator.
pub fn random_32() -> Result<[u8; 32], KeyError> {
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| KeyError::Random)?;
    Ok(bytes)
}

pub fn encode(bytes: &[u8; 32]) -> String {
    STANDARD.encode(bytes)
}

/// Decodes a base64 key, ignoring surrounding whitespace (key files end in
/// a newline).
pub fn decode(text: &str) -> Result<[u8; 32], KeyError> {
    let bytes = STANDARD.decode(text.trim()).map_err(|_| KeyError::Base64)?;
    let len = bytes.len();
    bytes.try_into().map_err(|_| KeyError::Length(len))
}

/// A WireGuard (X25519) key pair.
pub struct WireGuardKeys {
    pub secret: StaticSecret,
    pub public: PublicKey,
}

impl WireGuardKeys {
    pub fn generate() -> Result<Self, KeyError> {
        Ok(Self::from_secret(random_32()?))
    }

    pub fn from_secret(bytes: [u8; 32]) -> Self {
        let secret = StaticSecret::from(bytes);
        let public = PublicKey::from(&secret);
        Self { secret, public }
    }

    pub fn from_base64(secret: &str) -> Result<Self, KeyError> {
        decode(secret).map(Self::from_secret)
    }

    pub fn secret_base64(&self) -> String {
        encode(&self.secret.to_bytes())
    }

    pub fn public_base64(&self) -> String {
        encode(self.public.as_bytes())
    }
}

pub fn public_key_from_base64(text: &str) -> Result<PublicKey, KeyError> {
    decode(text).map(PublicKey::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_matches_wireguard() {
        // RFC 7748 section 6.1 test vector (Alice).
        let secret = decode("dwdtCnMYpX08FsFyUbJmRd9ML4frwJkqsXf7pR25LCo=").unwrap();
        let keys = WireGuardKeys::from_secret(secret);
        assert_eq!(
            keys.public_base64(),
            "hSDwCYkwp1R0i33ctD73Wg2/Og0mOBr066SpjqqbTmo="
        );
        assert_eq!(
            WireGuardKeys::from_base64(&format!("{}\n", keys.secret_base64()))
                .unwrap()
                .public_base64(),
            keys.public_base64()
        );
    }

    #[test]
    fn rejects_bad_keys() {
        assert_eq!(decode("not base64!").err(), Some(KeyError::Base64));
        assert_eq!(decode("AAAA").err(), Some(KeyError::Length(3)));
    }

    #[test]
    fn generated_keys_differ() {
        let a = WireGuardKeys::generate().unwrap();
        let b = WireGuardKeys::generate().unwrap();
        assert_ne!(a.public_base64(), b.public_base64());
    }
}
