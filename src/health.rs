//! Health status engine (PRD module 1): periodic network probes that map each
//! target to LIVE | SLOW | DOWN. Uses a tiny built-in HTTP/1.1 client so no
//! TLS stack or HTTP library has to be linked in.

use crate::state::{AppState, HealthSample, HEALTH_HISTORY};
use crate::util::{fmt_utc, now_secs};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const PROBE_INTERVAL: Duration = Duration::from_secs(15);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// Responses slower than this are reported as SLOW.
const SLOW_THRESHOLD_MS: u64 = 1_000;
/// Upper bound on how much of a response body a probe will read and discard.
const DRAIN_LIMIT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct HealthInfo {
    pub status: &'static str,
    pub latency_ms: Option<u64>,
    pub http_status: Option<u16>,
    pub detail: Option<String>,
    pub checked_at: String,
}

#[derive(Debug, PartialEq)]
pub struct ParsedUrl {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(url: &str) -> Result<ParsedUrl, String> {
    let url = url.trim();
    let (tls, rest) = if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else {
        return Err("health URL must start with http:// or https://".into());
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_string()),
    };
    if authority.is_empty() || authority.contains('@') || authority.contains(char::is_whitespace) {
        return Err("health URL has an invalid host".into());
    }
    let default_port = if tls { 443 } else { 80 };
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or("invalid IPv6 host")?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| "invalid port")?,
            None => default_port,
        };
        (h.to_string(), port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "invalid port")?),
            None => (authority.to_string(), default_port),
        }
    };
    if host.is_empty() || path.contains(['\r', '\n', ' ']) {
        return Err("health URL is malformed".into());
    }
    Ok(ParsedUrl { tls, host, port, path })
}

/// Minimal HTTP/1.1 request. Returns the status code of the response.
/// For `https://` URLs only TCP reachability is verified (returns `None`).
pub async fn http_request(
    method: &str,
    url: &str,
    timeout: Duration,
) -> Result<Option<u16>, String> {
    let u = parse_url(url)?;
    let work = async {
        let mut stream = TcpStream::connect((u.host.as_str(), u.port))
            .await
            .map_err(|e| format!("connect failed: {e}"))?;
        if u.tls {
            return Ok(None);
        }
        let host_header = if u.host.contains(':') {
            format!("[{}]:{}", u.host, u.port)
        } else {
            format!("{}:{}", u.host, u.port)
        };
        let req = format!(
            "{method} {} HTTP/1.1\r\nHost: {host_header}\r\nUser-Agent: PortalControl/{}\r\n\
             Accept: */*\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            u.path,
            env!("CARGO_PKG_VERSION")
        );
        stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| format!("write failed: {e}"))?;

        let mut buf = [0u8; 64];
        let mut filled = 0;
        while filled < buf.len() && !buf[..filled].contains(&b'\n') {
            let n = stream
                .read(&mut buf[filled..])
                .await
                .map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        let head = String::from_utf8_lossy(&buf[..filled]);
        let mut parts = head.split_whitespace();
        let code = match (parts.next(), parts.next().and_then(|c| c.parse::<u16>().ok())) {
            (Some(proto), Some(code)) if proto.starts_with("HTTP/") => code,
            _ => return Err("not an HTTP response".to_string()),
        };
        // Read the rest of the response (discarding it, bounded) so the server
        // sees a clean close instead of logging a broken pipe on every probe.
        let mut sink = [0u8; 8192];
        let mut drained = 0usize;
        while drained < DRAIN_LIMIT_BYTES {
            match stream.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n,
            }
        }
        Ok(Some(code))
    };
    tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
}

pub fn classify(result: &Result<Option<u16>, String>, latency_ms: u64) -> &'static str {
    match result {
        Err(_) => "DOWN",
        Ok(Some(code)) if *code >= 500 => "DOWN",
        Ok(_) if latency_ms >= SLOW_THRESHOLD_MS => "SLOW",
        Ok(_) => "LIVE",
    }
}

pub async fn probe(url: &str) -> HealthInfo {
    let t0 = Instant::now();
    let result = http_request("GET", url, PROBE_TIMEOUT).await;
    let latency_ms = t0.elapsed().as_millis() as u64;
    let status = classify(&result, latency_ms);
    HealthInfo {
        status,
        latency_ms: result.is_ok().then_some(latency_ms),
        http_status: result.as_ref().ok().copied().flatten(),
        detail: match &result {
            Err(e) => Some(e.clone()),
            Ok(Some(code)) if *code >= 500 => Some(format!("server error {code}")),
            Ok(None) => Some("TCP reachability only (https)".into()),
            _ => None,
        },
        checked_at: fmt_utc(now_secs()),
    }
}

