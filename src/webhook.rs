//! Checks for inbound deploy webhooks, kept apart from the HTTP handler so
//! they can be tested on their own.
//!
//! * **Signing secret.** The token in the URL is always required. A target can
//!   also have a signing secret; then the caller must prove it, in whichever
//!   form its sender supports:
//!     - GitHub / Gitea / Forgejo: `X-Hub-Signature-256` or `X-Gitea-Signature`,
//!       an HMAC-SHA256 of the raw body
//!     - GitLab: `X-Gitlab-Token` equal to the secret
//!     - anything else (curl, CI): `X-Webhook-Secret` equal to the secret
//! * **Branch filter.** A push payload names its ref. A push to another branch
//!   or a tag is acknowledged and skipped instead of deploying.
//! * **Ping.** GitHub's test delivery (`X-GitHub-Event: ping`) is answered
//!   without deploying.

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// What to do with a delivery once the URL token has matched.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Deploy,
    /// Acknowledge without deploying, with the reason.
    Skip(String),
    /// Refuse: the signature or secret is missing or wrong.
    Reject(&'static str),
}

/// The request headers this module looks at (lower-case names).
#[derive(Default)]
pub struct Delivery<'a> {
    pub hub_signature_256: Option<&'a str>,
    pub gitea_signature: Option<&'a str>,
    pub gitlab_token: Option<&'a str>,
    pub plain_secret: Option<&'a str>,
    pub github_event: Option<&'a str>,
    pub body: &'a [u8],
}

pub fn check(delivery: &Delivery, secret: Option<&str>, tracked_branch: &str) -> Verdict {
    if let Some(secret) = secret.filter(|s| !s.is_empty()) {
        if !secret_proven(delivery, secret) {
            return Verdict::Reject("signature or secret missing or wrong");
        }
    }
    if delivery.github_event.is_some_and(|e| e.eq_ignore_ascii_case("ping")) {
        return Verdict::Skip("ping received; webhook is set up".into());
    }
    match pushed_ref(delivery.body) {
        Some(r) => match r.strip_prefix("refs/heads/") {
            Some(branch) if branch == tracked_branch => Verdict::Deploy,
            Some(branch) => Verdict::Skip(format!("push to {branch}; this target tracks {tracked_branch}")),
            None => Verdict::Skip(format!("{r} is not a branch")),
        },
        // No payload, or not a push: a plain trigger from a script.
        None => Verdict::Deploy,
    }
}

fn secret_proven(d: &Delivery, secret: &str) -> bool {
    let hmac_ok = |header: Option<&str>| {
        header.is_some_and(|h| {
            let hex = h.trim().strip_prefix("sha256=").unwrap_or(h.trim());
            verify_hmac(secret.as_bytes(), d.body, hex)
        })
    };
    hmac_ok(d.hub_signature_256)
        || hmac_ok(d.gitea_signature)
        || d.gitlab_token.is_some_and(|t| constant_time_eq(t.as_bytes(), secret.as_bytes()))
        || d.plain_secret.is_some_and(|t| constant_time_eq(t.as_bytes(), secret.as_bytes()))
}

fn verify_hmac(key: &[u8], body: &[u8], hex_sig: &str) -> bool {
    let Some(sig) = crate::auth::unhex(hex_sig) else { return false };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) else { return false };
    mac.update(body);
    mac.verify_slice(&sig).is_ok() // constant-time
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The `ref` of a push payload: JSON as sent by GitHub, GitLab and Gitea, or
/// GitHub's form encoding (`payload=<url-encoded JSON>`).
fn pushed_ref(body: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(body).ok()?.trim();
    let json: Value = if let Some(form) = text.strip_prefix("payload=") {
        serde_json::from_str(&url_decode(form)?).ok()?
    } else {
        serde_json::from_str(text).ok()?
    };
    json.get("ref")?.as_str().map(str::to_string)
}

fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                out.push(u8::from_str_radix(s.get(i + 1..i + 3)?, 16).ok()?);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        format!("sha256={}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }

    const PUSH_MAIN: &[u8] = br#"{"ref":"refs/heads/main","after":"abc"}"#;

    #[test]
    fn no_secret_behaves_as_before() {
        let d = Delivery { body: b"", ..Default::default() };
        assert_eq!(check(&d, None, "main"), Verdict::Deploy);
    }

    #[test]
    fn github_signature() {
        let good = sign("s3cret", PUSH_MAIN);
        let d = Delivery { hub_signature_256: Some(&good), body: PUSH_MAIN, ..Default::default() };
        assert_eq!(check(&d, Some("s3cret"), "main"), Verdict::Deploy);

        let bad = sign("other", PUSH_MAIN);
        let d = Delivery { hub_signature_256: Some(&bad), body: PUSH_MAIN, ..Default::default() };
        assert!(matches!(check(&d, Some("s3cret"), "main"), Verdict::Reject(_)));

        let tampered = Delivery {
            hub_signature_256: Some(&good),
            body: br#"{"ref":"refs/heads/evil"}"#,
            ..Default::default()
        };
        assert!(matches!(check(&tampered, Some("s3cret"), "main"), Verdict::Reject(_)));
    }

    #[test]
    fn secret_required_once_set() {
        let d = Delivery { body: PUSH_MAIN, ..Default::default() };
        assert!(matches!(check(&d, Some("s3cret"), "main"), Verdict::Reject(_)));
        let gitlab = Delivery { gitlab_token: Some("s3cret"), body: PUSH_MAIN, ..Default::default() };
        assert_eq!(check(&gitlab, Some("s3cret"), "main"), Verdict::Deploy);
        let curl = Delivery { plain_secret: Some("s3cret"), ..Default::default() };
        assert_eq!(check(&curl, Some("s3cret"), "main"), Verdict::Deploy);
        let wrong = Delivery { plain_secret: Some("s3cre"), ..Default::default() };
        assert!(matches!(check(&wrong, Some("s3cret"), "main"), Verdict::Reject(_)));
    }

    #[test]
    fn branch_filter() {
        let d = Delivery { body: br#"{"ref":"refs/heads/feature/x"}"#, ..Default::default() };
        assert!(matches!(check(&d, None, "main"), Verdict::Skip(_)));
        let tag = Delivery { body: br#"{"ref":"refs/tags/v1.0"}"#, ..Default::default() };
        assert!(matches!(check(&tag, None, "main"), Verdict::Skip(_)));
        let d = Delivery { body: PUSH_MAIN, ..Default::default() };
        assert_eq!(check(&d, None, "main"), Verdict::Deploy);
        let form =
            Delivery { body: b"payload=%7B%22ref%22%3A%22refs%2Fheads%2Fdev%22%7D", ..Default::default() };
        assert!(matches!(check(&form, None, "main"), Verdict::Skip(_)));
    }

    #[test]
    fn ping_is_acknowledged() {
        let d = Delivery { github_event: Some("ping"), body: b"{\"zen\":\"hi\"}", ..Default::default() };
        assert!(matches!(check(&d, None, "main"), Verdict::Skip(_)));
    }
}
