//! `blvm-ui-next agent --root /data`: the node agent as its own process, for the node container of the Umbrel
//! app (managed-node design B, H). It holds no web server. It watches `<root>/control/desired.json`, runs the
//! node the way the in-process agent does (same [`Agent`]: per-network folder, file logs, pid file, crash
//! backoff, SIGTERM then SIGKILL), and reports in `<root>/control/state.json`.
//!
//! * Level-triggered by generation: when `desired.generation` is one it has not dealt with, it acts once
//!   (start, stop, or stop-then-start when the network changed or `restart_id` went up) and reports the
//!   generation as applied once the node is in the wanted shape. Between generations it only supervises
//!   (crash restarts are the [`Agent`]'s job).
//! * A restart of this process (container restart, `kill -9`) finds the node still running (it leads its
//!   own process group and its logs go to a file) and adopts it through `control/node.pid`; if the node is
//!   gone and `desired.running` is true, it starts it again by itself.
//! * SIGTERM: stop the node (`stop_grace_secs` from desired.json, then SIGKILL), write the final state, exit 0.
//!   `desired.json` is left as it was, so the next container start brings the node back.
//! * State is written on every change and at least every two seconds.

use crate::agent::{Agent, AgentConfig, ProcState, RpcAuth, Timings};
use crate::control::{self, Desired, StateFile};
use crate::networks;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
pub struct DaemonOpts {
    pub root: PathBuf,
    pub timings: Timings,
    /// Address the node's RPC server binds to: `0.0.0.0` in the node container (the console reaches it from
    /// another container), loopback elsewhere.
    pub rpc_bind: String,
    /// The program to run when `desired.binary` (relative) is not in the data folder, for example the one
    /// baked into the node image.
    pub bundled_bin: Option<PathBuf>,
    /// The RPC login the node is started with (`BLVM_RPC_USER` / `BLVM_RPC_PASS`). The agent writes it into the
    /// node's config file before every start; without it a node that listens beyond loopback is not started.
    pub rpc_auth: Option<RpcAuth>,
    /// How often `desired.json` is looked at.
    pub poll: Duration,
    /// `state.json` is rewritten at least this often.
    pub heartbeat: Duration,
    /// Decide readiness without asking the RPC port (tests).
    pub ready: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    /// Peer-to-peer port instead of the network's own, on loopback only (tests: see [`AgentConfig::p2p_port`]).
    pub p2p_port: Option<u16>,
}

impl DaemonOpts {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            timings: Timings::default(),
            rpc_bind: "127.0.0.1".into(),
            bundled_bin: None,
            rpc_auth: None,
            poll: Duration::from_millis(250),
            heartbeat: Duration::from_secs(2),
            ready: None,
            p2p_port: None,
        }
    }

    /// `BLVM_AGENT_RPC_BIND`, `BLVM_AGENT_BUNDLED_BIN`, `BLVM_RPC_USER` and `BLVM_RPC_PASS`.
    pub fn from_env(root: PathBuf) -> Self {
        let mut o = Self::new(root);
        if let Some(b) = std::env::var("BLVM_AGENT_RPC_BIND").ok().filter(|b| !b.trim().is_empty()) {
            o.rpc_bind = b.trim().to_string();
        }
        o.bundled_bin = std::env::var("BLVM_AGENT_BUNDLED_BIN")
            .ok()
            .filter(|b| !b.trim().is_empty())
            .map(PathBuf::from);
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        o.rpc_auth = match (get("BLVM_RPC_USER"), get("BLVM_RPC_PASS")) {
            (Some(user), Some(pass)) => Some(RpcAuth { user, pass }),
            _ => None,
        };
        o
    }
}

pub struct Daemon {
    opts: DaemonOpts,
    agent: Agent,
    /// The generation of `desired.json` the node is in the wanted shape for.
    applied: u64,
    /// A generation being worked on.
    pending: Option<Desired>,
    /// `restart_id` the running node was started under.
    running_restart_id: u64,
    boot: bool,
    /// `desired.binary` as last read (what `binary_found` in `state.json` is about).
    last_binary: String,
    last_written: Option<(String, Instant)>,
}

impl Daemon {
    pub fn new(opts: DaemonOpts) -> Self {
        let cfg = AgentConfig {
            bin: None,
            root: opts.root.clone(),
            network: None,
            rpc_addr: None,
            assume_valid: None,
            child_env: Default::default(),
            rpc_auth: opts.rpc_auth.clone(),
            p2p_port: opts.p2p_port,
        };
        let agent = match &opts.ready {
            Some(r) => Agent::with_ready(cfg, opts.timings.clone(), Arc::clone(r)),
            None => Agent::new(cfg, opts.timings.clone()),
        };
        Self { opts, agent, applied: 0, pending: None, running_restart_id: 0, boot: true, last_binary: control::DEFAULT_BINARY.into(), last_written: None }
    }

