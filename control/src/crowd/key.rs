//! The crowd key (docs/decisions/012-crowd-data-and-dashboard.md, "Keys"):
//! 32 random bytes in their own file beside the crowd database, never in
//! it, and left out of backups. Network tags and contributor hashes are
//! keyed with it, so they stay the same across restarts (a per-process key
//! would give every network and person a second identity after each one).
//!
//! To whoever holds the key, a network tag *is* the /24 or /48: there are
//! only 2^24 of them to try. Rule 9 allows truncated addresses; the tag is
//! never published.

use std::fs;
use std::io::Write;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use pingpath_crowd::ContributorId;
use ring::hmac;

use super::prefix::NetworkPrefix;

/// A network, as stored: 8 bytes of a keyed hash of its prefix.
pub type NetworkTag = [u8; 8];
/// A person, as stored: 16 bytes of a keyed hash of their contributor ID.
pub type ContributorHash = [u8; 16];

pub struct CrowdKey {
    network: hmac::Key,
    contributor: hmac::Key,
}

impl CrowdKey {
    /// Reads the key at `path`, or makes one there (readable by its owner
    /// only) when there is none. A key that won't read is an error, never
    /// silently replaced: the stored tags and hashes depend on it.
    pub fn load_or_create(path: &Path) -> Result<Self, String> {
        let master = match fs::read_to_string(path) {
            Ok(text) => {
                let bytes = STANDARD
                    .decode(text.trim())
                    .map_err(|e| format!("{}: not base64: {e}", path.display()))?;
                <[u8; 32]>::try_from(bytes.as_slice())
                    .map_err(|_| format!("{}: the key must be 32 bytes", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let master = pingpath_tunnel::keys::random_32().map_err(|e| e.to_string())?;
                write_private(path, STANDARD.encode(master).as_bytes())
                    .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                tracing::info!(path = %path.display(), "crowd key made");
                master
            }
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok(Self::from_master(&master))
    }

    pub fn from_master(master: &[u8; 32]) -> Self {
        let master = hmac::Key::new(hmac::HMAC_SHA256, master);
        let derive =
            |label: &[u8]| hmac::Key::new(hmac::HMAC_SHA256, hmac::sign(&master, label).as_ref());
        Self {
            network: derive(b"pingpath crowd network v1"),
            contributor: derive(b"pingpath crowd contributor v1"),
        }
    }

    pub fn tag(&self, prefix: &NetworkPrefix) -> NetworkTag {
        let mut tag = [0u8; 8];
        tag.copy_from_slice(&hmac::sign(&self.network, &prefix.tag_input()).as_ref()[..8]);
        tag
    }

    pub fn contributor(&self, id: &ContributorId) -> ContributorHash {
        let mut hash = [0u8; 16];
        hash.copy_from_slice(&hmac::sign(&self.contributor, id.as_bytes()).as_ref()[..16]);
        hash
    }
}

fn write_private(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.write_all(b"\n")?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crowd.key");
        let first = CrowdKey::load_or_create(&path).unwrap();
        let again = CrowdKey::load_or_create(&path).unwrap();
        let prefix = NetworkPrefix::of("203.0.113.9".parse().unwrap());
        let id = ContributorId::from_bytes([7; 16]);
        assert_eq!(first.tag(&prefix), again.tag(&prefix));
        assert_eq!(first.contributor(&id), again.contributor(&id));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        fs::write(&path, "short").unwrap();
        assert!(CrowdKey::load_or_create(&path).is_err());
    }

    #[test]
    fn tags_and_hashes_depend_on_the_key() {
        let a = CrowdKey::from_master(&[1; 32]);
        let b = CrowdKey::from_master(&[2; 32]);
        let prefix = NetworkPrefix::of("203.0.113.9".parse().unwrap());
        assert_ne!(a.tag(&prefix), b.tag(&prefix));
        let id = ContributorId::from_bytes([7; 16]);
        assert_ne!(a.contributor(&id), b.contributor(&id));
        // The same network, whichever address in it.
        let same = NetworkPrefix::of("203.0.113.200".parse().unwrap());
        assert_eq!(a.tag(&prefix), a.tag(&same));
    }
}
