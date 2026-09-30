use crate::node_ctl::{self, PowerPhase};
use crate::state::{poll_once, stamp_power, Shared};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
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

pub async fn serve(addr: SocketAddr, state: Shared) -> anyhow::Result<()> {
    let dist = dist_dir();
    tracing::info!(
        "blvm-ui-next listening on http://{addr} (web dir {})",
        dist.display()
    );
    let listener = TcpListener::bind(addr).await?;
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
    let resp = match (method, path.as_str()) {
        (Method::GET, "/api/status") => {
            let snap = state.read().await.snapshot.clone();
            json_ok(&snap)
        }
        (Method::POST, "/api/node") => power_node(req, state).await,
        (Method::POST, "/api/connect") => {
            let collected = req.into_body().collect().await.ok().map(|c| c.to_bytes());
            let addr = collected
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .and_then(|v| v.get("rpc").and_then(|s| s.as_str()).map(|s| s.to_string()))
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
        (Method::POST, "/api/rpc") => {
            let collected = req.into_body().collect().await.ok().map(|c| c.to_bytes());
            let body = collected
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .unwrap_or(serde_json::json!({}));
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
        (Method::GET, _) => serve_web(dist, &path),
        _ => {
            let mut r = Response::new(Full::new(Bytes::from("not found")));
            *r.status_mut() = StatusCode::NOT_FOUND;
            r
        }
    };
    Ok(resp)
}

async fn power_node(req: Request<Incoming>, state: Shared) -> Response<Full<Bytes>> {
    let lock = { state.read().await.power_lock.clone() };
    let _busy = lock.lock().await;
    let collected = req.into_body().collect().await.ok().map(|c| c.to_bytes());
    let action = collected
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| {
            v.get("action")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        })
        .unwrap_or_else(|| "toggle".into());
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
