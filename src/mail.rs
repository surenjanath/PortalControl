//! A small SMTP client for alert emails: STARTTLS (port 587), implicit TLS
//! (465) or plain (a local relay on 25), AUTH PLAIN / LOGIN, one text/plain
//! UTF-8 message. Certificates are checked against Mozilla's root store.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const TIMEOUT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// Plain connection upgraded with STARTTLS (port 587). The default.
    #[default]
    Starttls,
    /// TLS from the first byte (port 465).
    Tls,
    /// No encryption: only for a relay on this machine or a trusted network.
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Smtp {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub security: Security,
    #[serde(default)]
    pub username: String,
    /// Plaintext here; stored sealed by the caller.
    #[serde(default)]
    pub password: String,
    pub from: String,
    pub to: Vec<String>,
}

pub fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Is `addr` a plausible single email address (and safe to put in a header)?
pub fn valid_address(addr: &str) -> bool {
    let a = addr.trim();
    let Some((local, domain)) = a.rsplit_once('@') else { return false };
    !local.is_empty()
        && domain.contains('.')
        && a.len() <= 254
        && !a.chars().any(|c| c.is_whitespace() || c.is_control() || "<>,;\"()[]\\".contains(c))
}

/// The message as sent: headers plus a base64 body, so any text is safe
/// (no line-length limits, no dot-stuffing).
pub fn compose(cfg: &Smtp, subject: &str, body: &str) -> String {
    let subject: String = subject.chars().filter(|c| !c.is_control()).collect();
    let encoded_body: Vec<String> = base64(body.replace("\r\n", "\n").replace('\n', "\r\n").as_bytes())
        .as_bytes()
        .chunks(76)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    let id = crate::auth::random_hex(12);
    let domain = cfg.from.rsplit_once('@').map(|(_, d)| d).unwrap_or("portalcontrol.local");
    format!(
        "From: PortalControl <{from}>\r\nTo: {to}\r\nSubject: =?UTF-8?B?{subject}?=\r\nDate: {date}\r\n\
         Message-ID: <{id}@{domain}>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\
         Content-Transfer-Encoding: base64\r\nX-Mailer: PortalControl\r\n\r\n{body}\r\n",
        from = cfg.from.trim(),
        to = cfg.to.iter().map(|t| t.trim()).collect::<Vec<_>>().join(", "),
        subject = base64(subject.as_bytes()),
        date = chrono::Local::now().to_rfc2822(),
        body = encoded_body.join("\r\n"),
    )
}

/// Read one (possibly multi-line) reply; check its code.
async fn reply<S: AsyncRead + Unpin>(r: &mut BufReader<S>, expect: &[u16]) -> Result<String, String> {
    let mut all = String::new();
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(TIMEOUT, r.read_line(&mut line))
            .await
            .map_err(|_| "the mail server did not answer in time".to_string())?
            .map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the mail server closed the connection".into());
        }
        all.push_str(&line);
        // "250-..." continues, "250 ..." ends.
        if line.len() < 4 || line.as_bytes()[3] != b'-' {
            break;
        }
    }
    let code: u16 = all.get(..3).and_then(|c| c.parse().ok()).unwrap_or(0);
    if expect.contains(&code) {
        Ok(all)
    } else {
        Err(format!("the mail server said: {}", all.trim().replace("\r\n", " ")))
    }
}

async fn command<S: AsyncRead + AsyncWrite + Unpin>(
    r: &mut BufReader<S>,
    line: &str,
    expect: &[u16],
) -> Result<String, String> {
    r.get_mut().write_all(format!("{line}\r\n").as_bytes()).await.map_err(|e| e.to_string())?;
    r.get_mut().flush().await.map_err(|e| e.to_string())?;
    reply(r, expect).await
}

/// EHLO, AUTH and the message, on an established (and, if wanted, encrypted)
/// connection. `greeted`: the 220 greeting was already read.
async fn deliver<S: AsyncRead + AsyncWrite + Unpin>(
    r: &mut BufReader<S>,
    cfg: &Smtp,
    message: &str,
    greeted: bool,
) -> Result<(), String> {
    if !greeted {
        reply(r, &[220]).await?;
    }
    let ehlo = command(r, "EHLO portalcontrol", &[250]).await?;
    if !cfg.username.is_empty() {
        let caps = ehlo.to_uppercase();
        if caps.contains("PLAIN") || !caps.contains("LOGIN") {
            let token = base64(format!("\0{}\0{}", cfg.username, cfg.password).as_bytes());
            command(r, &format!("AUTH PLAIN {token}"), &[235])
                .await
                .map_err(|e| format!("sign-in failed: {e}"))?;
        } else {
            command(r, "AUTH LOGIN", &[334]).await?;
            command(r, &base64(cfg.username.as_bytes()), &[334]).await?;
            command(r, &base64(cfg.password.as_bytes()), &[235])
                .await
                .map_err(|e| format!("sign-in failed: {e}"))?;
        }
    }
    command(r, &format!("MAIL FROM:<{}>", cfg.from.trim()), &[250]).await?;
    for to in &cfg.to {
        command(r, &format!("RCPT TO:<{}>", to.trim()), &[250, 251]).await?;
    }
    command(r, "DATA", &[354]).await?;
    // A base64 body never has a line starting with '.', so no dot-stuffing.
    r.get_mut().write_all(message.as_bytes()).await.map_err(|e| e.to_string())?;
    command(r, ".", &[250]).await?;
    let _ = command(r, "QUIT", &[221]).await;
    Ok(())
}

