use anyhow::Result;
use blvm_ui_next::managed;
use blvm_ui_next::persist;
use blvm_ui_next::signals::shutdown_signal;
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

    // `blvm-ui-next agent --root /data`: no web server, just the node agent (the node container of the
    // Umbrel app). Everything else below is the console.
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("agent") => return run_agent(&args[1..]).await,
        Some("--version" | "-V" | "version") => {
            println!("blvm-ui-next {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        _ => {}
    }

    let listen: SocketAddr = std::env::var("BLVM_UI_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:3849".into())
        .parse()?;
    if let Some(w) = blvm_ui_next::http::lan_listen_warning(listen, blvm_ui_next::http::allowed_hosts_configured()) {
        tracing::warn!("{w}");
    }
    // Mode, settings and (managed) the node agent. In standalone mode with no `console.json` this is
    // the same as before: the target is BLVM_UI_RPC, else 127.0.0.1:48332.
    let console = managed::Console::from_process();
    tracing::info!(
        mode = console.eff.mode.as_str(),
        source = console.eff.mode_source,
        setup_done = console.eff.setup_done,
        "console mode"
    );
    let mut live = LiveState::new(console.initial_rpc());
    live.console = console;
    live.restore(persist::load());
    // The first /api/status already says which mode this is, before the first poll lands.
    blvm_ui_next::state::stamp_power(&mut live);
    let state: Shared = Arc::new(RwLock::new(live));
    {
        // Adopt a node that outlived the last console, or start one if asked. Does not block serving.
        let state = Arc::clone(&state);
        tokio::spawn(async move { managed::boot(&state).await });
    }
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
            // Managed node: stop it first (default) or leave it running. A second Ctrl-C quits
            // without waiting; the node keeps draining on its own.
            tokio::select! {
                _ = managed::shutdown(&state) => {}
                _ = shutdown_signal() => {
                    tracing::warn!("second signal: quitting without waiting for the node");
                    if let Some(a) = state.read().await.console.agent.clone() {
                        a.detach();
                    }
                }
            }
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

/// `agent [--root <dir>]`: run the node agent until SIGTERM, then stop the node and exit 0.
async fn run_agent(args: &[String]) -> Result<()> {
    let mut root: Option<std::path::PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" | "-r" => {
                root = Some(it.next().ok_or_else(|| anyhow::anyhow!("--root needs a folder"))?.into());
            }
            _ if a.starts_with("--root=") => root = Some(a["--root=".len()..].into()),
            "-h" | "--help" => {
                println!(
                    "blvm-ui-next agent [--root DIR]\n\nRuns the node agent: watches DIR/control/desired.json, runs the BLVM node as asked and\nreports in DIR/control/state.json. DIR defaults to $BLVM_UI_DATA_ROOT, else /data.\nEnvironment: BLVM_AGENT_RPC_BIND (node RPC bind address, default 127.0.0.1), BLVM_AGENT_BUNDLED_BIN (program to\nuse when DIR/bin/blvm does not exist), BLVM_RPC_USER and BLVM_RPC_PASS (the node's RPC login: written to the\nnode's config file before every start; a node that listens beyond loopback does not start without them)."
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}; try `agent --root /data`"),
        }
    }
    let root = root
        .or_else(|| std::env::var_os("BLVM_UI_DATA_ROOT").filter(|v| !v.is_empty()).map(Into::into))
        .unwrap_or_else(|| "/data".into());
    if let Err(msg) = blvm_ui_next::agent_daemon::run_forever(blvm_ui_next::agent_daemon::DaemonOpts::from_env(root)).await {
        tracing::error!("{msg}");
        std::process::exit(1);
    }
    Ok(())
}
