use anyhow::Result;
use blvm_ui_next::persist;
use blvm_ui_next::state::{poll_tick, LiveState, Shared};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{interval, Duration};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "blvm_ui_next=info".into()),
        )
        .init();

    let listen: SocketAddr = std::env::var("BLVM_UI_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3849".into())
        .parse()?;
    let rpc = std::env::var("BLVM_UI_RPC").unwrap_or_else(|_| "127.0.0.1:48332".into());

    let mut live = LiveState::new(rpc);
    live.restore(persist::load());
    let state: Shared = Arc::new(RwLock::new(live));
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                poll_tick(&state).await;
            }
        });
    }

    // Save Latest Blocks history a few seconds after it changes.
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let mut tick = interval(Duration::from_secs(3));
            loop {
                tick.tick().await;
                let dirty = state.write().await.take_dirty_feeds();
                if let Some(views) = dirty {
                    save_blocking(views).await;
                }
            }
        });
    }

    tokio::select! {
        r = blvm_ui_next::http::serve(listen, Arc::clone(&state)) => r?,
        _ = shutdown_signal() => {
            let views = state.write().await.all_feeds();
            save_blocking(views).await;
            tracing::info!("blvm-ui-next stopped; block history saved");
        }
    }
    Ok(())
}

async fn save_blocking(views: std::collections::BTreeMap<String, persist::StoredView>) {
    let res = tokio::task::spawn_blocking(move || persist::save(&views)).await;
    if let Ok(Err(e)) = res {
        tracing::warn!("block history: save failed: {e}");
    }
}

/// Ctrl-C everywhere; SIGTERM too on Unix (Docker, systemd, `kill`).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
