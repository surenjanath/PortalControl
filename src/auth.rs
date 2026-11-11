//! Optional password protection for the dashboard.
//!
//! With no password set the dashboard behaves as before (anyone who can
//! reach the bound address may use it). Once a password is set, every API
//! call and the WebSocket need a session cookie obtained by signing in.
//!
//! * The password is stored only as a salted PBKDF2-HMAC-SHA256 hash.
//! * Sessions are random 256-bit tokens kept in memory; they end when the
//!   manager restarts, on sign-out, and whenever the password changes.
//! * Repeated wrong passwords lock sign-in for a short while.
//! * The CLI on this machine authenticates with a control token written to
//!   the data directory at startup, so `portalcontrol stop` keeps working.

use pbkdf2::pbkdf2_hmac;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

pub const MIN_PASSWORD_CHARS: usize = 8;
pub const SESSION_COOKIE: &str = "pc_session";
pub const CONTROL_HEADER: &str = "x-portalcontrol-token";
pub const SESSION_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Database key under which the password hash is stored.
pub const PASSWORD_KEY: &str = "password_hash";

/// Tests use a low work factor so the suite stays fast.
const ROUNDS: u32 = if cfg!(test) { 1_000 } else { 150_000 };
const MAX_FAILURES: usize = 5;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `bytes` random bytes from the operating system, hex encoded.
pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::getrandom(&mut buf).expect("the operating system random source is unavailable");
    hex(&buf)
}

fn derive(password: &str, salt: &[u8], rounds: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), salt, rounds, &mut out);
    out
}

/// `pbkdf2-sha256$<rounds>$<salt hex>$<hash hex>`
pub fn hash_password(password: &str) -> String {
    let salt = unhex(&random_hex(16)).unwrap_or_default();
    format!("pbkdf2-sha256${ROUNDS}${}${}", hex(&salt), hex(&derive(password, &salt, ROUNDS)))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    let parts: Vec<&str> = stored.split('$').collect();
    let [scheme, rounds, salt, expected] = parts.as_slice() else { return false };
    if *scheme != "pbkdf2-sha256" {
        return false;
    }
    let (Ok(rounds), Some(salt), Some(expected)) = (rounds.parse::<u32>(), unhex(salt), unhex(expected)) else {
        return false;
    };
    if rounds == 0 || rounds > 10_000_000 || expected.len() != 32 {
        return false;
    }
    let actual = derive(password, &salt, rounds);
    // Constant-time comparison.
    actual.iter().zip(&expected).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

pub fn validate_new_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!("the password must be at least {MIN_PASSWORD_CHARS} characters long"));
    }
    if password.len() > 1024 {
        return Err("the password is too long".into());
    }
    Ok(())
}

/// Value of one cookie in a `Cookie:` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (k, v) = pair.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

pub struct Auth {
    password_hash: RwLock<Option<String>>,
    sessions: Mutex<HashMap<String, Instant>>,
    failures: Mutex<Vec<Instant>>,
    /// Lets the CLI on this machine call the API without a browser session.
    pub control_token: String,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            password_hash: RwLock::new(None),
            sessions: Mutex::new(HashMap::new()),
            failures: Mutex::new(Vec::new()),
            control_token: random_hex(32),
        }
    }
}

impl Auth {
    pub fn password_set(&self) -> bool {
        self.password_hash.read().map(|h| h.is_some()).unwrap_or(true)
    }

    pub fn current_hash(&self) -> Option<String> {
        self.password_hash.read().ok().and_then(|h| h.clone())
    }

    /// Install (or remove) the password hash and end every session.
    pub fn set_hash(&self, hash: Option<String>) {
        if let Ok(mut h) = self.password_hash.write() {
            *h = hash;
        }
        if let Ok(mut s) = self.sessions.lock() {
            s.clear();
        }
    }

