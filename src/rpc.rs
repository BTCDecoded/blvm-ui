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
    Ok(call_batch_results(rpc_addr, calls)
        .await?
        .into_iter()
        .map(|r| r.and_then(Result::ok))
        .collect())
}

/// Like [`call_batch_calls`], but an error answer keeps its error object: `Some(Ok(result))`,
/// `Some(Err(error))`, or `None` when the node left that call out of its answer.
pub async fn call_batch_results(rpc_addr: &str, calls: &[RpcCall]) -> Result<Vec<Option<std::result::Result<Value, Value>>>> {
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
            out[id] = match result_of(item) {
                Some(r) => Some(Ok(r)),
                None => item.get("error").cloned().map(Err),
            };
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
    rpc_exchange_with(stream, rpc_addr, payload, auth_header()).await
}

async fn rpc_exchange_with(
    stream: &mut TcpStream,
    rpc_addr: &str,
    payload: &Value,
    auth: Option<String>,
) -> Result<Value> {
    let body = payload.to_string();
    let auth = auth.map(|a| format!("Authorization: {a}\r\n")).unwrap_or_default();
    let req = format!(
        "POST / HTTP/1.1\r\nHost: {rpc_addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;
    read_http_json(stream, rpc_addr).await
}

/// The login the console itself chose for a node it starts (see `managed::Console::rpc_auth`). Wins over the
/// environment, which it already includes when the environment names a login.
fn login_override() -> &'static std::sync::Mutex<Option<String>> {
    static LOGIN: OnceLock<std::sync::Mutex<Option<String>>> = OnceLock::new();
    LOGIN.get_or_init(|| std::sync::Mutex::new(None))
}

/// Send this user and password with every RPC request (`None`: back to the environment).
pub fn set_login(creds: Option<(String, String)>) {
    *login_override().lock().unwrap() = creds.map(|(u, p)| basic_auth(&format!("{u}:{p}")));
}

/// `Basic …` from the console's own login for the node it runs, else `BLVM_UI_RPC_USER` +
/// `BLVM_UI_RPC_PASS`, else the node's cookie file at `BLVM_UI_RPC_COOKIE`. No auth when none is set.
fn auth_header() -> Option<String> {
    if let Some(h) = login_override().lock().unwrap().clone() {
        return Some(h);
    }
    static USER_PASS: OnceLock<Option<String>> = OnceLock::new();
    let fixed = USER_PASS.get_or_init(|| {
        let user = std::env::var("BLVM_UI_RPC_USER").ok().filter(|u| !u.is_empty())?;
        let pass = std::env::var("BLVM_UI_RPC_PASS").unwrap_or_default();
        Some(basic_auth(&format!("{user}:{pass}")))
    });
    if fixed.is_some() {
        return fixed.clone();
    }
    // Re-read each time: the node writes a new cookie on every restart.
    let path = std::env::var("BLVM_UI_RPC_COOKIE").ok().filter(|p| !p.is_empty())?;
    let cookie = std::fs::read_to_string(path).ok()?;
    Some(basic_auth(cookie.trim()))
}

/// One-off connectivity check for the setup wizard: a fresh connection (not the shared one), its own
/// login if given (else the console's), `getblockchaininfo` + `getnetworkinfo` in one batch.
pub async fn probe_once(rpc_addr: &str, creds: Option<(&str, &str)>) -> Result<Vec<Option<Value>>> {
    let calls = [RpcCall::method("getblockchaininfo"), RpcCall::method("getnetworkinfo")];
    let payload = Value::Array(
        calls
            .iter()
            .enumerate()
            .map(|(i, c)| json!({"jsonrpc": "2.0", "id": i, "method": c.method, "params": c.params}))
            .collect(),
    );
    let mut stream = connect(rpc_addr).await?;
    let auth = creds.map(|(u, p)| basic_auth(&format!("{u}:{p}"))).or_else(auth_header);
    let v = tokio::time::timeout(
        Duration::from_secs(4),
        rpc_exchange_with(&mut stream, rpc_addr, &payload, auth),
    )
    .await
    .map_err(|_| anyhow!("RPC {rpc_addr} frozen"))??;
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

/// What a node said to a caller with no login (see [`probe_login`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginProbe {
    /// A clean `200`: the node serves RPC calls to anybody.
    Open,
    /// An explicit `401` or `403`: the node asks for a login.
    Locked,
    /// No usable answer (refused, dropped, timed out, `429`, a server error ...). The node limits the
    /// connections of one address (10 per minute), so silence says nothing either way: ask again later.
    Unknown,
}

/// Ask the node at `addr` for `getblockcount` without a login. Used right after a node that was given a
/// password came up, to make sure the password was really applied. Only an explicit refusal counts as
/// [`LoginProbe::Locked`]; a dropped connection or a timeout is [`LoginProbe::Unknown`], never "safe".
pub async fn probe_login(addr: &str) -> LoginProbe {
    let body = r#"{"jsonrpc":"2.0","id":0,"method":"getblockcount","params":[]}"#;
    let req = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let attempt = async {
        let mut s = TcpStream::connect(addr).await.ok()?;
        s.write_all(req.as_bytes()).await.ok()?;
        let mut head = Vec::new();
        let mut tmp = [0u8; 256];
        while head.len() < 12 {
            let n = s.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            head.extend_from_slice(&tmp[..n]);
        }
        let code = head
            .strip_prefix(b"HTTP/1.1 ")
            .or_else(|| head.strip_prefix(b"HTTP/1.0 "))
            .map(|rest| &rest[..rest.len().min(3)])?;
        Some(match code {
            b"200" => LoginProbe::Open,
            b"401" | b"403" => LoginProbe::Locked,
            _ => LoginProbe::Unknown,
        })
    };
    match tokio::time::timeout(Duration::from_secs(2), attempt).await {
        Ok(Some(p)) => p,
        _ => LoginProbe::Unknown,
    }
}