fn tls_connector() -> tokio_rustls::TlsConnector {
    let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}

pub fn check(cfg: &Smtp) -> Result<(), String> {
    if cfg.host.trim().is_empty() {
        return Err("enter the mail server (SMTP host)".into());
    }
    if cfg.port == 0 {
        return Err("enter the SMTP port (587 for STARTTLS, 465 for TLS)".into());
    }
    if !valid_address(&cfg.from) {
        return Err("enter a valid sender address".into());
    }
    if cfg.to.is_empty() || !cfg.to.iter().all(|t| valid_address(t)) {
        return Err("enter at least one valid recipient address".into());
    }
    Ok(())
}

/// Send one email.
pub async fn send(cfg: &Smtp, subject: &str, body: &str) -> Result<(), String> {
    check(cfg)?;
    let message = compose(cfg, subject, body);
    let host = cfg.host.trim().to_string();
    let tcp = tokio::time::timeout(TIMEOUT, TcpStream::connect((host.as_str(), cfg.port)))
        .await
        .map_err(|_| format!("could not reach {host}:{} in time", cfg.port))?
        .map_err(|e| format!("could not connect to {host}:{}: {e}", cfg.port))?;
    let name = rustls::pki_types::ServerName::try_from(host.clone())
        .map_err(|_| format!("invalid host name {host}"))?;
    match cfg.security {
        Security::Tls => {
            let tls = tls_connector()
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS with {host} failed: {e}"))?;
            deliver(&mut BufReader::new(tls), cfg, &message, false).await
        }
        Security::None => deliver(&mut BufReader::new(tcp), cfg, &message, false).await,
        Security::Starttls => {
            let mut plain = BufReader::new(tcp);
            reply(&mut plain, &[220]).await?;
            let caps = command(&mut plain, "EHLO portalcontrol", &[250]).await?;
            if !caps.to_uppercase().contains("STARTTLS") {
                return Err(format!(
                    "{host} does not offer STARTTLS; choose TLS (port 465) or check the port"
                ));
            }
            command(&mut plain, "STARTTLS", &[220]).await?;
            let tcp = plain.into_inner();
            let tls = tls_connector()
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS with {host} failed: {e}"))?;
            deliver(&mut BufReader::new(tls), cfg, &message, true).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        for (plain, enc) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(plain.as_bytes()), enc);
        }
    }

    #[test]
    fn addresses_are_checked() {
        assert!(valid_address("ops@example.com"));
        assert!(!valid_address("ops@localhost"));
        assert!(!valid_address("a@b.com\r\nBcc: x@y.com"), "no header injection");
        assert!(!valid_address("Name <a@b.com>"));
    }

    #[test]
    fn message_is_safe_and_encoded() {
        let cfg =
            Smtp { from: "pc@example.com".into(), to: vec!["me@example.com".into()], ..Default::default() };
        let m = compose(&cfg, "storefront-web: crash\r\nBcc: evil@x.com", "line one\n.\nline \u{2014} three");
        assert!(!m.contains("Bcc:"), "control characters are stripped from the subject");
        assert!(m.contains("Content-Transfer-Encoding: base64"));
        assert!(m.lines().all(|l| l.len() <= 998));
        assert!(!m.lines().any(|l| l == "."), "no bare dot line in the body");
    }

    /// A scripted plain SMTP server: checks the conversation end to end.
    #[tokio::test]
    async fn talks_smtp() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut seen = String::new();
            let mut buf = vec![0u8; 65536];
            s.write_all(b"220 test ready\r\n").await.unwrap();
            let mut in_data = false;
            loop {
                let n = s.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                seen.push_str(&chunk);
                for line in chunk.split("\r\n").filter(|l| !l.is_empty()) {
                    let answer: &[u8] = if in_data {
                        if line == "." {
                            in_data = false;
                            b"250 queued\r\n"
                        } else {
                            continue;
                        }
                    } else if line.starts_with("EHLO") {
                        b"250-test\r\n250 AUTH PLAIN LOGIN\r\n"
                    } else if line.starts_with("AUTH PLAIN") {
                        b"235 ok\r\n"
                    } else if line == "DATA" {
                        in_data = true;
                        b"354 go\r\n"
                    } else if line == "QUIT" {
                        b"221 bye\r\n"
                    } else {
                        b"250 ok\r\n"
                    };
                    s.write_all(answer).await.unwrap();
                }
                if seen.contains("QUIT") {
                    break;
                }
            }
            seen
        });
        let cfg = Smtp {
            host: "127.0.0.1".into(),
            port,
            security: Security::None,
            username: "u".into(),
            password: "p".into(),
            from: "pc@example.com".into(),
            to: vec!["me@example.com".into()],
        };
        send(&cfg, "Test", "hello").await.unwrap();
        let seen = server.await.unwrap();
        assert!(seen.contains(&format!("AUTH PLAIN {}", base64(b"\0u\0p"))));
        assert!(seen.contains("RCPT TO:<me@example.com>"));
        assert!(seen.contains(&base64(b"hello")));
    }
}