    pub fn agent(&self) -> Agent {
        self.agent.clone()
    }

    /// The program for `d`: absolute as given; relative to the data root; else the bundled one.
    fn resolve_bin(&self, d: &Desired) -> PathBuf {
        let given = PathBuf::from(&d.binary);
        if given.is_absolute() {
            return given;
        }
        let in_root = self.opts.root.join(&given);
        if in_root.is_file() {
            return in_root;
        }
        match &self.opts.bundled_bin {
            Some(b) if b.is_file() => b.clone(),
            _ => in_root,
        }
    }

    fn config_for(&self, d: &Desired) -> AgentConfig {
        let net = d.network.as_deref().and_then(networks::by_id);
        AgentConfig {
            bin: Some(self.resolve_bin(d)),
            root: self.opts.root.clone(),
            network: net,
            rpc_addr: net.map(|n| format!("{}:{}", self.opts.rpc_bind, d.rpc_port.unwrap_or(n.rpc_port))),
            assume_valid: d.assume_valid.clone(),
            child_env: d.env.clone(),
            rpc_auth: self.opts.rpc_auth.clone(),
            p2p_port: self.opts.p2p_port,
        }
    }

    /// One look at `desired.json` and one step towards it.
    async fn tick(&mut self) {
        let Some(d) = control::read_desired(&self.opts.root) else {
            return; // nothing asked yet
        };
        if d.stop_grace_secs > 0 {
            self.agent.set_stop_wait(Duration::from_secs(d.stop_grace_secs));
        }
        self.last_binary = d.binary.clone();
        if self.boot {
            self.boot = false;
            self.agent.set_config(self.config_for(&d));
            if self.agent.adopt() {
                tracing::info!("agent: took over the node that was still running");
                self.running_restart_id = d.restart_id;
                self.applied = if d.running { d.generation } else { 0 };
            }
        }
        if d.generation != self.applied && self.pending.as_ref().map(|p| p.generation) != Some(d.generation) {
            self.pending = Some(d);
        }
        let Some(p) = self.pending.clone() else { return };
        if self.step(&p).await {
            self.applied = p.generation;
            self.pending = None;
        }
    }

    /// Move towards `d`. `true` when the node is in the wanted shape (or nothing more can be done for it).
    async fn step(&mut self, d: &Desired) -> bool {
        let view = self.agent.view();
        if !d.running {
            if view.state.is_active() || view.state == ProcState::Crashed {
                self.agent.request_stop(); // also ends a crash backoff
            }
            return !self.agent.state().is_active();
        }
        let wanted_net = d.network.as_deref().and_then(networks::by_id).map(|n| n.id);
        if view.state.is_active() {
            let other_network = wanted_net.is_some() && view.network != wanted_net && view.network.is_some();
            let restart = d.restart_id != self.running_restart_id;
            if other_network || restart {
                tracing::info!(other_network, restart, "agent: stopping the node to start it again");
                self.agent.request_stop();
                return false;
            }
            return view.state != ProcState::Stopping;
        }
        // Not running: start it.
        self.agent.set_config(self.config_for(d));
        self.running_restart_id = d.restart_id;
        match self.agent.start().await {
            Ok(()) => {
                tracing::info!(network = ?d.network, generation = d.generation, "agent: node started");
                true
            }
            Err(e) => {
                // `Stopping` is not a failure, just not yet.
                if self.agent.state() == ProcState::Stopping {
                    return false;
                }
                tracing::warn!("agent: cannot start the node: {e}");
                true
            }
        }
    }

    fn build_state(&self) -> StateFile {
        let v = self.agent.view();
        let inst = self.agent.installed();
        let cfg = self.agent.config();
        StateFile {
            agent_version: env!("CARGO_PKG_VERSION").into(),
            heartbeat: crate::persist::unix_now(),
            generation: self.applied,
            state: v.state.as_str().into(),
            pid: v.pid,
            since: v.since,
            network: v.network.map(str::to_string).or_else(|| cfg.network.map(|n| n.id.to_string())),
            exit_code: v.exit_code,
            exit_signal: v.exit_signal,
            restarts: v.restarts,
            last_error: v.last_error,
            conflict: v.conflict,
            adopted: v.adopted,
            crash_log: v.crash_log,
            installed_version: inst.as_ref().and_then(|i| i.version.clone()),
            installed_report: inst.as_ref().and_then(|i| i.self_report.clone()),
            // Before anything is asked, the answer is about the default program (and the bundled one).
            binary_found: self.resolve_bin(&Desired { binary: self.last_binary.clone(), ..Default::default() }).is_file(),
            binary: Some(
                self.resolve_bin(&Desired { binary: self.last_binary.clone(), ..Default::default() }).display().to_string(),
            ),
            restart_id: self.running_restart_id,
        }
    }