/// `Basic …` for a user and password.
pub fn basic_auth_header(user: &str, pass: &str) -> String {
    basic_auth(&format!("{user}:{pass}"))
}

fn basic_auth(user_pass: &str) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::from("Basic ");
    for chunk in user_pass.as_bytes().chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

async fn read_http_json(stream: &mut TcpStream, rpc_addr: &str) -> Result<Value> {
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
    if http_status(headers) == Some(401) {
        return Err(anyhow!(
            "RPC {rpc_addr} rejected the login (set BLVM_UI_RPC_USER/BLVM_UI_RPC_PASS or BLVM_UI_RPC_COOKIE)"
        ));
    }
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

fn http_status(headers: &str) -> Option<u16> {
    headers.split_whitespace().nth(1)?.parse().ok()
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
        assert!(is_frozen_error("RPC 127.0.0.1:4242 frozen"));
        assert!(is_frozen_error("RPC 127.0.0.1:4242 timed out"));
        assert!(!is_frozen_error("connect 127.0.0.1:4242"));
        assert!(!is_frozen_error("Connection refused"));
        assert!(is_connect_refused("connect 127.0.0.1:4242"));
        assert!(is_connect_refused("Connection refused"));
        assert!(!is_connect_refused("Connection reset by peer"));
        assert!(!is_connect_refused("Connection rate limit exceeded"));
    }

    #[test]
    fn parses_content_length_header() {
        let h = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 12\r\n";
        assert_eq!(header_content_length(h), Some(12));
        assert_eq!(find_double_crlf(b"HTTP/1.1 200 OK\r\n\r\n{}"), Some(15));
        assert_eq!(http_status(h), Some(200));
        assert_eq!(http_status("HTTP/1.1 401 Unauthorized\r\n"), Some(401));
    }

    #[test]
    fn basic_auth_matches_rfc7617() {
        assert_eq!(basic_auth("Aladdin:open sesame"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(basic_auth("umbrel:pw"), "Basic dW1icmVsOnB3");
        assert_eq!(basic_auth("a:b"), "Basic YTpi");
        assert_eq!(basic_auth("ab:c"), "Basic YWI6Yw==");
    }

    #[test]
    fn the_consoles_own_login_is_sent_until_it_is_withdrawn() {
        assert_eq!(basic_auth_header("commons", "s3cret"), "Basic Y29tbW9uczpzM2NyZXQ=");
        set_login(Some(("commons".into(), "s3cret".into())));
        let sent = auth_header();
        set_login(None);
        assert_eq!(sent.as_deref(), Some("Basic Y29tbW9uczpzM2NyZXQ="));
        // (With nothing set and no environment, no header: the node's default.)
        if std::env::var_os("BLVM_UI_RPC_USER").is_none() && std::env::var_os("BLVM_UI_RPC_COOKIE").is_none() {
            assert_eq!(auth_header(), None);
        }
    }

    #[tokio::test]
    async fn a_node_that_answers_a_caller_with_no_login_is_told_apart_from_one_that_asks() {
        async fn stub(status: &'static str) -> String {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = l.local_addr().unwrap().to_string();
            tokio::spawn(async move {
                loop {
                    let Ok((mut s, _)) = l.accept().await else { return };
                    tokio::spawn(async move {
                        let mut b = [0u8; 1024];
                        let _ = s.read(&mut b).await;
                        let _ = s.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\n\r\n{{}}").as_bytes()).await;
                    });
                }
            });
            addr
        }
        assert_eq!(probe_login(&stub("200 OK").await).await, LoginProbe::Open);
        for refusal in ["401 Unauthorized", "403 Forbidden"] {
            assert_eq!(probe_login(&stub(refusal).await).await, LoginProbe::Locked, "{refusal}");
        }
        // Anything else is no answer to the question: the node's connection limit answers 429 or just hangs up.
        for other in ["429 Too Many Requests", "500 Internal Server Error", "404 Not Found"] {
            assert_eq!(probe_login(&stub(other).await).await, LoginProbe::Unknown, "{other}");
        }
        let free = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        assert_eq!(probe_login(&free).await, LoginProbe::Unknown, "nothing listening is not a locked node");
        // A peer that accepts and drops the connection (the per-address limit) is not a locked node either.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dropper = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                drop(s);
            }
        });
        assert_eq!(probe_login(&dropper).await, LoginProbe::Unknown, "a dropped connection is not a locked node");
        // ... and one that stays silent until the check gives up.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((s, _)) = l.accept().await {
                held.push(s);
            }
        });
        assert_eq!(probe_login(&silent).await, LoginProbe::Unknown, "a timeout is not a locked node");
    }
}
