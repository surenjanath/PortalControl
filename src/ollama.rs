//! Talking to a local Ollama server (`http://127.0.0.1:11434` by default):
//! list its models and ask one for a structured (JSON-schema) answer.
//! Plain HTTP only: Ollama runs on this machine or a trusted network, and log
//! lines must not leave it unencrypted over the internet.

use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub const DEFAULT_URL: &str = "http://127.0.0.1:11434";
const MAX_RESPONSE: usize = 8 * 1024 * 1024;

/// `http://host:port` -> (host, port). Paths are ignored.
fn host_port(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .trim()
        .strip_prefix("http://")
        .ok_or("the Ollama address must start with http:// (Ollama runs on this machine or your network)")?;
    let authority = rest.split('/').next().unwrap_or("");
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().map_err(|_| "invalid port in the Ollama address")?),
        None => (authority.to_string(), 80),
    };
    if host.is_empty() {
        return Err("invalid Ollama address".into());
    }
    Ok((host.trim_matches(['[', ']']).to_string(), port))
}

fn decode_chunked(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let line_end = body.windows(2).position(|w| w == b"\r\n").ok_or("bad chunked response")?;
        let size_text = std::str::from_utf8(&body[..line_end]).map_err(|_| "bad chunk size")?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "bad chunk size")?;
        body = &body[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if body.len() < size {
            return Err("truncated chunked response".into());
        }
        out.extend_from_slice(&body[..size]);
        body = body.get(size + 2..).unwrap_or(&[]);
    }
}

/// One HTTP/1.1 request with `Connection: close`; returns (status, body).
async fn request(
    url: &str,
    method: &str,
    path: &str,
    body: Option<&Value>,
    timeout: Duration,
) -> Result<(u16, Value), String> {
    let (host, port) = host_port(url)?;
    let payload = body.map(|b| b.to_string()).unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUser-Agent: PortalControl\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let work = async {
        let mut s = TcpStream::connect((host.as_str(), port)).await.map_err(|e| {
            format!("Ollama is not reachable at {host}:{port} ({e}). Is it running? (ollama serve)")
        })?;
        s.write_all(head.as_bytes()).await.map_err(|e| e.to_string())?;
        s.write_all(payload.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut raw = Vec::new();
        let mut buf = [0u8; 16384];
        loop {
            let n = s.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            raw.extend_from_slice(&buf[..n]);
            if raw.len() > MAX_RESPONSE {
                return Err("the answer from Ollama is too large".to_string());
            }
        }
        Ok(raw)
    };
    let raw = tokio::time::timeout(timeout, work)
        .await
        .map_err(|_| format!("Ollama did not answer within {} s", timeout.as_secs()))??;
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("malformed answer from Ollama")?;
    let headers = String::from_utf8_lossy(&raw[..split]).to_lowercase();
    let status: u16 = headers.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut body = raw[split + 4..].to_vec();
    if headers.contains("transfer-encoding: chunked") {
        body = decode_chunked(&body)?;
    }
    let value: Value =
        serde_json::from_slice(&body).unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&body)}));
    Ok((status, value))
}

fn error_of(status: u16, v: &Value) -> String {
    v.get("error")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Ollama answered HTTP {status}"))
}

/// Installed models: (name, size in bytes, parameter size such as "3.1B").
pub async fn models(url: &str) -> Result<Vec<Value>, String> {
    let (status, v) = request(url, "GET", "/api/tags", None, Duration::from_secs(8)).await?;
    if status != 200 {
        return Err(error_of(status, &v));
    }
    Ok(v.get("models")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .map(|m| {
                    json!({
                        "name": m.get("name"),
                        "size": m.get("size"),
                        "parameters": m.get("details").and_then(|d| d.get("parameter_size")),
                        "family": m.get("details").and_then(|d| d.get("family")),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// What one model call cost, and its raw text (kept in the trace).
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub took: Duration,
    pub prompt_tokens: Option<i64>,
    pub answer_tokens: Option<i64>,
    pub raw: String,
}

/// Ask `model` with a system and a user message for a JSON answer that
/// matches `schema`. Returns the parsed answer and what the call cost.
pub async fn chat_json(
    url: &str,
    model: &str,
    system: &str,
    user: &str,
    schema: &Value,
    timeout: Duration,
) -> Result<(Value, Usage), String> {
    let body = json!({
        "model": model,
        "stream": false,
        "format": schema,
        "keep_alive": "15m",
        // Reasoning ("thinking") models would spend the answer budget thinking;
        // the monitor wants a direct answer. Older models ignore the flag.
        "think": false,
        // num_predict caps the answer, so a model that loops can never hang
        // the monitor.
        "options": {"temperature": 0.1, "num_ctx": 8192, "num_predict": 700},
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
    });
    let started = std::time::Instant::now();
    let (status, v) = request(url, "POST", "/api/chat", Some(&body), timeout).await?;
    if status != 200 {
        return Err(error_of(status, &v));
    }
    let content = v.get("message").and_then(|m| m.get("content")).and_then(Value::as_str).unwrap_or("");
    let parsed: Value = serde_json::from_str(content.trim()).map_err(|_| {
        format!(
            "the model did not answer in the expected format: {}",
            content.chars().take(200).collect::<String>()
        )
    })?;
    let usage = Usage {
        took: started.elapsed(),
        prompt_tokens: v.get("prompt_eval_count").and_then(Value::as_i64),
        answer_tokens: v.get("eval_count").and_then(Value::as_i64),
        raw: content.to_string(),
    };
    Ok((parsed, usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses() {
        assert_eq!(host_port("http://127.0.0.1:11434").unwrap(), ("127.0.0.1".into(), 11434));
        assert_eq!(host_port("http://gpu-box.lan:8080/").unwrap(), ("gpu-box.lan".into(), 8080));
        assert!(host_port("https://api.example.com").is_err(), "only plain http to a local server");
    }

    #[test]
    fn chunked_bodies_are_decoded() {
        assert_eq!(decode_chunked(b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n").unwrap(), b"Wikipedia");
        assert!(decode_chunked(b"zz\r\n").is_err());
    }
}