    pub fn create_session(&self) -> String {
        let token = random_hex(32);
        if let Ok(mut s) = self.sessions.lock() {
            let now = Instant::now();
            s.retain(|_, expires| *expires > now);
            s.insert(token.clone(), now + SESSION_TTL);
        }
        token
    }

    pub fn session_valid(&self, token: &str) -> bool {
        self.sessions
            .lock()
            .map(|s| s.get(token).map(|expires| *expires > Instant::now()).unwrap_or(false))
            .unwrap_or(false)
    }

    pub fn end_session(&self, token: &str) {
        if let Ok(mut s) = self.sessions.lock() {
            s.remove(token);
        }
    }

    pub fn control_token_matches(&self, presented: &str) -> bool {
        presented.len() == self.control_token.len()
            && presented
                .bytes()
                .zip(self.control_token.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    }

    /// Seconds until sign-in is allowed again, if it is currently locked.
    pub fn locked_for(&self) -> Option<u64> {
        let now = Instant::now();
        let mut f = self.failures.lock().ok()?;
        f.retain(|t| now.duration_since(*t) < FAILURE_WINDOW);
        if f.len() < MAX_FAILURES {
            return None;
        }
        let oldest = f.first().copied()?;
        Some(FAILURE_WINDOW.saturating_sub(now.duration_since(oldest)).as_secs().max(1))
    }

    pub fn note_failure(&self) {
        if let Ok(mut f) = self.failures.lock() {
            f.push(Instant::now());
        }
    }

    pub fn clear_failures(&self) {
        if let Ok(mut f) = self.failures.lock() {
            f.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_hash_and_verify() {
        let stored = hash_password("correct horse battery");
        assert!(stored.starts_with("pbkdf2-sha256$"));
        assert!(!stored.contains("correct"));
        assert!(verify_password("correct horse battery", &stored));
        assert!(!verify_password("correct horse batterx", &stored));
        assert!(!verify_password("", &stored));
        // Salted: the same password never hashes to the same string.
        assert_ne!(stored, hash_password("correct horse battery"));
        for garbage in ["", "plain", "pbkdf2-sha256$0$00$00", "md5$1$aa$bb", "pbkdf2-sha256$x$zz$yy"] {
            assert!(!verify_password("anything", garbage), "{garbage:?}");
        }
        assert!(validate_new_password("short").is_err());
        assert!(validate_new_password("long enough").is_ok());
    }

    #[test]
    fn sessions_and_control_token() {
        let auth = Auth::default();
        assert!(!auth.password_set());
        auth.set_hash(Some(hash_password("password-1")));
        assert!(auth.password_set());

        let a = auth.create_session();
        let b = auth.create_session();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(auth.session_valid(&a) && auth.session_valid(&b));
        assert!(!auth.session_valid("nope") && !auth.session_valid(""));
        auth.end_session(&a);
        assert!(!auth.session_valid(&a) && auth.session_valid(&b));

        // Changing the password signs everyone out.
        auth.set_hash(Some(hash_password("password-2")));
        assert!(!auth.session_valid(&b));

        assert!(auth.control_token_matches(&auth.control_token.clone()));
        assert!(!auth.control_token_matches("") && !auth.control_token_matches(&"0".repeat(64)));
    }

    #[test]
    fn repeated_failures_lock_sign_in() {
        let auth = Auth::default();
        for _ in 0..(MAX_FAILURES - 1) {
            auth.note_failure();
        }
        assert!(auth.locked_for().is_none());
        auth.note_failure();
        assert!(auth.locked_for().unwrap() >= 1);
        auth.clear_failures();
        assert!(auth.locked_for().is_none());
    }

    #[test]
    fn cookie_parsing() {
        let header = "theme=dark; pc_session=abc123; other=x=y";
        assert_eq!(cookie_value(header, "pc_session"), Some("abc123"));
        assert_eq!(cookie_value(header, "other"), Some("x=y"));
        assert_eq!(cookie_value(header, "session"), None);
        assert_eq!(cookie_value("", "pc_session"), None);
    }
}
