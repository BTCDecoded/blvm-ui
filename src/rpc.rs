//! Tiny JSON-RPC/2.0 client over HTTP (BLVM RPC). No extra HTTP client crate.
//!
//! The node caps **new** TCP connections at 10 per IP per 60s. This client keeps
//! one socket per RPC address (HTTP/1.1 keep-alive) so polls are not that cap.
//! IP request tokens (default burst 50, 5/s; a batch costs up to 10).
//! Height polls use 2 tokens; a full probe batch uses 10. Together that is
//! about 4 tokens/s if we full-poll every 4th second.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

pub async fn call(rpc_addr: &str, method: &str) -> Result<Value> {
    call_params(rpc_addr, method, json!([])).await
}

pub async fn call_params(rpc_addr: &str, method: &str, params: Value) -> Result<Value> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": "blvm-ui",
        "method": method,
        "params": params
    });
    let v = post(rpc_addr, &payload).await?;
    result_of(&v).ok_or_else(|| anyhow!("RPC {method}: {}", v.get("error").unwrap_or(&Value::Null)))
}

#[derive(Debug, Clone)]
pub struct RpcCall {
    pub method: String,
    pub params: Value,
}

impl RpcCall {
    pub fn method(method: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            params: json!([]),
        }
    }

    pub fn with_params(method: impl Into<String>, params: Value) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

/// One TCP round-trip. Index matches `methods`.
pub async fn call_batch(rpc_addr: &str, methods: &[&str]) -> Result<Vec<Option<Value>>> {
    let calls: Vec<RpcCall> = methods.iter().copied().map(RpcCall::method).collect();
    call_batch_calls(rpc_addr, &calls).await
}

/// One TCP round-trip for mixed methods/params (needed for `getblockhash`).
pub async fn call_batch_calls(rpc_addr: &str, calls: &[RpcCall]) -> Result<Vec<Option<Value>>> {
    let payload = Value::Array(
        calls
            .iter()
            .enumerate()
            .map(|(i, call)| {
                json!({
                    "jsonrpc": "2.0",
                    "id": i,
                    "method": call.method,
                    "params": call.params
                })
            })
            .collect(),
    );
    let v = post(rpc_addr, &payload).await?;
    let arr = v
        .as_array()
        .ok_or_else(|| anyhow!("RPC batch response was not an array"))?;
    let mut out = vec![None; calls.len()];
    for item in arr {
        let id = item.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        if id < out.len() {
            out[id] = result_of(item);
        }
    }
    Ok(out)
}

fn result_of(v: &Value) -> Option<Value> {
    if v.get("error").is_some_and(|e| !e.is_null()) {
        None
    } else {
        Some(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn rpc_pool() -> &'static Mutex<HashMap<String, TcpStream>> {
    static POOL: OnceLock<Mutex<HashMap<String, TcpStream>>> = OnceLock::new();
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn connect(rpc_addr: &str) -> Result<TcpStream> {
    match tokio::time::timeout(Duration::from_millis(800), TcpStream::connect(rpc_addr)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(e).with_context(|| format!("connect {rpc_addr}")),
        Err(_) => Err(anyhow!("RPC {rpc_addr} frozen")),
    }
}

async fn post(rpc_addr: &str, payload: &Value) -> Result<Value> {
    let reused = {
        let mut pool = rpc_pool().lock().await;
        pool.remove(rpc_addr)
    };
    let mut stream = match reused {
        Some(s) => s,
        None => connect(rpc_addr).await?,
    };
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        rpc_exchange(&mut stream, rpc_addr, payload),
    )
    .await
    .map_err(|_| anyhow!("RPC {rpc_addr} frozen"))?;
    match result {
        Ok(v) => {
            rpc_pool().lock().await.insert(rpc_addr.to_string(), stream);
            Ok(v)
        }
        Err(e) => Err(e),
    }
}

async fn rpc_exchange(
    stream: &mut TcpStream,
    rpc_addr: &str,
    payload: &Value,
) -> Result<Value> {
    let body = payload.to_string();
    let req = format!(
        "POST / HTTP/1.1\r\nHost: {rpc_addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    read_http_json(stream).await
}

async fn read_http_json(stream: &mut TcpStream) -> Result<Value> {
    let mut buf = Vec::with_capacity(4096);
    let header_end = loop {
        let mut tmp = [0u8; 2048];
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(anyhow!("RPC response had no body"));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        if buf.len() > 64 * 1024 {
            return Err(anyhow!("RPC headers too large"));
        }
    };
    let headers = std::str::from_utf8(&buf[..header_end]).context("RPC headers")?;
    let leftover = buf[header_end + 4..].to_vec();
    let mut body = leftover;
    if let Some(len) = header_content_length(headers) {
        while body.len() < len {
            let mut tmp = [0u8; 8192];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(len);
    } else {
        loop {
            let mut tmp = [0u8; 8192];
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
    }
    serde_json::from_slice(&body).context("RPC JSON")
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn header_content_length(headers: &str) -> Option<usize> {
    headers.lines().find_map(|line| {
        let (k, v) = line.split_once(':')?;
        k.eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse().ok())
            .flatten()
    })
}

/// TCP accepted but the node did not answer (IBD stuck, lock held, etc.).
pub fn is_frozen_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("frozen") || e.contains("timed out")
}

/// Nothing accepted the TCP connect. The process is actually down (or wrong port).
pub fn is_connect_refused(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("connection refused") || e.starts_with("connect ")
}

/// Node RPCs the Settings page is allowed to invoke.
pub fn allowed_setting(method: &str) -> bool {
    matches!(
        method,
        "setban" | "listbanned" | "clearbanned" | "disconnectnode" | "addnode" | "setnetworkactive"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_whitelist_is_peer_and_network_only() {
        assert!(allowed_setting("addnode"));
        assert!(allowed_setting("disconnectnode"));
        assert!(allowed_setting("setban"));
        assert!(allowed_setting("listbanned"));
        assert!(allowed_setting("clearbanned"));
        assert!(allowed_setting("setnetworkactive"));
        assert!(!allowed_setting("getblockchaininfo"));
        assert!(!allowed_setting("stop"));
        assert!(!allowed_setting(""));
    }

    #[test]
    fn batch_result_skips_errors() {
        let ok = json!({"jsonrpc":"2.0","id":0,"result":{"blocks":1}});
        let err = json!({"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"no"}});
        assert_eq!(result_of(&ok), Some(json!({"blocks":1})));
        assert_eq!(result_of(&err), None);
    }

    #[test]
    fn frozen_vs_connect_errors() {
        assert!(is_frozen_error("RPC 127.0.0.1:38332 frozen"));
        assert!(is_frozen_error("RPC 127.0.0.1:38332 timed out"));
        assert!(!is_frozen_error("connect 127.0.0.1:38332"));
        assert!(!is_frozen_error("Connection refused"));
        assert!(is_connect_refused("connect 127.0.0.1:38332"));
        assert!(is_connect_refused("Connection refused"));
        assert!(!is_connect_refused("Connection reset by peer"));
        assert!(!is_connect_refused("Connection rate limit exceeded"));
    }

    #[test]
    fn parses_content_length_header() {
        let h = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\n";
        assert_eq!(header_content_length(h), Some(12));
        assert_eq!(find_double_crlf(b"HTTP/1.1 200 OK\r\n\r\n{}"), Some(15));
    }
}