    /// Write `state.json` when something changed or the heartbeat is due.
    fn write_state(&mut self, force: bool) {
        let mut s = self.build_state();
        // What counts as a change: everything except the clock.
        let key = {
            let mut k = s.clone();
            k.heartbeat = 0;
            serde_json::to_string(&k).unwrap_or_default()
        };
        let due = match &self.last_written {
            Some((last, at)) => *last != key || at.elapsed() >= self.opts.heartbeat,
            None => true,
        };
        if !due && !force {
            return;
        }
        s.heartbeat = crate::persist::unix_now();
        match control::write_state(&self.opts.root, &s) {
            Ok(()) => self.last_written = Some((key, Instant::now())),
            Err(e) => tracing::warn!("agent: cannot write state.json: {e}"),
        }
    }

    /// Run until `shutdown` completes, then stop the node and return.
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) {
        let _ = std::fs::create_dir_all(control::dir(&self.opts.root));
        let pid_path = control::dir(&self.opts.root).join("agent.pid");
        let _ = std::fs::write(&pid_path, std::process::id().to_string());
        tracing::info!(root = %self.opts.root.display(), "agent: running");
        let prober = self.spawn_prober();
        tokio::pin!(shutdown);
        let mut tick = tokio::time::interval(self.opts.poll);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.tick().await;
                    self.write_state(false);
                }
                _ = &mut shutdown => break,
            }
        }
        prober.abort();
        tracing::info!("agent: shutting down");
        // Stop the node, and keep the heartbeat going while the (up to 200 s) stop runs.
        if let Some(d) = control::read_desired(&self.opts.root) {
            if d.stop_grace_secs > 0 {
                self.agent.set_stop_wait(Duration::from_secs(d.stop_grace_secs));
            }
        }
        {
            let agent = self.agent.clone();
            let stop = agent.shutdown(true);
            tokio::pin!(stop);
            let mut beat = tokio::time::interval(Duration::from_millis(500));
            loop {
                tokio::select! {
                    _ = &mut stop => break,
                    _ = beat.tick() => self.write_state(false),
                }
            }
        }
        self.write_state(true);
        let _ = std::fs::remove_file(pid_path);
    }

    /// While the node is `starting`, ask its RPC port whether anything answers. Any HTTP reply counts, a login
    /// error included: it means the server is up.
    fn spawn_prober(&self) -> tokio::task::JoinHandle<()> {
        let agent = self.agent.clone();
        let custom = self.opts.ready.is_some();
        tokio::spawn(async move {
            if custom {
                return;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                if agent.state() != ProcState::Starting {
                    continue;
                }
                let cfg = agent.config();
                let Some(addr) = cfg.rpc() else { continue };
                if rpc_answers_as(&crate::agent::probe_addr(&addr), cfg.rpc_auth.as_ref()).await {
                    agent.rpc_flag().store(true, Ordering::Relaxed);
                }
            }
        })
    }
}

/// Connect and send one JSON-RPC request; `true` when an HTTP response comes back.
pub async fn rpc_answers(addr: &str) -> bool {
    rpc_answers_as(addr, None).await
}

/// [`rpc_answers`] with the node's login, so that asking is not counted as a failed login attempt.
pub async fn rpc_answers_as(addr: &str, auth: Option<&RpcAuth>) -> bool {
    let body = r#"{"jsonrpc":"2.0","id":0,"method":"getblockcount","params":[]}"#;
    let auth = auth.map(|a| format!("Authorization: {}\r\n", crate::rpc::basic_auth_header(&a.user, &a.pass))).unwrap_or_default();
    let req = format!(
        "POST / HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let attempt = async {
        let mut s = tokio::net::TcpStream::connect(addr).await.ok()?;
        s.write_all(req.as_bytes()).await.ok()?;
        let mut head = [0u8; 12];
        let mut got = 0;
        while got < head.len() {
            let n = s.read(&mut head[got..]).await.ok()?;
            if n == 0 {
                break;
            }
            got += n;
        }
        Some(head[..got].starts_with(b"HTTP/"))
    };
    matches!(tokio::time::timeout(Duration::from_secs(2), attempt).await, Ok(Some(true)))
}

/// Held for as long as this agent runs. Two agents on one data folder would each adopt the other's node and
/// overwrite one another's `state.json`, so the second one is refused.
#[derive(Debug)]
pub struct AgentLock {
    _file: std::fs::File,
}

pub const LOCK_FILE: &str = "agent.lock";

