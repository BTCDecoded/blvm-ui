use crate::installer;
use crate::managed;
use crate::node_ctl::{self, PowerPhase};
use crate::state::{poll_once, stamp_power, Shared};
use crate::switch;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;

const MISSING_FRONT: &str = r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Commons · Next console</title>
<style>body{font-family:system-ui;background:#050505;color:#f0e6d0;padding:2rem;max-width:40rem}</style>
</head><body>
<h1>Frontend not built</h1>
<p>From <code>blvm-ui-next/web</code> run <code>npm install && npm run build</code>, then restart this process.</p>
</body></html>"#;

/// Header the page sends on every mutating request. A browser will not add a custom header to a
/// cross-site form post, so requiring it (together with `Content-Type: application/json`, which also
/// forces a CORS preflight) shuts out drive-by posts from other sites.
pub const UI_HEADER: &str = "x-blvm-ui";
/// Same header under the name used in the milestone brief; either is accepted.
pub const UI_HEADER_ALIAS: &str = "x-commons-ui";

/// One-line switch: require the custom header on the routes that existed before the managed work
/// (`/api/node`, `/api/connect`, `/api/rpc`) too. Content-Type, Host and Origin are always checked on
/// them. Flip to `true` once the page that sends the header (web `POST_HEADERS`) is what is served.
pub const REQUIRE_UI_HEADER_ON_LEGACY_ROUTES: bool = false;

const LEGACY_MUTATING: [&str; 3] = ["/api/node", "/api/connect", "/api/rpc"];
const MAX_BODY: usize = 64 * 1024;

/// Host names (no port) that may address the console besides loopback: `BLVM_UI_ALLOWED_HOSTS`,
/// comma separated. Umbrel sets its app host here.
fn allowed_hosts() -> &'static [String] {
    static HOSTS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    HOSTS.get_or_init(|| {
        std::env::var("BLVM_UI_ALLOWED_HOSTS")
            .map(|v| v.split(',').map(host_of).filter(|h| !h.is_empty()).collect())
            .unwrap_or_default()
    })
}

/// `127.0.0.1:3849` -> `127.0.0.1`, `[::1]:3849` -> `::1`, `Example.COM` -> `example.com`.
fn host_of(authority: &str) -> String {
    let a = authority.trim();
    let a = a.strip_prefix("http://").or_else(|| a.strip_prefix("https://")).unwrap_or(a);
    let a = a.split('/').next().unwrap_or("");
    let host = if let Some(rest) = a.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        a.rsplit_once(':').map_or(a, |(h, p)| if p.bytes().all(|b| b.is_ascii_digit()) { h } else { a })
    };
    host.to_ascii_lowercase()
}

fn host_allowed(host: &str, extra: &[String]) -> bool {
    if host.is_empty() {
        return false;
    }
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()) {
        return true;
    }
    // Behind a gateway (Umbrel) the console sees the Host the browser typed: a name, the LAN address, or a
    // Tor name. Once the operator has opted in by listing hosts, any IP address and any `.onion` name is
    // fine too, and `*` allows every Host. Without the variable only loopback is accepted.
    if !extra.is_empty() {
        if host.parse::<std::net::IpAddr>().is_ok() || host.ends_with(".onion") {
            return true;
        }
        if extra.iter().any(|h| h == "*") {
            return true;
        }
    }
    extra.iter().any(|h| h == host)
}

/// The 403 text for a `Host` that is not loopback and not listed: it says how to fix it, because the usual
/// cause is a LAN user (or an Umbrel app proxy) that simply has not set `BLVM_UI_ALLOWED_HOSTS` yet.
pub const HOST_REFUSED: &str = "This address is not allowed to use the console. To open it from another computer, start the console with BLVM_UI_ALLOWED_HOSTS set to the name you type in the browser (comma separated, for example BLVM_UI_ALLOWED_HOSTS=umbrel.local), or use http://127.0.0.1:3849 on this computer.";
const ORIGIN_REFUSED: &str = "Requests from other sites are not allowed. If this is the console's own address, add its name to BLVM_UI_ALLOWED_HOSTS.";

/// `Host` must be loopback (or allowed), which stops DNS rebinding: a page on `evil.example` that
/// resolves to 127.0.0.1 still sends `Host: evil.example`.
pub fn check_host(headers: &HeaderMap, extra: &[String]) -> Result<(), (StatusCode, &'static str)> {
    let host = headers.get(hyper::header::HOST).and_then(|v| v.to_str().ok()).map(host_of).unwrap_or_default();
    if host_allowed(&host, extra) {
        Ok(())
    } else {
        Err((StatusCode::FORBIDDEN, HOST_REFUSED))
    }
}

/// `BLVM_UI_LISTEN` that is reachable from other computers while no `Host` besides loopback is allowed:
/// every request from the LAN would be refused. Returns the warning to log at startup, if any.
pub fn lan_listen_warning(listen: SocketAddr, allowed_hosts_set: bool) -> Option<String> {
    if listen.ip().is_loopback() || allowed_hosts_set {
        return None;
    }
    Some(format!(
        "listening on {listen}, which other computers can reach, but BLVM_UI_ALLOWED_HOSTS is not set: every request whose Host is not loopback is answered 403. Set BLVM_UI_ALLOWED_HOSTS to the name or address people type in the browser (comma separated)."
    ))
}

/// Is `BLVM_UI_ALLOWED_HOSTS` set to at least one name?
pub fn allowed_hosts_configured() -> bool {
    !allowed_hosts().is_empty()
}

/// Checks for a request that changes something: Host, Origin (when the browser sent one), JSON
/// content type, and (`require_header`) the custom header.
pub fn check_mutation(
    headers: &HeaderMap,
    require_header: bool,
    extra: &[String],
) -> Result<(), (StatusCode, &'static str)> {
    check_host(headers, extra)?;
    if let Some(origin) = headers.get(hyper::header::ORIGIN) {
        let o = origin.to_str().unwrap_or("");
        if !host_allowed(&host_of(o), extra) {
            return Err((StatusCode::FORBIDDEN, ORIGIN_REFUSED));
        }
    }
    let json = headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "application/json" || v.starts_with("application/json;")
        });
    if !json {
        return Err((StatusCode::UNSUPPORTED_MEDIA_TYPE, "Content-Type must be application/json."));
    }
    if require_header
        && headers.get(UI_HEADER).or_else(|| headers.get(UI_HEADER_ALIAS)).and_then(|v| v.to_str().ok()) != Some("1")
    {
        return Err((StatusCode::FORBIDDEN, "Missing X-BLVM-UI: 1 header."));
    }
    Ok(())
}