/// Probe one target now, cache the result and notify dashboards on change.
pub async fn probe_target(state: &Arc<AppState>, target_id: i64, url: &str) -> HealthInfo {
    let info = probe(url).await;
    let previous = {
        let mut map = match state.health.lock() {
            Ok(m) => m,
            Err(p) => p.into_inner(),
        };
        map.insert(target_id, info.clone()).map(|old| old.status)
    };
    if let Ok(mut history) = state.health_history.lock() {
        let ring = history.entry(target_id).or_default();
        ring.push_back(HealthSample {
            t: now_secs(),
            status: info.status,
            latency_ms: info.latency_ms,
        });
        while ring.len() > HEALTH_HISTORY {
            ring.pop_front();
        }
    }
    if previous != Some(info.status) {
        state.emit(json!({"type": "health", "target_id": target_id, "health": info}));
        if let Some(old) = previous {
            let level = match info.status {
                "LIVE" => "ok",
                "SLOW" => "warn",
                _ => "error",
            };
            state.activity(
                level,
                "health",
                Some(target_id),
                format!(
                    "Health changed from {old} to {}{}",
                    info.status,
                    info.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default()
                ),
            );
        }
    }
    info
}

pub fn spawn_worker(state: Arc<AppState>) {
    tokio::spawn(async move {
        loop {
            if let Ok(targets) = state.db.list_targets().await {
                let mut probed = Vec::new();
                for t in targets {
                    let Some(url) = t.health_url.filter(|u| !u.trim().is_empty()) else {
                        continue;
                    };
                    probed.push(t.id);
                    let st = state.clone();
                    tokio::spawn(async move {
                        probe_target(&st, t.id, &url).await;
                    });
                }
                if let Ok(mut map) = state.health.lock() {
                    map.retain(|id, _| probed.contains(id));
                }
                if let Ok(mut map) = state.health_history.lock() {
                    map.retain(|id, _| probed.contains(id));
                }
            }
            tokio::time::sleep(PROBE_INTERVAL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn url_parsing() {
        assert_eq!(
            parse_url("http://127.0.0.1:8000/").unwrap(),
            ParsedUrl { tls: false, host: "127.0.0.1".into(), port: 8000, path: "/".into() }
        );
        assert_eq!(parse_url("http://localhost").unwrap().port, 80);
        assert_eq!(parse_url("https://example.com/health?x=1").unwrap().port, 443);
        assert_eq!(parse_url("http://[::1]:9000/a").unwrap().host, "::1");
        assert_eq!(parse_url("http://h:1?q=1").unwrap().path, "/?q=1");
        assert!(parse_url("ftp://x").is_err());
        assert!(parse_url("http://").is_err());
        assert!(parse_url("http://h:notaport/").is_err());
        assert!(parse_url("http://user@h/").is_err());
    }

    #[test]
    fn classification() {
        assert_eq!(classify(&Ok(Some(200)), 20), "LIVE");
        assert_eq!(classify(&Ok(Some(302)), 20), "LIVE");
        assert_eq!(classify(&Ok(Some(404)), 20), "LIVE");
        assert_eq!(classify(&Ok(Some(200)), 1500), "SLOW");
        assert_eq!(classify(&Ok(Some(503)), 20), "DOWN");
        assert_eq!(classify(&Err("refused".into()), 20), "DOWN");
    }

    async fn serve_once(response: &'static str, delay: Duration) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = sock.read(&mut buf).await;
                tokio::time::sleep(delay).await;
                let _ = sock.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn live_slow_and_down_probes() {
        let ok = "HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n";
        let url = serve_once(ok, Duration::ZERO).await;
        let h = probe(&url).await;
        assert_eq!((h.status, h.http_status), ("LIVE", Some(200)));

        let url = serve_once(ok, Duration::from_millis(1200)).await;
        assert_eq!(probe(&url).await.status, "SLOW");

        let url = serve_once("HTTP/1.1 500 Internal Server Error\r\n\r\n", Duration::ZERO).await;
        assert_eq!(probe(&url).await.status, "DOWN");

        // Nothing listening: grab a free port and close it again.
        let port = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
        let h = probe(&format!("http://127.0.0.1:{port}/")).await;
        assert_eq!(h.status, "DOWN");
        assert!(h.latency_ms.is_none());
    }
}