/// Take `<root>/control/agent.lock` (an exclusive `flock`, released by the system when the process ends, however
/// it ends). `Err` carries the sentence to log when another agent holds it.
pub fn take_lock(root: &std::path::Path) -> Result<AgentLock, String> {
    let dir = control::dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    let path = dir.join(LOCK_FILE);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("Cannot open {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: plain flock(2) on a descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let holder = std::fs::read_to_string(&path).ok().and_then(|t| t.trim().parse::<u32>().ok());
            return Err(format!(
                "Another node agent is already running on {}{}. Only one agent can use a data folder, so this one is exiting.",
                root.display(),
                holder.map(|p| format!(" (pid {p})")).unwrap_or_default()
            ));
        }
    }
    {
        use std::io::{Seek, Write};
        let mut f = &file;
        let _ = file.set_len(0);
        let _ = f.seek(std::io::SeekFrom::Start(0));
        let _ = write!(f, "{}", std::process::id());
    }
    Ok(AgentLock { _file: file })
}

/// The `agent` subcommand: run until SIGTERM / SIGINT, then stop the node and return. `Err` when another
/// agent already runs on this data folder.
pub async fn run_forever(opts: DaemonOpts) -> Result<(), String> {
    let _lock = take_lock(&opts.root)?;
    let daemon = Daemon::new(opts);
    daemon
        .run(async {
            crate::signals::shutdown_signal().await;
            tracing::info!("agent: stop requested");
        })
        .await;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::AtomicBool;

    #[tokio::test]
    async fn rpc_answers_needs_an_http_reply() {
        // A listener that speaks HTTP: counts, even with an error status.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                tokio::spawn(async move {
                    let mut b = [0u8; 512];
                    let _ = s.read(&mut b).await;
                    let _ = s.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").await;
                });
            }
        });
        assert!(rpc_answers(&addr).await);
        // A listener that accepts and says nothing useful.
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr2 = l2.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l2.accept().await else { return };
                let _ = s.write_all(b"SSH-2.0-nope\r\n").await;
            }
        });
        assert!(!rpc_answers(&addr2).await, "not HTTP: not the node");
        // Nothing listening.
        let free = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().to_string()
        };
        assert!(!rpc_answers(&free).await);
    }

    // ---- the agent in one thread, the console's client in the test ----

    const FAKE: &str = r#"#!/bin/sh