fn guard(req: &Request<Incoming>, path: &str) -> Result<(), Response<Full<Bytes>>> {
    let mutating = matches!(*req.method(), Method::POST | Method::PUT | Method::PATCH | Method::DELETE);
    let r = if mutating && path.starts_with("/api/") {
        let legacy = LEGACY_MUTATING.contains(&path);
        check_mutation(req.headers(), !legacy || REQUIRE_UI_HEADER_ON_LEGACY_ROUTES, allowed_hosts())
    } else if path.starts_with("/api/") {
        // Every API read, the status included, is kept off DNS-rebinding pages: the page itself always
        // arrives under an allowed Host.
        check_host(req.headers(), allowed_hosts())
    } else {
        Ok(())
    };
    r.map_err(|(code, msg)| error_json(code, msg))
}

fn error_json(code: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    let mut r = json_ok(&serde_json::json!({ "ok": false, "error": msg }));
    *r.status_mut() = code;
    r
}

fn json_status(code: u16, v: &serde_json::Value) -> Response<Full<Bytes>> {
    let mut r = json_ok(v);
    *r.status_mut() = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r
}

/// Body bytes, at most 64 KiB (413 beyond that, or when the body cannot be read).
async fn read_body(req: Request<Incoming>) -> Result<Bytes, Response<Full<Bytes>>> {
    Ok(Limited::new(req.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|_| error_json(StatusCode::PAYLOAD_TOO_LARGE, "Request body is too large."))?
        .to_bytes())
}

fn parse_json(bytes: &[u8]) -> Result<serde_json::Value, Response<Full<Bytes>>> {
    serde_json::from_slice(bytes).map_err(|_| error_json(StatusCode::BAD_REQUEST, "The body is not valid JSON."))
}

/// Body as JSON, at most 64 KiB.
async fn read_json(req: Request<Incoming>) -> Result<serde_json::Value, Response<Full<Bytes>>> {
    parse_json(&read_body(req).await?)
}

fn query_param<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

pub async fn serve(addr: SocketAddr, state: Shared) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    serve_listener(listener, state).await
}

pub async fn serve_listener(listener: TcpListener, state: Shared) -> anyhow::Result<()> {
    let dist = dist_dir();
    tracing::info!(
        "blvm-ui-next listening on http://{} (web dir {})",
        listener.local_addr()?,
        dist.display()
    );
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = Arc::clone(&state);
        let dist = dist.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = Arc::clone(&state);
                let dist = dist.clone();
                async move { handle(req, state, &dist).await }
            });
            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                tracing::debug!("http conn: {e}");
            }
        });
    }
}

async fn handle(
    req: Request<Incoming>,
    state: Shared,
    dist: &Path,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    if let Err(resp) = guard(&req, &path) {
        return Ok(resp);
    }
    let query = req.uri().query().map(str::to_string);
    let managed_mode = state.read().await.console.is_managed();
    let resp = match (method, path.as_str()) {
        (Method::GET, "/api/status") => {
            let snap = state.read().await.snapshot.clone();
            json_ok(&snap)
        }
        (Method::POST, "/api/node") => power_node(req, state, None).await,
        (Method::POST, "/api/node/start") => power_node(req, state, Some("start")).await,
        (Method::POST, "/api/node/stop") => power_node(req, state, Some("stop")).await,
        (Method::POST, "/api/node/restart") => power_node(req, state, Some("restart")).await,
        (Method::GET, "/api/node/logs") => {
            let after = query_param(query.as_deref(), "after").and_then(|v| v.parse::<u64>().ok());
            let limit = query_param(query.as_deref(), "limit")
                .or_else(|| query_param(query.as_deref(), "lines"))
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(200);
            match managed::logs(&state, after, limit).await {
                Some(v) => json_ok(&v),
                None => error_json(StatusCode::NOT_FOUND, "This console is not managing a node."),
            }
        }
        (Method::GET, "/api/node/releases") => {
            let force = query_param(query.as_deref(), "force").is_some_and(|v| v == "1" || v == "true");
            json_ok(&managed::releases_json(&state, force).await)
        }
        (Method::GET, "/api/node/install" | "/api/node/update") => json_ok(&installer::job_json(&state).await),
        (Method::POST, "/api/node/install") => job_request(req, &state, installer::Kind::Install).await,
        (Method::POST, "/api/node/update") => job_request(req, &state, installer::Kind::Update).await,
        (Method::GET, "/api/network/preview") => {
            let (code, v) = switch::preview(&state, query_param(query.as_deref(), "to").unwrap_or("")).await;
            json_status(code, &v)
        }
        (Method::POST, "/api/network/preview") => match read_json(req).await {
            Ok(body) => {
                let (code, v) = switch::preview(&state, switch::target_of(&body)).await;
                json_status(code, &v)
            }
            Err(r) => r,
        },
        (Method::POST, "/api/network/switch") => match read_json(req).await {
            Ok(body) => {
                let (code, v) = switch::begin(&state, switch::target_of(&body)).await;
                json_status(code, &v)
            }
            Err(r) => r,
        },
        (Method::GET, "/api/settings") => {
            let g = state.read().await;
            json_ok(&managed::settings_json(&g.console))
        }
        (Method::PUT | Method::POST, "/api/settings") => match read_json(req).await {
            Ok(body) => {
                let (code, v) = managed::patch_settings(&state, &body).await;
                json_status(code, &v)
            }
            Err(r) => r,
        },
        (Method::POST, "/api/setup") => match read_json(req).await {
            Ok(body) => {
                let (code, v) = managed::setup(&state, &body).await;
                json_status(code, &v)
            }
            Err(r) => r,
        },
        (Method::POST, "/api/setup/test") => match read_json(req).await {
            Ok(body) => json_ok(&managed::setup_test(&body).await),
            Err(r) => r,
        },
        (Method::POST, "/api/connect") if managed_mode => error_json(
            StatusCode::CONFLICT,
            "A managed console follows the network you chose; use the network switch instead.",
        ),
        (Method::POST, "/api/connect") => match read_json(req).await {
            Err(r) => r,
            Ok(body) => {
                let addr = body
                    .get("rpc")
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "127.0.0.1:48332".into());
                {
                    let mut g = state.write().await;
                    g.switch_rpc(addr);
                }
                let state_bg = Arc::clone(&state);
                tokio::spawn(async move {
                    poll_once(&state_bg).await;
                });
                let snap = state.read().await.snapshot.clone();
                json_ok(&snap)
            }
        },
        (Method::POST, "/api/rpc") => match read_json(req).await {
            Err(r) => r,
            Ok(body) => {
                let method_name = body
                    .get("method")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let params = body.get("params").cloned().unwrap_or(serde_json::json!([]));
                if !crate::rpc::allowed_setting(&method_name) {
                    let mut r = json_ok(&serde_json::json!({
                        "ok": false,
                        "error": "method not allowed"
                    }));
                    *r.status_mut() = StatusCode::BAD_REQUEST;
                    r
                } else {
                    let rpc_addr = { state.read().await.rpc_addr.clone() };
                    match crate::rpc::call_params(&rpc_addr, &method_name, params).await {
                        Ok(result) => {
                            let state_bg = Arc::clone(&state);
                            tokio::spawn(async move {
                                poll_once(&state_bg).await;
                            });
                            json_ok(&serde_json::json!({ "ok": true, "result": result }))
                        }
                        Err(e) => json_ok(&serde_json::json!({
                            "ok": false,
                            "error": e.to_string()
                        })),
                    }
                }
            }
        },
        // An API path this host does not have is a JSON 404, never the page: the cards that probe for
        // newer routes (modules, releases, the network preview) tell "not here" from "here, and empty".
        (_, p) if p == "/api" || p.starts_with("/api/") => {
            error_json(StatusCode::NOT_FOUND, "This console has no such API route.")
        }
        (Method::GET, _) => serve_web(dist, &path),
        _ => {
            let mut r = Response::new(Full::new(Bytes::from("not found")));
            *r.status_mut() = StatusCode::NOT_FOUND;
            r
        }
    };
    Ok(resp)
}

