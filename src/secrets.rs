//! Encryption at rest for the secrets kept in the database: target
//! environment variables and webhook signing secrets.
//!
//! Values are sealed with ChaCha20-Poly1305 under a random 256-bit key kept
//! in `secret.key` next to the database (readable only by the owner on macOS
//! and Linux; `%LOCALAPPDATA%` is already per-user on Windows). The database
//! file on its own, in a backup, a support bundle or copied to another
//! machine, then no longer reveals passwords and API keys. Someone who can
//! read the whole data folder can still read both; that is the limit of
//! encryption without a password typed at every start.
//!
//! Stored form: `enc1:<hex nonce><hex ciphertext+tag>`. Values without the
//! prefix are plaintext from before encryption existed; they are read as they
//! are and sealed by the start-up migration.

use crate::auth::{hex, unhex};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305, NONCE_LEN};
use std::path::Path;
use std::sync::OnceLock;

pub const KEY_FILE: &str = "secret.key";
const PREFIX: &str = "enc1:";
const KEY_LEN: usize = 32;

static KEY: OnceLock<LessSafeKey> = OnceLock::new();

fn make_key(bytes: &[u8]) -> Result<LessSafeKey, String> {
    UnboundKey::new(&CHACHA20_POLY1305, bytes)
        .map(LessSafeKey::new)
        .map_err(|_| "the key has the wrong length".to_string())
}

/// Load the key from `dir`, creating it on first use. Called once at start-up
/// before the database is opened; later calls keep the key already loaded.
pub fn load_or_create(dir: &Path) -> Result<(), String> {
    if KEY.get().is_some() {
        return Ok(());
    }
    let path = dir.join(KEY_FILE);
    let bytes = match std::fs::read_to_string(&path) {
        Ok(text) => unhex(text.trim()).filter(|b| b.len() == KEY_LEN).ok_or_else(|| {
            format!("{} is damaged (expected {} hex characters)", path.display(), KEY_LEN * 2)
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut key = vec![0u8; KEY_LEN];
            getrandom::getrandom(&mut key).map_err(|e| format!("no random source: {e}"))?;
            write_private(&path, &hex(&key)).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            key
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let _ = KEY.set(make_key(&bytes)?);
    Ok(())
}

/// Create the key file so that only its owner can read it. `create_new`
/// never replaces an existing key.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(text.as_bytes())
}

/// Tests (and in-memory databases) use a throwaway key.
#[cfg(test)]
pub fn ensure_test_key() {
    let _ = KEY.get_or_init(|| {
        let mut key = vec![0u8; KEY_LEN];
        getrandom::getrandom(&mut key).unwrap();
        make_key(&key).unwrap()
    });
}

pub fn is_sealed(stored: &str) -> bool {
    stored.starts_with(PREFIX)
}

/// Encrypt `plain` for the column named by `aad` (which binds the value to
/// its column, so one cannot be swapped for another). Empty stays empty.
pub fn seal(plain: &str, aad: &str) -> String {
    if plain.is_empty() {
        return String::new();
    }
    let key = KEY.get().expect("secrets::load_or_create runs before the database is used");
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).expect("the operating system random source is unavailable");
    let mut buf = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(aad.as_bytes()), &mut buf)
        .expect("ChaCha20-Poly1305 sealing does not fail for in-memory input");
    format!("{PREFIX}{}{}", hex(&nonce), hex(&buf))
}

/// Decrypt a stored value. Plaintext from before encryption is returned as it
/// is; `None` means the value is sealed but cannot be opened with this key.
pub fn open(stored: &str, aad: &str) -> Option<String> {
    let Some(rest) = stored.strip_prefix(PREFIX) else {
        return Some(stored.to_string());
    };
    let key = KEY.get()?;
    let bytes = unhex(rest)?;
    if bytes.len() < NONCE_LEN {
        return None;
    }
    let (nonce, sealed) = bytes.split_at(NONCE_LEN);
    let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut buf = sealed.to_vec();
    let plain = key.open_in_place(nonce, Aad::from(aad.as_bytes()), &mut buf).ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper() {
        ensure_test_key();
        let sealed = seal("DB_PASSWORD=hunter2", "env_vars");
        assert!(is_sealed(&sealed) && !sealed.contains("hunter2"));
        assert_eq!(open(&sealed, "env_vars").as_deref(), Some("DB_PASSWORD=hunter2"));
        assert_ne!(seal("x", "env_vars"), seal("x", "env_vars"), "a fresh nonce every time");

        assert_eq!(open(&sealed, "webhook_secret"), None, "bound to its column");
        let mut flipped = sealed.clone();
        let last = flipped.pop().unwrap();
        flipped.push(if last == '0' { '1' } else { '0' });
        assert_eq!(open(&flipped, "env_vars"), None, "tampering is detected");

        assert_eq!(open("DEBUG=1", "env_vars").as_deref(), Some("DEBUG=1"), "old plaintext reads as is");
        assert_eq!(seal("", "env_vars"), "");
    }

    #[test]
    fn key_file_is_created_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE);
        write_private(&path, "abc").unwrap();
        assert!(write_private(&path, "def").is_err(), "an existing key is never replaced");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
}