if [ "$1" = "version" ] || [ "$1" = "--version" ]; then echo "blvm 9.9.9"; exit 0; fi
trap 'echo "got TERM"; exit 0' TERM
echo "fake node up: $*"
echo "cwd=$(pwd)"
while kill -0 $PPID 2>/dev/null; do sleep 0.1; done
"#;

    fn fast() -> Timings {
        Timings {
            start_timeout: Duration::from_secs(10),
            start_max: Duration::from_secs(30),
            owner_check: Duration::from_millis(100),
            stop_wait: Duration::from_millis(1500),
            kill_wait: Duration::from_secs(3),
            backoff: vec![Duration::from_millis(150), Duration::from_millis(250), Duration::from_millis(350)],
            crash_window: Duration::from_secs(60),
            ready_poll: Duration::from_millis(30),
            tail_poll: Duration::from_millis(30),
            log_max_bytes: 50 * 1024 * 1024,
            version_timeout: Duration::from_secs(5),
            meta_tick: Duration::from_millis(200),
            stop_give_up: None,
        }
    }

    struct Env {
        dir: PathBuf,
        root: PathBuf,
        bin: PathBuf,
        /// Free ports for this test's nodes: never the network's own (a regtest node of the person running the
        /// tests would make them fail).
        rpc_port: u16,
        p2p_port: u16,
        _reap: crate::agent::testkit::ReapNodes,
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn env(tag: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("blvm-daemon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-blvm.sh");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _reap = crate::agent::testkit::ReapNodes::under(&dir);
        Env { root: dir.join("data"), dir, bin, rpc_port: free_port(), p2p_port: free_port(), _reap }
    }

    fn opts(e: &Env, ready: Arc<AtomicBool>) -> DaemonOpts {
        let mut o = DaemonOpts::new(e.root.clone());
        o.timings = fast();
        o.p2p_port = Some(e.p2p_port);
        o.poll = Duration::from_millis(40);
        o.heartbeat = Duration::from_millis(300);
        o.ready = Some(Arc::new(move || ready.load(Ordering::Relaxed)));
        o
    }

    /// An agent process stand-in: its own thread and runtime.
    struct Running {
        graceful: Option<tokio::sync::oneshot::Sender<()>>,
        kill: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Running {
        fn spawn(o: DaemonOpts) -> Running {
            let (gtx, grx) = tokio::sync::oneshot::channel::<()>();
            let (ktx, krx) = tokio::sync::oneshot::channel::<()>();
            let thread = std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
                rt.block_on(async move {
                    let d = Daemon::new(o);
                    tokio::select! {
                        _ = d.run(async move { let _ = grx.await; }) => {}
                        _ = krx => {} // like kill -9: nothing gets to clean up
                    }
                });
                rt.shutdown_background();
            });
            Running { graceful: Some(gtx), kill: Some(ktx), thread: Some(thread) }
        }

        /// SIGTERM: the agent stops its node and exits.
        fn terminate(mut self) {
            let _ = self.graceful.take().unwrap().send(());
            self.thread.take().unwrap().join().unwrap();
        }

        /// kill -9 of the agent: the node is left running.
        fn kill(mut self) {
            let _ = self.kill.take().unwrap().send(());
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn client(e: &Env, net: &str) -> Agent {
        Agent::external(
            AgentConfig {
                bin: Some(e.bin.clone()),
                root: e.root.clone(),
                network: networks::by_id(net),
                rpc_addr: networks::by_id(net).map(|_| format!("127.0.0.1:{}", e.rpc_port)),
                assume_valid: None,
                child_env: Default::default(),
                rpc_auth: None,
                p2p_port: None,
            },
            fast(),
        )
    }

    async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    #[tokio::test]
    async fn the_console_drives_the_agent_through_the_control_files() {
        let e = env("loop");
        let ready = Arc::new(AtomicBool::new(false));
        let agent = Running::spawn(opts(&e, Arc::clone(&ready)));
        let web = client(&e, "regtest");
        assert!(web.is_external());
        wait_for("agent heartbeat", || web.alive()).await;
        assert_eq!(web.state(), ProcState::Stopped);
        // The agent reports the program before anything is asked.
        assert_eq!(web.binary_found(), Some(false), "default bin/blvm does not exist in this data folder");

        // Start: the view says `starting` at once, from the request alone.
        web.start().await.unwrap();
        assert_eq!(web.state(), ProcState::Starting);
        let d = control::read_desired(&e.root).unwrap();
        assert_eq!((d.generation, d.running, d.network.as_deref()), (1, true, Some("regtest")));
        assert_eq!(d.binary, e.bin.display().to_string());
        assert_eq!(d.stop_grace_secs, 1, "1.5 s rounds down to whole seconds, never to zero");
        wait_for("agent started the node", || control::read_state(&e.root).is_some_and(|s| s.pid.is_some() && s.generation == 1)).await;
        let st = control::read_state(&e.root).unwrap();
        assert_eq!(st.state, "starting");
        assert_eq!(st.network.as_deref(), Some("regtest"));
        let pid = st.pid.unwrap();
        assert!(crate::agent::pid_alive(pid));
        wait_for("the console sees the pid", || web.view().pid == Some(pid)).await;

        // `running` once the RPC answers (here: the agent's readiness).
        ready.store(true, Ordering::Relaxed);
        wait_for("running", || web.state() == ProcState::Running).await;
        wait_for("agent says running", || control::read_state(&e.root).is_some_and(|s| s.state == "running")).await;

        // Logs come from the shared file, with the node's cwd and arguments.
        wait_for("logs", || web.logs(None, 100).iter().any(|l| l.text.contains("fake node up"))).await;
        let text: String = web.logs(None, 100).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("--network regtest") && text.contains("networks/regtest"), "{text}");
        assert!(text.contains(&format!("--rpc-addr 127.0.0.1:{}", e.rpc_port)), "bind address defaults to loopback: {text}");
        assert!(text.contains(&format!("--listen-addr 127.0.0.1:{}", e.p2p_port)), "{text}");

        // Stop.
        web.request_stop();
        assert_eq!(web.state(), ProcState::Stopping, "shown at once");
        web.wait_inactive(Duration::from_secs(10)).await;
        assert_eq!(web.state(), ProcState::Stopped);
        wait_for("node gone", || !crate::agent::pid_alive(pid)).await;
        assert_eq!(control::read_state(&e.root).unwrap().generation, control::read_desired(&e.root).unwrap().generation);

        // Start again, then restart (new process).
        ready.store(false, Ordering::Relaxed);
        web.start().await.unwrap();
        ready.store(true, Ordering::Relaxed);
        wait_for("running again", || web.state() == ProcState::Running).await;
        let second = web.view().pid.unwrap();
        assert_ne!(second, pid);
        web.restart().await.unwrap();
        assert_eq!(web.state(), ProcState::Stopping);
        wait_for("restarted", || web.state() == ProcState::Running && web.view().pid.is_some_and(|p| p != second)).await;
        assert!(!crate::agent::pid_alive(second));

        // A network switch the way the switch code does it: stop, change the config, start.
        web.stop_and_wait().await;
        let mut cfg = web.config();
        cfg.network = networks::by_id("signet");
        cfg.rpc_addr = Some(networks::default_rpc(networks::by_id("signet").unwrap()));
        web.set_config(cfg);
        web.start().await.unwrap();
        wait_for("signet", || web.state() == ProcState::Running && web.view().network == Some("signet")).await;
        let d = control::read_desired(&e.root).unwrap();
        assert_eq!((d.network.as_deref(), d.running, d.rpc_port), (Some("signet"), true, Some(38332)));
        let st = control::read_state(&e.root).unwrap();
        assert_eq!(st.network.as_deref(), Some("signet"));
        wait_for("log of the signet start", || web.logs(None, 400).iter().any(|l| l.text.contains("--network signet"))).await;
        // A switch from the console without stopping first: the agent does it.
        let mut cfg = web.config();
        cfg.network = networks::by_id("regtest");
        cfg.rpc_addr = Some(networks::default_rpc(networks::by_id("regtest").unwrap()));
        web.set_config(cfg);
        web.restart().await.unwrap();
        wait_for("back on regtest", || web.state() == ProcState::Running && web.view().network == Some("regtest")).await;

        // The console quitting never touches the node.
        web.shutdown(true).await;
        assert!(web.state().is_active());
        let pid = web.view().pid.unwrap();

        // SIGTERM to the agent: the node is stopped, desired.json is left alone, state says stopped.
        agent.terminate();
        assert!(!crate::agent::pid_alive(pid));
        let st = control::read_state(&e.root).unwrap();
        assert_eq!((st.state.as_str(), st.pid), ("stopped", None));
        assert!(control::read_desired(&e.root).unwrap().running, "the next container start brings the node back");
        let _ = std::fs::remove_dir_all(&e.dir);
    }

    #[tokio::test]
    async fn a_restarted_agent_adopts_the_running_node_or_starts_it_again() {
        let e = env("adopt");
        let ready = Arc::new(AtomicBool::new(true));
        let first = Running::spawn(opts(&e, Arc::clone(&ready)));
        let web = client(&e, "regtest");
        wait_for("alive", || web.alive()).await;
        web.start().await.unwrap();
        wait_for("running", || web.state() == ProcState::Running).await;
        let pid = web.view().pid.unwrap();

        // kill -9 of the agent container's process: the node (own process group, file logs) lives on.
        first.kill();
        assert!(crate::agent::pid_alive(pid), "the node survives its agent");
        wait_for("heartbeat goes stale", || {
            control::read_state(&e.root).is_some_and(|s| !s.alive(crate::persist::unix_now()))
        })
        .await;
        assert!(!web.alive());

        // A new agent takes it over: same process, `adopted`, nothing restarted.
        let second = Running::spawn(opts(&e, Arc::clone(&ready)));
        wait_for("agent back", || web.alive()).await;
        wait_for("adopted", || control::read_state(&e.root).is_some_and(|s| s.adopted && s.state == "running")).await;
        assert_eq!(control::read_state(&e.root).unwrap().pid, Some(pid));
        assert!(crate::agent::pid_alive(pid));
        assert_eq!(web.view().pid, Some(pid));
        let before = control::read_desired(&e.root).unwrap().generation;
        assert_eq!(control::read_state(&e.root).unwrap().generation, before, "adopting is not a new request");

        // The node is stopped from the console as usual afterwards (the adopted node is supervised).
        web.stop_and_wait().await;
        wait_for("gone", || !crate::agent::pid_alive(pid)).await;
        web.start().await.unwrap();
        wait_for("running anew", || web.state() == ProcState::Running).await;
        let pid2 = web.view().pid.unwrap();
        assert_ne!(pid2, pid);

        // Container stop (SIGTERM): the node goes with it. The next container start finds desired.json
        // saying "run" and starts the node by itself.
        second.terminate();
        assert!(!crate::agent::pid_alive(pid2));
        let third = Running::spawn(opts(&e, Arc::clone(&ready)));
        wait_for("started by itself", || {
            control::read_state(&e.root).is_some_and(|s| s.state == "running" && s.pid.is_some_and(|p| p != pid2))
        })
        .await;
        let pid3 = control::read_state(&e.root).unwrap().pid.unwrap();

        // ... but a node the user stopped stays stopped across an agent restart.
        web.stop_and_wait().await;
        wait_for("gone", || !crate::agent::pid_alive(pid3)).await;
        third.terminate();
        let fourth = Running::spawn(opts(&e, Arc::clone(&ready)));
        wait_for("alive", || web.alive()).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(control::read_state(&e.root).unwrap().state, "stopped");
        assert!(!control::read_desired(&e.root).unwrap().running);
        fourth.terminate();
        let _ = std::fs::remove_dir_all(&e.dir);
    }

    #[tokio::test]
    async fn a_request_nobody_answers_is_reported_not_waited_for_forever() {
        let e = env("noagent");
        let web = client(&e, "regtest");
        assert_eq!(web.state(), ProcState::Stopped);
        assert!(!web.alive());
        web.start().await.unwrap();
        // Just asked: starting, for now.
        assert_eq!(web.state(), ProcState::Starting);
        // An old request and no heartbeat: the agent is not there.
        let mut d = control::read_desired(&e.root).unwrap();
        d.updated -= 60;
        control::write_desired(&e.root, &d).unwrap();
        let web2 = client(&e, "regtest");
        let v = web2.view();
        assert_eq!(v.state, ProcState::Crashed);
        assert!(v.last_error.unwrap().contains("agent is not responding"));
        // A start request is idempotent while one is pending; a stop of a non-running node writes nothing new.
        let g = control::read_desired(&e.root).unwrap().generation;
        web.request_stop();
        assert_eq!(control::read_desired(&e.root).unwrap().generation, g + 1, "stop cancels the pending start");
        web.request_stop();
        assert_eq!(control::read_desired(&e.root).unwrap().generation, g + 1, "and asking again changes nothing");
        // No network, no start.
        let mut cfg = web.config();
        cfg.network = None;
        web.set_config(cfg);
        assert!(web.start().await.unwrap_err().contains("Choose a network"));
        let _ = std::fs::remove_dir_all(&e.dir);
    }

    #[test]
    fn the_binary_is_resolved_like_the_node_image_expects() {
        let e = env("resolve");
        let mut o = DaemonOpts::new(e.root.clone());
        let d = Daemon::new(o.clone());
        let desired = |b: &str| Desired { binary: b.into(), ..Default::default() };
        // Relative to the data folder, even when it is not there (the error then names that path).
        assert_eq!(d.resolve_bin(&desired("bin/blvm")), e.root.join("bin/blvm"));
        // The bundled program fills in when the data folder has none.
        o.bundled_bin = Some(e.bin.clone());
        let d = Daemon::new(o.clone());
        assert_eq!(d.resolve_bin(&desired("bin/blvm")), e.bin);
        // ... but a program in the data folder wins.
        std::fs::create_dir_all(e.root.join("bin")).unwrap();
        std::fs::write(e.root.join("bin/blvm"), "x").unwrap();
        assert_eq!(d.resolve_bin(&desired("bin/blvm")), e.root.join("bin/blvm"));
        // Absolute paths are taken as given.
        assert_eq!(d.resolve_bin(&desired("/opt/x/blvm")), PathBuf::from("/opt/x/blvm"));
        // The node's RPC address: bind address from the environment, port from the console.
        let mut o2 = DaemonOpts::new(e.root.clone());
        o2.rpc_bind = "0.0.0.0".into();
        let cfg = Daemon::new(o2).config_for(&Desired { network: Some("signet".into()), ..Default::default() });
        assert_eq!(cfg.rpc_addr.as_deref(), Some("0.0.0.0:38332"));
        let cfg = Daemon::new(DaemonOpts::new(e.root.clone()))
            .config_for(&Desired { network: Some("signet".into()), rpc_port: Some(4242), ..Default::default() });
        assert_eq!(cfg.rpc_addr.as_deref(), Some("127.0.0.1:4242"));
        assert!(Daemon::new(DaemonOpts::new(e.root.clone())).config_for(&Desired::default()).network.is_none());
        let _ = std::fs::remove_dir_all(&e.dir);
    }

    // ---- review fixes: the RPC login, one agent per folder, the probe address ----

    #[test]
    fn the_agent_takes_its_login_from_the_container_environment() {
        std::env::set_var("BLVM_RPC_USER", "umbrel");
        std::env::set_var("BLVM_RPC_PASS", "seeded-secret");
        let o = DaemonOpts::from_env(PathBuf::from("/data"));
        std::env::remove_var("BLVM_RPC_USER");
        std::env::remove_var("BLVM_RPC_PASS");
        assert_eq!(o.rpc_auth, Some(RpcAuth { user: "umbrel".into(), pass: "seeded-secret".into() }));
        let cfg = Daemon::new(o).config_for(&Desired { network: Some("regtest".into()), ..Default::default() });
        assert_eq!(cfg.rpc_auth.as_ref().map(|a| a.user.as_str()), Some("umbrel"));
        // One of the two alone is no login at all.
        std::env::set_var("BLVM_RPC_USER", "umbrel");
        assert_eq!(DaemonOpts::from_env(PathBuf::from("/data")).rpc_auth, None);
        std::env::remove_var("BLVM_RPC_USER");
    }

    #[tokio::test]
    async fn the_agents_node_gets_the_config_file_and_a_bind_without_a_login_is_refused() {
        // With a login: a config file in the network folder, `--config` on the command line, bind beyond loopback.
        let e = env("login");
        let ready = Arc::new(AtomicBool::new(false));
        let mut o = opts(&e, Arc::clone(&ready));
        o.rpc_bind = "0.0.0.0".into();
        o.rpc_auth = Some(RpcAuth { user: "umbrel".into(), pass: "seeded-secret".into() });
        let agent = Running::spawn(o);
        let web = client(&e, "regtest");
        wait_for("alive", || web.alive()).await;
        web.start().await.unwrap();
        wait_for("started", || control::read_state(&e.root).is_some_and(|s| s.pid.is_some())).await;
        let text = std::fs::read_to_string(e.root.join("networks/regtest/blvm.toml")).unwrap();
        assert!(text.contains("username = \"umbrel\"") && text.contains("password = \"seeded-secret\""), "{text}");
        wait_for("log", || web.logs(None, 100).iter().any(|l| l.text.contains("fake node up"))).await;
        let log: String = web.logs(None, 100).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n");
        assert!(log.contains(&format!("--rpc-addr 0.0.0.0:{}", e.rpc_port)) && log.contains("--config ./blvm.toml"), "{log}");
        assert!(!log.contains("seeded-secret"));
        agent.terminate();
        let _ = std::fs::remove_dir_all(&e.dir);

        // Without one: the node is not started, and the page is told why.
        let e = env("nologin");
        let mut o = opts(&e, Arc::new(AtomicBool::new(false)));
        o.rpc_bind = "0.0.0.0".into();
        let agent = Running::spawn(o);
        let web = client(&e, "regtest");
        wait_for("alive", || web.alive()).await;
        web.start().await.unwrap();
        wait_for("refused", || control::read_state(&e.root).is_some_and(|s| s.state == "crashed")).await;
        let st = control::read_state(&e.root).unwrap();
        assert!(st.last_error.unwrap_or_default().contains("BLVM_RPC_PASS"));
        assert_eq!(st.pid, None);
        assert!(!e.root.join("networks/regtest/blvm.toml").exists());
        agent.terminate();
        let _ = std::fs::remove_dir_all(&e.dir);
    }

    #[test]
    fn a_second_agent_on_the_same_data_folder_is_refused_and_the_lock_goes_with_the_first() {
        let e = env("lock");
        let first = take_lock(&e.root).expect("the first agent gets the lock");
        let msg = take_lock(&e.root).unwrap_err();
        assert!(msg.contains("Another node agent is already running") && msg.contains(&std::process::id().to_string()), "{msg}");
        // Another data folder is another agent.
        let other = env("lock2");
        assert!(take_lock(&other.root).is_ok());
        drop(first);
        assert!(take_lock(&e.root).is_ok(), "the lock is released when the first agent ends");
        let _ = std::fs::remove_dir_all(&e.dir);
        let _ = std::fs::remove_dir_all(&other.dir);
    }

    #[tokio::test]
    async fn run_forever_exits_with_a_message_when_another_agent_holds_the_folder() {
        let e = env("lock3");
        let _held = take_lock(&e.root).unwrap();
        // (An agent that wrongly started would run for ever: the limit turns that into a failure.)
        let r = tokio::time::timeout(Duration::from_secs(5), run_forever(DaemonOpts::new(e.root.clone())))
            .await
            .expect("the second agent must exit at once, not run");
        assert!(r.unwrap_err().contains("Another node agent"));
        let _ = std::fs::remove_dir_all(&e.dir);
    }

    /// REVIEW GAP (mutation c): once a generation is applied the daemon must not act on it again; otherwise a
    /// crashing node is restarted on every tick, bypassing the agent's backoff and its give-up limit.
    #[tokio::test]
    async fn review_an_applied_generation_is_not_applied_again_after_the_node_crashes() {
        let e = env("review-reapply");
        let crash = e.dir.join("crash-blvm.sh");
        std::fs::write(&crash, "#!/bin/sh\nif [ \"$1\" = version ] || [ \"$1\" = --version ]; then echo 'blvm 9.9.9'; exit 0; fi\necho boom\nexit 1\n").unwrap();
        std::fs::set_permissions(&crash, std::fs::Permissions::from_mode(0o755)).unwrap();
        let ready = Arc::new(AtomicBool::new(false));
        let agent = Running::spawn(opts(&e, Arc::clone(&ready)));
        let mut web = client(&e, "regtest");
        let mut cfg = web.config();
        cfg.bin = Some(crash.clone());
        web.set_config(cfg);
        wait_for("alive", || web.alive()).await;
        web.start().await.unwrap();
        let log = networks::log_file(&e.root, networks::by_id("regtest").unwrap());
        let starts = || std::fs::read_to_string(&log).unwrap_or_default().matches("[console] starting").count();
        // 1 start + 3 backoff restarts (150/250/350 ms), then the agent gives up.
        tokio::time::sleep(Duration::from_secs(4)).await;
        let a = starts();
        tokio::time::sleep(Duration::from_secs(2)).await;
        let b = starts();
        assert!(a >= 1 && a <= 4, "the agent's own backoff allows at most 4 starts, saw {a}");
        assert_eq!(a, b, "nothing restarts a node the agent gave up on");
        web = client(&e, "regtest");
        let _ = web;
        agent.terminate();
        let _ = std::fs::remove_dir_all(&e.dir);
    }
}