/// Body `{tag?, version?}` (`{}` or no body: the newest release).
async fn job_request(req: Request<Incoming>, state: &Shared, kind: installer::Kind) -> Response<Full<Bytes>> {
    let bytes = match read_body(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    let body = if bytes.iter().all(u8::is_ascii_whitespace) {
        serde_json::json!({})
    } else {
        match parse_json(&bytes) {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
    let tag = ["tag", "version"]
        .iter()
        .find_map(|k| body.get(*k).and_then(|v| v.as_str()).map(str::trim).filter(|t| !t.is_empty()));
    let downgrade = body.get("downgrade").and_then(|v| v.as_bool()).unwrap_or(false);
    let (code, v) = installer::begin_with(state, kind, tag, downgrade).await;
    json_status(code, &v)
}

async fn power_node(req: Request<Incoming>, state: Shared, forced: Option<&str>) -> Response<Full<Bytes>> {
    // Read the body before taking the power lock, so a slow client cannot hold every other click.
    let bytes = match read_body(req).await {
        Ok(b) => b,
        Err(r) => return r,
    };
    // A body that is not JSON, or an action that is not text, is a bad request: never a guess. A JSON
    // object without `action` keeps its old meaning, "toggle". The /start /stop /restart routes ignore it.
    let action = match forced {
        Some(a) => a.to_string(),
        None => match parse_json(&bytes) {
            Err(r) => return r,
            Ok(v) => match v.get("action") {
                None => "toggle".to_string(),
                Some(a) => match a.as_str() {
                    Some(a) => a.to_string(),
                    None => return error_json(StatusCode::BAD_REQUEST, "action must be text."),
                },
            },
        },
    };
    let lock = { state.read().await.power_lock.clone() };
    let _busy = lock.lock().await;
    if state.read().await.console.is_managed() {
        let out = managed::node_action(&state, &action).await;
        let snap = { state.read().await.snapshot.clone() };
        return json_ok(&serde_json::json!({
            "ok": out.ok,
            "message": out.message,
            "error": out.error,
            "status": snap
        }));
    }
    if action == "restart" {
        return json_ok(&serde_json::json!({
            "ok": false,
            "message": "",
            "error": "Restart is only available when the console manages the node."
        }));
    }
    let action = if action == "start" { "on".to_string() } else if action == "stop" { "off".to_string() } else { action };
    let rpc = { state.read().await.rpc_addr.clone() };
    let going_on = match action.as_str() {
        "on" => true,
        "off" => false,
        _ => !node_ctl::node_is_up(&rpc).await,
    };
    {
        let mut g = state.write().await;
        g.power_phase = if going_on {
            PowerPhase::Starting
        } else {
            PowerPhase::Stopping
        };
        stamp_power(&mut g);
    }
    let mut launch = { state.read().await.launch.clone() };
    let result = if going_on {
        node_ctl::start(&rpc, &mut launch).await
    } else {
        node_ctl::stop(&rpc).await
    };
    {
        let mut g = state.write().await;
        g.power_phase = PowerPhase::Idle;
        g.launch = launch;
        stamp_power(&mut g);
    }
    poll_once(&state).await;
    let snap = { state.read().await.snapshot.clone() };
    json_ok(&serde_json::json!({
        "ok": result.ok,
        "message": result.message,
        "error": result.error,
        "status": snap
    }))
}

fn dist_dir() -> PathBuf {
    if let Ok(p) = std::env::var("BLVM_UI_WEB_DIR") {
        return PathBuf::from(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for cand in [dir.join("web"), dir.join("dist"), dir.join("web/dist")] {
                if cand.join("index.html").is_file() {
                    return cand;
                }
            }
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("web/dist")
}

fn serve_web(dist: &Path, url_path: &str) -> Response<Full<Bytes>> {
    let rel = url_path.trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    if let Some(resp) = file_response(dist, rel) {
        return resp;
    }
    if !rel.contains('.') {
        if let Some(resp) = file_response(dist, "index.html") {
            return resp;
        }
    }
    if rel == "index.html" || rel.is_empty() {
        return typed(MISSING_FRONT, "text/html; charset=utf-8");
    }
    let mut r = Response::new(Full::new(Bytes::from("not found")));
    *r.status_mut() = StatusCode::NOT_FOUND;
    r
}

fn file_response(dist: &Path, rel: &str) -> Option<Response<Full<Bytes>>> {
    let joined = dist.join(rel);
    let canon = joined.canonicalize().ok()?;
    let root = dist.canonicalize().ok()?;
    if !canon.starts_with(&root) || !canon.is_file() {
        return None;
    }
    let bytes = std::fs::read(&canon).ok()?;
    let mime = mime_guess::from_path(&canon)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    let mut r = Response::new(Full::new(Bytes::from(bytes)));
    if let Ok(v) = hyper::header::HeaderValue::from_str(&mime) {
        r.headers_mut().insert(hyper::header::CONTENT_TYPE, v);
    }
    if rel.contains("fonts/") || rel.ends_with(".woff2") {
        r.headers_mut().insert(
            hyper::header::CACHE_CONTROL,
            hyper::header::HeaderValue::from_static("public, max-age=31536000"),
        );
    }
    Some(r)
}

fn typed(s: &'static str, ct: &'static str) -> Response<Full<Bytes>> {
    let mut r = Response::new(Full::new(Bytes::from_static(s.as_bytes())));
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static(ct),
    );
    r.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    r
}

fn json_ok<T: serde::Serialize>(v: &T) -> Response<Full<Bytes>> {
    let body = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    let mut r = Response::new(Full::new(Bytes::from(body)));
    r.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    r
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::ProcState;
    use crate::managed::Console;
    use crate::settings::EnvOverrides;
    use crate::state::LiveState;
    use hyper::header::{HeaderName, HeaderValue};
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::RwLock;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn host_parsing() {
        assert_eq!(host_of("127.0.0.1:3849"), "127.0.0.1");
        assert_eq!(host_of("[::1]:3849"), "::1");
        assert_eq!(host_of("Example.COM"), "example.com");
        assert_eq!(host_of("http://localhost:5174"), "localhost");
        assert_eq!(host_of("https://evil.example/path"), "evil.example");
        assert_eq!(host_of("null"), "null");
        assert!(host_allowed("localhost", &[]) && host_allowed("127.0.0.1", &[]) && host_allowed("::1", &[]));
        assert!(host_allowed("app.localhost", &[]));
        assert!(!host_allowed("evil.example", &[]) && !host_allowed("", &[]) && !host_allowed("192.168.1.5", &[]));
        assert!(host_allowed("umbrel.local", &["umbrel.local".to_string()]));
    }

    #[test]
    fn behind_a_gateway_any_ip_and_onion_host_is_fine_once_hosts_are_configured() {
        let some = ["umbrel.local".to_string()];
        for h in ["192.168.1.5", "10.0.0.2", "::1", "fe80::1", "abcdefghij234567.onion", "sub.abcdefghij234567.onion"] {
            assert!(host_allowed(h, &some), "{h}");
        }
        assert!(host_allowed(&host_of("192.168.1.5:3849"), &some));
        assert!(host_allowed(&host_of("[2001:db8::1]:3849"), &some));
        // Names still need to be listed, and nothing opens up without the variable.
        assert!(!host_allowed("evil.example", &some));
        assert!(!host_allowed("onion.evil.example", &some));
        for h in ["192.168.1.5", "abcdefghij234567.onion"] {
            assert!(!host_allowed(h, &[]), "{h} without BLVM_UI_ALLOWED_HOSTS");
        }
        // `*` lets every Host in.
        let star = ["*".to_string()];
        assert!(host_allowed("evil.example", &star) && host_allowed("anything.test", &star));
        assert!(!host_allowed("", &star), "an empty Host is never fine");
        // The same rule applies to the Origin of a mutating request.
        let h = headers(&[("host", "192.168.1.5:3849"), ("origin", "http://192.168.1.5:3849"), ("content-type", "application/json"), ("x-blvm-ui", "1")]);
        assert!(check_mutation(&h, true, &some).is_ok());
        assert!(check_mutation(&h, true, &[]).is_err());
    }

    #[test]
    fn mutation_checks_host_origin_content_type_and_header() {
        let ok = [("host", "127.0.0.1:3849"), ("content-type", "application/json"), ("x-blvm-ui", "1")];
        assert!(check_mutation(&headers(&ok), true, &[]).is_ok());
        // Charset parameter is fine; the alias header name is accepted.
        let alias = [("host", "localhost:3849"), ("content-type", "application/json; charset=utf-8"), ("x-commons-ui", "1")];
        assert!(check_mutation(&headers(&alias), true, &[]).is_ok());
        // Same-origin and dev-proxy origins pass.
        let with_origin = [("host", "127.0.0.1:3849"), ("origin", "http://localhost:5174"), ("content-type", "application/json"), ("x-blvm-ui", "1")];
        assert!(check_mutation(&headers(&with_origin), true, &[]).is_ok());

        let code = |pairs: &[(&str, &str)], strict: bool| check_mutation(&headers(pairs), strict, &[]).unwrap_err().0;
        // Rebinding: a foreign name resolving to loopback.
        assert_eq!(code(&[("host", "evil.example"), ("content-type", "application/json"), ("x-blvm-ui", "1")], true), StatusCode::FORBIDDEN);
        assert_eq!(code(&[("content-type", "application/json"), ("x-blvm-ui", "1")], true), StatusCode::FORBIDDEN, "no Host");
        // Cross-site post.
        assert_eq!(
            code(&[("host", "127.0.0.1:3849"), ("origin", "https://evil.example"), ("content-type", "application/json"), ("x-blvm-ui", "1")], true),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            code(&[("host", "127.0.0.1:3849"), ("origin", "null"), ("content-type", "application/json"), ("x-blvm-ui", "1")], true),
            StatusCode::FORBIDDEN
        );
        // Simple-form content types.
        for ct in ["text/plain", "application/x-www-form-urlencoded", "multipart/form-data; boundary=x", "application/jsonx"] {
            assert_eq!(code(&[("host", "127.0.0.1:3849"), ("content-type", ct), ("x-blvm-ui", "1")], true), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{ct}");
        }
        assert_eq!(code(&[("host", "127.0.0.1:3849"), ("x-blvm-ui", "1")], true), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        // The custom header.
        assert_eq!(code(&[("host", "127.0.0.1:3849"), ("content-type", "application/json")], true), StatusCode::FORBIDDEN);
        assert_eq!(code(&[("host", "127.0.0.1:3849"), ("content-type", "application/json"), ("x-blvm-ui", "0")], true), StatusCode::FORBIDDEN);
        // ... which legacy routes do not need while the switch is off.
        assert!(check_mutation(&headers(&[("host", "127.0.0.1:3849"), ("content-type", "application/json")]), false, &[]).is_ok());
        assert!(!REQUIRE_UI_HEADER_ON_LEGACY_ROUTES, "documented default; flip it when the page sends the header");
        // Allowed hosts (Umbrel).
        let umbrel = ["umbrel.local".to_string()];
        assert!(check_mutation(&headers(&[("host", "umbrel.local"), ("origin", "http://umbrel.local"), ("content-type", "application/json"), ("x-blvm-ui", "1")]), true, &umbrel).is_ok());
    }

    #[test]
    fn query_parameters() {
        assert_eq!(query_param(Some("after=12&lines=50"), "after"), Some("12"));
        assert_eq!(query_param(Some("after=12&lines=50"), "lines"), Some("50"));
        assert_eq!(query_param(Some("after=12"), "limit"), None);
        assert_eq!(query_param(None, "after"), None);
    }

    // ---- end to end over a real socket ----

    struct Server {
        addr: SocketAddr,
        dir: PathBuf,
        state: Shared,
        _reap: crate::agent::testkit::ReapNodes,
    }

    async fn server(tag: &str, managed: bool) -> Server {
        let dir = std::env::temp_dir().join(format!("blvm-http-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-blvm.sh");
        std::fs::write(&bin, "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\ntrap 'exit 0' TERM\necho up\nwhile kill -0 $PPID 2>/dev/null; do sleep 0.1; done\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = EnvOverrides {
            mode: Some(if managed { "managed" } else { "standalone" }.into()),
            node_bin: Some(bin.display().to_string()),
            data_root: Some(dir.join("root").display().to_string()),
            ..Default::default()
        };
        let console = Console::load(Some(dir.join("state")), env, None);
        let mut live = LiveState::new(console.initial_rpc());
        live.console = console;
        crate::state::stamp_power(&mut live);
        let state: Shared = Arc::new(RwLock::new(live));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let st = Arc::clone(&state);
        tokio::spawn(async move {
            let _ = serve_listener(listener, st).await;
        });
        let _reap = crate::agent::testkit::ReapNodes::under(&dir);
        Server { addr, dir, state, _reap }
    }

    /// Minimal HTTP/1.1 client: returns (status, body).
    async fn call(addr: SocketAddr, method: &str, path: &str, hdrs: &[(&str, &str)], body: Option<&str>) -> (u16, serde_json::Value) {
        let mut req = format!("{method} {path} HTTP/1.1\r\n");
        let mut has_host = false;
        for (k, v) in hdrs {
            has_host |= k.eq_ignore_ascii_case("host");
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if !has_host {
            req.push_str(&format!("Host: {addr}\r\n"));
        }
        let body = body.unwrap_or("");
        req.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        let status: u16 = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let body = text.split_once("\r\n\r\n").map(|x| x.1).unwrap_or("");
        (status, serde_json::from_str(body).unwrap_or(serde_json::Value::Null))
    }

    const JSON: (&str, &str) = ("Content-Type", "application/json");
    const UI: (&str, &str) = ("X-BLVM-UI", "1");

    #[tokio::test]
    async fn new_routes_require_json_and_the_custom_header() {
        let s = server("hdr", true).await;
        let b = Some(r#"{"mode":"managed"}"#);
        // No content type -> 415, even with the header.
        let (c, v) = call(s.addr, "POST", "/api/setup", &[UI], b).await;
        assert_eq!(c, 415, "{v}");
        assert_eq!(v["ok"], false);
        // Form content type -> 415.
        let (c, _) = call(s.addr, "POST", "/api/setup", &[("Content-Type", "text/plain"), UI], b).await;
        assert_eq!(c, 415);
        // JSON but no custom header -> 403.
        let (c, _) = call(s.addr, "POST", "/api/setup", &[JSON], b).await;
        assert_eq!(c, 403);
        // Both -> reaches the handler, which rejects the missing network (400, not a security error).
        let (c, v) = call(s.addr, "POST", "/api/setup", &[JSON, UI], b).await;
        assert_eq!(c, 400);
        assert_eq!(v["error"], "Pick a network first.");
        // The brief's header name works too.
        let (c, v) = call(s.addr, "POST", "/api/setup", &[JSON, ("X-Commons-UI", "1")], b).await;
        assert_eq!((c, v["error"].as_str()), (400, Some("Pick a network first.")));
        // Nothing was written.
        assert!(!s.dir.join("state/console.json").exists());
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn setup_over_http_rejects_bad_bodies() {
        let s = server("bad", true).await;
        let (c, v) = call(s.addr, "POST", "/api/setup", &[JSON, UI], Some("not json")).await;
        assert_eq!(c, 400, "{v}");
        let big = format!(r#"{{"mode":"managed","pad":"{}"}}"#, "x".repeat(70_000));
        let (c, _) = call(s.addr, "POST", "/api/setup", &[JSON, UI], Some(&big)).await;
        assert_eq!(c, 413);
        let (c, v) = call(s.addr, "POST", "/api/setup", &[JSON, UI], Some(r#"{"mode":"managed","network":"narnia"}"#)).await;
        assert_eq!(c, 400);
        assert!(v["error"].as_str().unwrap().contains("Unknown network"));
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn cross_site_and_rebinding_requests_are_refused() {
        let s = server("xsite", false).await;
        let good = [JSON, UI];
        let b = Some(r#"{"method":"stop"}"#);
        // Baseline: same-origin passes the guard (the handler then says method not allowed).
        let (c, v) = call(s.addr, "POST", "/api/rpc", &good, b).await;
        assert_eq!((c, v["error"].as_str()), (400, Some("method not allowed")));
        // Legacy routes, cross-site Origin.
        for path in ["/api/rpc", "/api/connect", "/api/node"] {
            let (c, _) = call(s.addr, "POST", path, &[JSON, UI, ("Origin", "https://evil.example")], b).await;
            assert_eq!(c, 403, "{path}");
            let (c, _) = call(s.addr, "POST", path, &[JSON, UI, ("Host", "evil.example")], b).await;
            assert_eq!(c, 403, "{path} rebinding");
            let (c, _) = call(s.addr, "POST", path, &[("Content-Type", "text/plain")], b).await;
            assert_eq!(c, 415, "{path} simple form post");
        }
        // The custom header is not required on legacy routes yet (documented switch).
        let (c, _) = call(s.addr, "POST", "/api/rpc", &[JSON], b).await;
        assert_eq!(c, 400, "reaches the handler");
        // Every API read needs an allowed Host, the status included (it carries peer addresses and, in
        // managed mode, the data folder); the page itself always arrives under one.
        let (c, v) = call(s.addr, "GET", "/api/status", &[("Host", "192.168.1.5:3849")], None).await;
        assert_eq!(c, 403, "{v}");
        let (c, v) = call(s.addr, "GET", "/api/status", &[("Host", "evil.example")], None).await;
        assert_eq!(c, 403, "{v}");
        let (c, v) = call(s.addr, "GET", "/api/status", &[], None).await;
        assert_eq!(c, 200);
        assert_eq!(v["mode"], "standalone");
        let (c, v) = call(s.addr, "GET", "/api/status", &[("Host", "localhost:3849")], None).await;
        assert_eq!((c, v["mode"].as_str()), (200, Some("standalone")));
        let (c, _) = call(s.addr, "GET", "/api/settings", &[("Host", "evil.example")], None).await;
        assert_eq!(c, 403);
        let (c, _) = call(s.addr, "GET", "/api/settings", &[], None).await;
        assert_eq!(c, 200);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn managed_node_routes_settings_and_logs_work_end_to_end() {
        let s = server("e2e", true).await;
        let (c, v) = call(s.addr, "GET", "/api/status", &[], None).await;
        assert_eq!((c, v["mode"].as_str(), v["managed"]["process"]["state"].as_str()), (200, Some("managed"), Some("stopped")));

        // Starting without a network is an error, not a default.
        let (c, v) = call(s.addr, "POST", "/api/node/start", &[JSON, UI], Some("{}")).await;
        assert_eq!(c, 200);
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "Choose a network first.");

        // Setup with a network, start now.
        let (c, v) = call(s.addr, "POST", "/api/setup", &[JSON, UI], Some(r#"{"mode":"managed","network":"regtest","start_now":true}"#)).await;
        assert_eq!((c, v["ok"].as_bool()), (200, Some(true)), "{v}");
        assert_eq!(v["status"]["managed"]["process"]["state"], "starting");
        assert!(v["status"]["managed"]["process"]["pid"].as_u64().is_some());

        // Logs show the node's output and the console's own start line.
        let mut found = false;
        for _ in 0..100 {
            let (c, v) = call(s.addr, "GET", "/api/node/logs?limit=50", &[], None).await;
            assert_eq!(c, 200);
            assert!(v["file"].as_str().unwrap().ends_with("networks/regtest/logs/node.log"));
            if v["lines"].as_array().unwrap().iter().any(|l| l["text"] == "up") {
                found = true;
                let last = v["lines"].as_array().unwrap().last().unwrap()["seq"].as_u64().unwrap();
                let (_, after) = call(s.addr, "GET", &format!("/api/node/logs?after={last}"), &[], None).await;
                assert_eq!(after["lines"].as_array().unwrap().len(), 0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(found, "node output did not reach /api/node/logs");

        // Settings: read, patch with PUT, persisted.
        let (c, v) = call(s.addr, "GET", "/api/settings", &[], None).await;
        assert_eq!((c, v["network"].as_str(), v["mode"].as_str()), (200, Some("regtest"), Some("managed")));
        let (c, v) = call(s.addr, "PUT", "/api/settings", &[JSON, UI], Some(r#"{"stop_node_on_exit":false}"#)).await;
        assert_eq!((c, v["stop_node_on_exit"].as_bool()), (200, Some(false)), "{v}");
        let (c, _) = call(s.addr, "PUT", "/api/settings", &[JSON, UI], Some(r#"{"network":"signet"}"#)).await;
        assert_eq!(c, 409, "network change needs the node stopped");
        let (c, _) = call(s.addr, "POST", "/api/connect", &[JSON, UI], Some(r#"{"rpc":"127.0.0.1:1"}"#)).await;
        assert_eq!(c, 409, "a managed console does not follow arbitrary targets");

        // Stop returns at once with the stopping state, then settles.
        let (c, v) = call(s.addr, "POST", "/api/node/stop", &[JSON, UI], Some("{}")).await;
        assert_eq!((c, v["ok"].as_bool()), (200, Some(true)));
        assert_eq!(v["status"]["managed"]["process"]["state"], "stopping");
        let agent = s.state.read().await.console.agent.clone().unwrap();
        agent.stop_and_wait().await;
        assert!(!agent.state().is_active());

        // Restart is a managed-only action; the toggle alias still works.
        let (c, v) = call(s.addr, "POST", "/api/node", &[JSON, UI], Some(r#"{"action":"toggle"}"#)).await;
        assert_eq!((c, v["ok"].as_bool()), (200, Some(true)), "{v}");
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn network_preview_and_switch_routes_work_end_to_end() {
        let s = server("switch", true).await;
        // Before a network is chosen nothing is allowed, and nothing is defaulted.
        let (c, v) = call(s.addr, "GET", "/api/network/preview?to=signet", &[], None).await;
        assert_eq!((c, v["allowed"].as_bool(), v["from"].clone()), (200, Some(false), serde_json::Value::Null), "{v}");
        let (c, v) = call(s.addr, "POST", "/api/network/switch", &[JSON, UI], Some(r#"{"to":"signet"}"#)).await;
        assert_eq!(c, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("No network"));

        let (c, v) = call(s.addr, "POST", "/api/setup", &[JSON, UI], Some(r#"{"mode":"managed","network":"regtest","start":true}"#)).await;
        assert_eq!((c, v["ok"].as_bool()), (200, Some(true)), "{v}");

        // Preview: GET with ?to= (what the page uses) and POST with a body (scripts) agree.
        let (c, g) = call(s.addr, "GET", "/api/network/preview?to=signet", &[], None).await;
        assert_eq!((c, g["allowed"].as_bool(), g["from"].as_str(), g["to"].as_str()), (200, Some(true), Some("regtest"), Some("signet")), "{g}");
        for key in ["from_bytes", "to_bytes", "to_height", "from_ibd_pct", "free_bytes", "estimate_bytes", "blocked", "warnings", "reason"] {
            assert!(g.get(key).is_some(), "{key} missing: {g}");
        }
        let (c, p) = call(s.addr, "POST", "/api/network/preview", &[JSON, UI], Some(r#"{"network":"signet"}"#)).await;
        assert_eq!(c, 200);
        assert_eq!(p["allowed"], g["allowed"]);
        assert_eq!(p["to"], g["to"]);
        let (_, same) = call(s.addr, "GET", "/api/network/preview?to=regtest", &[], None).await;
        assert_eq!(same["allowed"], false);
        assert!(same["reason"].as_str().unwrap().contains("Already on Regtest"));
        let (c, _) = call(s.addr, "GET", "/api/network/preview?to=narnia", &[], None).await;
        assert_eq!(c, 400);
        // Reads are kept off rebinding pages; the switch needs JSON and the header like every mutation.
        let (c, _) = call(s.addr, "GET", "/api/network/preview?to=signet", &[("Host", "evil.example")], None).await;
        assert_eq!(c, 403);
        let (c, _) = call(s.addr, "POST", "/api/network/switch", &[JSON], Some(r#"{"to":"signet"}"#)).await;
        assert_eq!(c, 403);
        let (c, _) = call(s.addr, "POST", "/api/network/switch", &[UI], Some(r#"{"to":"signet"}"#)).await;
        assert_eq!(c, 415);
        let (c, v) = call(s.addr, "POST", "/api/network/switch", &[JSON, UI], Some(r#"{"to":"regtest"}"#)).await;
        assert_eq!(c, 400, "{v}");
        let (c, v) = call(s.addr, "POST", "/api/network/switch", &[JSON, UI], Some(r#"{"to":"narnia"}"#)).await;
        assert_eq!(c, 400, "{v}");

        // The real switch: 202 with an id, status carries the phase, a second one is 409.
        let (c, v) = call(s.addr, "POST", "/api/network/switch", &[JSON, UI], Some(r#"{"to":"signet"}"#)).await;
        assert_eq!(c, 202, "{v}");
        let id = v["id"].as_str().unwrap().to_string();
        assert_eq!(v["status"]["managed"]["switch"]["id"], id.as_str());
        let (c, v2) = call(s.addr, "POST", "/api/network/switch", &[JSON, UI], Some(r#"{"to":"testnet4"}"#)).await;
        assert_eq!((c, v2["error"].as_str()), (409, Some("busy")), "{v2}");
        let (c, st) = call(s.addr, "GET", "/api/status", &[], None).await;
        assert_eq!(c, 200);
        let sw = &st["managed"]["switch"];
        assert_eq!(sw["id"], id.as_str());
        assert_eq!((sw["from"].as_str(), sw["to"].as_str()), (Some("regtest"), Some("signet")));
        assert!(matches!(sw["phase"].as_str(), Some("stopping" | "starting")), "{sw}");
        assert!(sw["started"].as_u64().is_some());
        // The node is the one thing to clean up; the switch task dies with the test runtime.
        let agent = s.state.read().await.console.agent.clone().unwrap();
        loop {
            // Wait for the signet node to be spawned, then stop it.
            if agent.view().network == Some("signet") && agent.view().pid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let (_, st) = call(s.addr, "GET", "/api/status", &[], None).await;
        assert_eq!(st["managed"]["network"], "signet");
        assert_eq!(st["rpc_addr"], "127.0.0.1:38332");
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn standalone_has_no_network_switch() {
        let s = server("sa-switch", false).await;
        let (c, v) = call(s.addr, "GET", "/api/network/preview?to=signet", &[], None).await;
        assert_eq!(c, 404, "{v}");
        let (c, v) = call(s.addr, "POST", "/api/network/switch", &[JSON, UI], Some(r#"{"to":"signet"}"#)).await;
        assert_eq!(c, 404, "{v}");
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn standalone_keeps_the_old_behaviour() {
        let s = server("sa", false).await;
        let (c, v) = call(s.addr, "GET", "/api/node/logs", &[], None).await;
        assert_eq!(c, 404, "{v}");
        let (c, v) = call(s.addr, "POST", "/api/node/restart", &[JSON, UI], Some("{}")).await;
        assert_eq!(c, 200);
        assert_eq!(v["ok"], false, "restart needs a managed node: {v}");
        let (c, v) = call(s.addr, "GET", "/api/status", &[], None).await;
        assert_eq!(c, 200);
        assert_eq!(v["managed"], serde_json::Value::Null);
        assert_eq!(v["blvm_version"], "—");
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    // ---- review (U0/U1) tests ----

    #[tokio::test]
    async fn review_setup_without_network_over_http_writes_and_starts_nothing() {
        let s = server("rv-nonet", true).await;
        for b in [r#"{"mode":"managed","start":true}"#, r#"{"mode":"managed","network":null,"start":true}"#, r#"{"mode":"managed","network":""}"#] {
            let (c, v) = call(s.addr, "POST", "/api/setup", &[JSON, UI], Some(b)).await;
            assert_eq!(c, 400, "{b} -> {v}");
        }
        let (_, v) = call(s.addr, "GET", "/api/status", &[], None).await;
        assert_eq!(v["managed"]["process"]["state"], "stopped");
        assert_eq!(v["managed"]["process"]["pid"], serde_json::Value::Null);
        assert_eq!(v["managed"]["network"], serde_json::Value::Null);
        assert_eq!(v["setup_done"], false);
        assert!(!s.dir.join("state/console.json").exists());
        assert!(!s.dir.join("root").exists());
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn review_body_cap_applies_to_legacy_routes_and_never_turns_into_a_toggle() {
        let s = server("rv-cap", true).await;
        let pad = "x".repeat(70_000);
        let (c, v) = call(s.addr, "POST", "/api/node", &[JSON, UI], Some(&format!(r#"{{"action":"stop","pad":"{pad}"}}"#))).await;
        let node_code = (c, v["error"].clone());
        let (c, _) = call(s.addr, "POST", "/api/rpc", &[JSON, UI], Some(&format!(r#"{{"method":"x","pad":"{pad}"}}"#))).await;
        let rpc_code = c;
        let _ = std::fs::remove_dir_all(&s.dir);
        assert_eq!(rpc_code, 413, "/api/rpc reads an unlimited body");
        assert_eq!(node_code.0, 413, "/api/node with an oversize body was handled as action=toggle: {:?}", node_code.1);
    }

    #[tokio::test]
    async fn review_unknown_api_get_is_a_json_404_not_the_page() {
        let s = server("rv-404", true).await;
        // U2 added /api/network/preview; /api/modules is still a route this host does not have (U5).
        let mut req = format!("GET /api/modules HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", s.addr);
        let mut st = tokio::net::TcpStream::connect(s.addr).await.unwrap();
        st.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        st.read_to_end(&mut out).await.unwrap();
        req.clear();
        let text = String::from_utf8_lossy(&out).to_string();
        let _ = std::fs::remove_dir_all(&s.dir);
        let status = text.split_whitespace().nth(1).unwrap_or("").to_string();
        assert_eq!(status, "404", "unknown /api GET answered {status} with: {}", &text[..text.len().min(200)]);
    }

    // ---- fix pass for the U0/U1 review (S7, S8, S11, nits) ----

    /// Raw GET returning (status line code, content-type header, body text).
    async fn raw_get(addr: SocketAddr, path: &str) -> (u16, String, String) {
        let req = format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
        let mut st = tokio::net::TcpStream::connect(addr).await.unwrap();
        st.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        st.read_to_end(&mut out).await.unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        let code = text.split_whitespace().nth(1).unwrap().parse().unwrap();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let ct = head
            .lines()
            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-type:").map(|v| v.trim().to_string()))
            .unwrap_or_default();
        (code, ct, body.to_string())
    }

    #[tokio::test]
    async fn s7_unknown_api_paths_are_json_404s_for_every_method_and_mode() {
        for managed in [false, true] {
            let s = server(if managed { "s7-m" } else { "s7-s" }, managed).await;
            for path in ["/api/modules", "/api/node/nope", "/api/modules/install", "/api/nope", "/api/network/nope", "/api/"] {
                let (c, ct, body) = raw_get(s.addr, path).await;
                assert_eq!(c, 404, "GET {path} managed={managed}: {body}");
                assert!(ct.starts_with("application/json"), "GET {path}: content-type {ct}");
                assert!(!body.contains("<html") && !body.contains("<!DOCTYPE"), "GET {path} answered the page");
                let v: serde_json::Value = serde_json::from_str(&body).unwrap();
                assert_eq!(v["ok"], false);
                assert!(v["error"].as_str().is_some_and(|e| !e.is_empty()));
            }
            // POST to a missing route: JSON 404 too (after the mutation guard, so with the right headers).
            for path in ["/api/node/nope", "/api/modules/install", "/api/nope"] {
                let (c, v) = call(s.addr, "POST", path, &[JSON, UI], Some("{}")).await;
                assert_eq!(c, 404, "POST {path}: {v}");
                assert_eq!(v["ok"], false);
            }
            // The page and its assets are still served, and an unknown *page* path falls back to the app
            // (when the web build is there to serve).
            if dist_dir().join("index.html").is_file() {
                let (c, ct, _) = raw_get(s.addr, "/").await;
                assert_eq!(c, 200);
                assert!(ct.starts_with("text/html"), "{ct}");
                let (c, _, body) = raw_get(s.addr, "/some/deep/link").await;
                assert_eq!(c, 200, "client-side routes still get the app");
                assert!(body.contains("<html"));
            }
            let _ = std::fs::remove_dir_all(&s.dir);
        }
    }

    #[tokio::test]
    async fn s8_legacy_routes_cap_and_parse_their_bodies() {
        let s = server("s8", false).await;
        let pad = "x".repeat(70_000);
        // /api/connect: oversize 413, not JSON 400, and the target did not move.
        let before = s.state.read().await.rpc_addr.clone();
        let (c, _) = call(s.addr, "POST", "/api/connect", &[JSON, UI], Some(&format!(r#"{{"rpc":"127.0.0.1:1","pad":"{pad}"}}"#))).await;
        assert_eq!(c, 413);
        let (c, v) = call(s.addr, "POST", "/api/connect", &[JSON, UI], Some("not json")).await;
        assert_eq!(c, 400, "{v}");
        let (c, _) = call(s.addr, "POST", "/api/connect", &[JSON, UI], Some("")).await;
        assert_eq!(c, 400);
        assert_eq!(s.state.read().await.rpc_addr, before, "a bad body changes nothing");
        let (c, v) = call(s.addr, "POST", "/api/connect", &[JSON, UI], Some(r#"{"rpc":"127.0.0.1:38332"}"#)).await;
        assert_eq!((c, v["rpc_addr"].as_str()), (200, Some("127.0.0.1:38332")), "a good body still works");
        // /api/rpc.
        let (c, _) = call(s.addr, "POST", "/api/rpc", &[JSON, UI], Some("{oops")).await;
        assert_eq!(c, 400);
        let (c, _) = call(s.addr, "POST", "/api/rpc", &[JSON, UI], Some(&format!(r#"{{"method":"x","pad":"{pad}"}}"#))).await;
        assert_eq!(c, 413);
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn s8_an_unreadable_node_body_is_a_400_and_never_a_toggle() {
        let s = server("s8-node", true).await;
        // Managed, a network chosen, nothing running: a toggle would start the node.
        let (c, _) = call(s.addr, "PUT", "/api/settings", &[JSON, UI], Some(r#"{"network":"regtest"}"#)).await;
        assert_eq!(c, 200);
        let agent = s.state.read().await.console.agent.clone().unwrap();
        let pad = "x".repeat(70_000);
        for (label, body, want) in [
            ("oversize", format!(r#"{{"action":"start","pad":"{pad}"}}"#), 413),
            ("not json", "start".to_string(), 400),
            ("empty", String::new(), 400),
            ("truncated", r#"{"action":"sta"#.to_string(), 400),
            ("action not text", r#"{"action":7}"#.to_string(), 400),
            ("action null", r#"{"action":null}"#.to_string(), 400),
        ] {
            let (c, v) = call(s.addr, "POST", "/api/node", &[JSON, UI], Some(&body)).await;
            assert_eq!(c, want, "{label}: {v}");
            assert_eq!(agent.state(), ProcState::Stopped, "{label} must not start the node");
            assert_eq!(agent.view().pid, None, "{label}");
        }
        // A well-formed body still acts; `{}` keeps its old meaning (toggle).
        let (c, v) = call(s.addr, "POST", "/api/node", &[JSON, UI], Some(r#"{"action":"start"}"#)).await;
        assert_eq!((c, v["ok"].as_bool()), (200, Some(true)), "{v}");
        agent.stop_and_wait().await;
        let (c, v) = call(s.addr, "POST", "/api/node", &[JSON, UI], Some("{}")).await;
        assert_eq!((c, v["ok"].as_bool()), (200, Some(true)), "{v}");
        agent.stop_and_wait().await;
        // The /start route does not read its body at all, but still refuses an oversize one.
        let (c, _) = call(s.addr, "POST", "/api/node/start", &[JSON, UI], Some(&format!(r#"{{"pad":"{pad}"}}"#))).await;
        assert_eq!(c, 413);
        assert_eq!(agent.state(), ProcState::Stopped);
        let (c, v) = call(s.addr, "POST", "/api/node/start", &[JSON, UI], Some("garbage")).await;
        assert_eq!((c, v["ok"].as_bool()), (200, Some(true)), "forced actions ignore the body: {v}");
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[tokio::test]
    async fn s11_the_403_names_the_variable_and_the_fix() {
        let s = server("s11", false).await;
        for (method, path, hdrs) in [
            ("GET", "/api/status", vec![("Host", "192.168.1.5:3849")]),
            ("POST", "/api/node", vec![JSON, UI, ("Host", "umbrel.local")]),
        ] {
            let (c, v) = call(s.addr, method, path, &hdrs, Some("{}")).await;
            assert_eq!(c, 403, "{method} {path}");
            let e = v["error"].as_str().unwrap();
            assert!(e.contains("BLVM_UI_ALLOWED_HOSTS"), "{e}");
            assert!(e.contains("127.0.0.1"), "also says what works without it: {e}");
        }
        let (c, v) = call(s.addr, "POST", "/api/node", &[JSON, UI, ("Origin", "http://192.168.1.5:3849")], Some("{}")).await;
        assert_eq!(c, 403);
        assert!(v["error"].as_str().unwrap().contains("BLVM_UI_ALLOWED_HOSTS"), "{v}");
        let _ = std::fs::remove_dir_all(&s.dir);
    }

    #[test]
    fn s11_a_startup_warning_when_the_console_is_reachable_but_no_host_is_allowed() {
        let lan: SocketAddr = "0.0.0.0:3849".parse().unwrap();
        let w = lan_listen_warning(lan, false).expect("warns");
        assert!(w.contains("BLVM_UI_ALLOWED_HOSTS") && w.contains("0.0.0.0:3849"), "{w}");
        assert!(lan_listen_warning("192.168.1.5:3849".parse().unwrap(), false).is_some());
        assert!(lan_listen_warning("[::]:3849".parse().unwrap(), false).is_some());
        assert!(lan_listen_warning(lan, true).is_none(), "the variable is set");
        assert!(lan_listen_warning("127.0.0.1:3849".parse().unwrap(), false).is_none());
        assert!(lan_listen_warning("[::1]:3849".parse().unwrap(), false).is_none());
    }

}
