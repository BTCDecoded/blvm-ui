//! Switching the managed node to another network (managed-node design C, F).
//!
//! * `GET|POST /api/network/preview`: what the confirmation dialog shows, and whether the switch is
//!   allowed at all.
//! * `POST /api/network/switch`: validates the same way, answers 202 with a switch id, and runs the
//!   rest in the background. Progress is `managed.switch` in `/api/status`:
//!   `stopping` (graceful stop of the old node, its data stays) -> `starting` (the new network's
//!   node is spawned in its own folder and the console's RPC target follows) -> `done`, or `failed`
//!   with the last 50 log lines. A finished switch stays in the status for 30 seconds.
//!
//! Rules worth knowing:
//! * One switch at a time (a second request is 409 `busy`). While it runs, Start/Stop/Restart,
//!   `/api/settings` network changes and setup are refused.
//! * A switch while the node is stopped skips the stopping phase and still starts the new network.
//! * A switch while the node is `starting`, `stopping` or waiting to restart after a crash is
//!   accepted: it cancels any queued restart, waits for the old process to be gone, and goes on.
//! * `settings.network` (and with it `console.json`) changes only after the new node has been
//!   spawned. A stop that does not finish, or a spawn that fails, leaves the old network chosen.
//!   A console that dies in the middle therefore wakes up on the old network, unless the new node had
//!   already been spawned: then the saved network is the new one and the next console adopts that
//!   node through `control/node.pid`.
//! * A new node that crashes, stops by itself, or never answers fails the switch. The console then
//!   already points at the new network (its node was spawned), the node card shows the crash, and
//!   "Try again" starts it; the automatic restart is cancelled so the two do not race.

use crate::agent::{assume_valid_args, ProcState};
use crate::managed::{self, DiskCache};
use crate::networks::{self, Network};
use crate::state::{stamp_power, volume_space_at, Shared};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Below this much free space on the data volume a switch is refused.
pub const MIN_FREE_BYTES: u64 = 5_000_000_000;
/// A finished switch stays in `managed.switch` this long, so the page can show how it ended.
pub const LINGER: Duration = Duration::from_secs(30);
const LOG_LINES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SwitchPhase {
    Stopping,
    Starting,
    Done,
    Failed,
}

/// The switch as the console remembers it.
#[derive(Debug, Clone)]
pub struct SwitchState {
    pub id: String,
    pub from: &'static str,
    pub to: &'static str,
    pub phase: SwitchPhase,
    /// Unix seconds.
    pub started: u64,
    pub error: Option<String>,
    /// Last 50 log lines, only when `failed`.
    pub log: Vec<String>,
    pub finished: Option<Instant>,
}

/// `managed.switch` in `/api/status`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SwitchView {
    pub id: String,
    pub from: &'static str,
    pub to: &'static str,
    pub phase: SwitchPhase,
    pub started: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub log: Vec<String>,
}

static COUNTER: AtomicU64 = AtomicU64::new(1);

impl SwitchState {
    pub fn new(from: &'static str, to: &'static str, phase: SwitchPhase) -> Self {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        Self {
            id: format!("sw-{ms:x}-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
            from,
            to,
            phase,
            started: crate::persist::unix_now(),
            error: None,
            log: Vec::new(),
            finished: None,
        }
    }

    /// Stopping or starting: the switch still owns the node.
    pub fn active(&self) -> bool {
        matches!(self.phase, SwitchPhase::Stopping | SwitchPhase::Starting)
    }

    /// Finished more than [`LINGER`] ago.
    pub fn expired(&self) -> bool {
        self.finished.is_some_and(|t| t.elapsed() > LINGER)
    }

    pub fn view(&self) -> SwitchView {
        SwitchView {
            id: self.id.clone(),
            from: self.from,
            to: self.to,
            phase: self.phase,
            started: self.started,
            error: self.error.clone(),
            log: self.log.clone(),
        }
    }
}

// ---- validation -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoNetwork,
    Same(&'static str),
    Busy,
    NoDisk,
}

impl Refusal {
    pub fn reason(&self) -> String {
        match self {
            Refusal::NoNetwork => "No network has been chosen yet. Pick one in setup first.".into(),
            Refusal::Same(label) => format!("Already on {label}."),
            Refusal::Busy => "A network switch is already running.".into(),
            Refusal::NoDisk => {
                "Less than 5 GB is free on this disk, so the switch is blocked until you free up space.".into()
            }
        }
    }

    /// HTTP status of a refused `POST /api/network/switch`.
    pub fn status(&self) -> u16 {
        match self {
            Refusal::Busy => 409,
            _ => 400,
        }
    }
}

/// The one rule set behind `allowed` (preview) and the 4xx answers (switch).
pub fn judge(current: Option<&'static Network>, to: &Network, switch_active: bool, free_bytes: u64) -> Option<Refusal> {
    let Some(cur) = current else {
        return Some(Refusal::NoNetwork);
    };
    if switch_active {
        return Some(Refusal::Busy);
    }
    if cur.id == to.id {
        return Some(Refusal::Same(to.label));
    }
    if free_bytes < MIN_FREE_BYTES {
        return Some(Refusal::NoDisk);
    }
    None
}

fn err(code: u16, msg: impl Into<String>) -> (u16, Value) {
    (code, json!({ "ok": false, "error": msg.into() }))
}

fn parse_target(id: &str) -> Result<&'static Network, (u16, Value)> {
    let id = id.trim();
    if id.is_empty() {
        return Err(err(400, "Say which network: {\"to\": \"signet\"}."));
    }
    networks::by_id(id).ok_or_else(|| err(400, format!("Unknown network \"{id}\".")))
}

/// `to` (design F) or `network` from a request body.
pub fn target_of(body: &Value) -> &str {
    body.get("to").or_else(|| body.get("network")).and_then(Value::as_str).unwrap_or("")
}

// ---- preview ----------------------------------------------------------------------------------

/// Make sure the folder sizes were measured at least once (the first second after start-up they
/// have not been). Runs the measurement on a blocking thread.
async fn ensure_measured(state: &Shared) {
    let (cache, root) = {
        let g = state.read().await;
        (g.console.disk_snapshot(), g.console.eff.data_root.clone())
    };
    if cache_ready(&cache) {
        return;
    }
    let _ = tokio::task::spawn_blocking(move || managed::run_disk_job(cache, root)).await;
}

fn cache_ready(cache: &std::sync::Arc<std::sync::Mutex<DiskCache>>) -> bool {
    cache.lock().unwrap().measured()
}

pub async fn preview(state: &Shared, to_id: &str) -> (u16, Value) {
    let to = match parse_target(to_id) {
        Ok(n) => n,
        Err(e) => return e,
    };
    if !state.read().await.console.is_managed() {
        return err(404, "This console is not managing a node.");
    }
    ensure_measured(state).await;
    let g = state.read().await;
    let c = &g.console;
    if c.agent.is_none() {
        return err(404, "This console is not managing a node.");
    }
    let root = &c.eff.data_root;
    let free = volume_space_at(root).1;
    let cur = c.network();
    let refusal = judge(cur, to, c.switch_active(), free);

    let cache = c.disk_snapshot();
    let (from_disk, to_disk) = {
        let d = cache.lock().unwrap();
        (cur.map(|n| d.net(n.id)).unwrap_or_default(), d.net(to.id))
    };
    // Fresh from the files rather than the 60 s cache: the dialog should not quote an old height.
    let to_meta = networks::read_meta(root, to).or(to_disk.meta.clone()).unwrap_or_default();
    let from_meta = cur.and_then(|n| networks::read_meta(root, n)).or(from_disk.meta.clone()).unwrap_or_default();

    let snap = &g.snapshot;
    let active = c.agent.as_ref().is_some_and(|a| a.state().is_active());
    let from_height = match cur {
        Some(_) if snap.connected && snap.local_height > 0 => Some(snap.local_height),
        _ => from_meta.last_height.filter(|h| *h > 0),
    };
    let from_ibd_pct = (cur.is_some() && active && snap.connected && snap.ibd && snap.network_height > 0).then(|| {
        let p = snap.local_height as f64 / snap.network_height as f64 * 100.0;
        (p.clamp(0.0, 100.0) * 10.0).round() / 10.0
    });
    let to_height = to_meta.last_height.filter(|h| *h > 0 && to_disk.has_data);

    let mut warnings: Vec<String> = Vec::new();
    if assume_valid_args(c.settings.assume_valid.as_deref(), to).iter().any(|a| a == "--noassumevalid")
        && to.id != "regtest"
    {
        warnings.push(format!(
            "{} checks every block in full (assume-valid is off), so it syncs more slowly.",
            to.label
        ));
    }

    let body = json!({
        "from": cur.map(|n| n.id),
        "to": to.id,
        "from_bytes": from_disk.bytes,
        "to_bytes": to_disk.bytes,
        "from_height": from_height,
        "to_height": to_height,
        "to_last_used": to_meta.last_used,
        "from_ibd_pct": from_ibd_pct,
        "free_bytes": free,
        "estimate_bytes": to.estimate_bytes,
        "blocked": matches!(refusal, Some(Refusal::NoDisk)),
        "warnings": warnings,
        "allowed": refusal.is_none(),
        "reason": refusal.as_ref().map(Refusal::reason),
    });
    (200, body)
}

// ---- POST /api/network/switch -------------------------------------------------------------------

/// Validate, claim the node for the switch and start it in the background. 202 on success.
pub async fn begin(state: &Shared, to_id: &str) -> (u16, Value) {
    let to = match parse_target(to_id) {
        Ok(n) => n,
        Err(e) => return e,
    };
    // Same lock the power routes take, so a Start/Stop click cannot slip in between the check and
    // the claim. It is released at once; the switch itself does not hold it.
    let lock = state.read().await.power_lock.clone();
    let _held = lock.lock().await;
    let mut g = state.write().await;
    if !g.console.is_managed() {
        return err(404, "This console is not managing a node.");
    }
    let Some(agent) = g.console.agent.clone() else {
        return err(404, "This console is not managing a node.");
    };
    if g.console.job_active() {
        return (409, json!({ "ok": false, "error": "busy", "reason": "The node is being installed or updated. Wait for that to finish." }));
    }
    let free = volume_space_at(&g.console.eff.data_root).1;
    let cur = g.console.network();
    if let Some(r) = judge(cur, to, g.console.switch_active(), free) {
        let body = match r {
            Refusal::Busy => json!({ "ok": false, "error": "busy", "reason": r.reason() }),
            _ => json!({ "ok": false, "error": r.reason() }),
        };
        return (r.status(), body);
    }
    let from = cur.expect("judge refuses when no network is chosen");
    let phase = if agent.state() == ProcState::Stopped { SwitchPhase::Starting } else { SwitchPhase::Stopping };
    let sw = SwitchState::new(from.id, to.id, phase);
    let id = sw.id.clone();
    let view = sw.view();
    g.console.switch = Some(sw);
    let new_cfg = g.console.agent_config_for(Some(to));
    stamp_power(&mut g);
    let status = serde_json::to_value(&g.snapshot).unwrap_or(Value::Null);
    drop(g);

    let st = std::sync::Arc::clone(state);
    let run_id = id.clone();
    tokio::spawn(async move {
        let res = run(&st, &run_id, from, to, agent, new_cfg, phase).await;
        finish(&st, &run_id, res).await;
    });
    (202, json!({ "ok": true, "id": id, "switch": view, "status": status }))
}

struct Failure {
    error: String,
    log: Vec<String>,
}

impl Failure {
    fn new(error: impl Into<String>, mut log: Vec<String>) -> Self {
        if log.len() > LOG_LINES {
            log.drain(..log.len() - LOG_LINES);
        }
        Self { error: error.into(), log }
    }
}

fn last_lines(agent: &crate::agent::Agent) -> Vec<String> {
    agent.logs(None, LOG_LINES).into_iter().map(|l| l.text).collect()
}

async fn set_phase(state: &Shared, id: &str, phase: SwitchPhase) {
    let mut g = state.write().await;
    if let Some(sw) = g.console.switch.as_mut().filter(|s| s.id == id) {
        sw.phase = phase;
    }
    stamp_power(&mut g);
}

async fn run(
    state: &Shared,
    id: &str,
    from: &'static Network,
    to: &'static Network,
    agent: crate::agent::Agent,
    new_cfg: crate::agent::AgentConfig,
    first_phase: SwitchPhase,
) -> Result<(), Failure> {
    // 1. Stop the old node. Its data stays where it is.
    if first_phase == SwitchPhase::Stopping {
        agent.cancel_restart();
        agent.request_stop();
        let limit = agent.stop_limit();
        if !agent.wait_inactive(limit).await {
            return Err(Failure::new(
                format!(
                    "The node on {} did not stop within {} seconds, so the switch was cancelled and nothing was changed.",
                    from.label,
                    limit.as_secs().max(1)
                ),
                last_lines(&agent),
            ));
        }
        set_phase(state, id, SwitchPhase::Starting).await;
    }

    // 2. Start the new network's node in its own folder. The saved network changes only after the
    //    process exists.
    let old_cfg = agent.config();
    state.read().await.console.persist_login();
    agent.set_config(new_cfg);
    if let Err(e) = agent.start().await {
        agent.set_config(old_cfg);
        return Err(Failure::new(e, last_lines(&agent)));
    }
    {
        let mut g = state.write().await;
        g.console.settings.network = Some(to.id.into());
        g.console.settings.node_wanted = Some(true);
        let saved = g.console.save();
        g.console.reresolve();
        let rpc = g.console.initial_rpc();
        g.switch_rpc(rpc); // the page's polled RPC target follows; also re-stamps the status
        g.console.invalidate_disk();
        drop(g);
        if let Err(e) = saved {
            tracing::warn!("network switch: could not save console.json: {e}");
        }
    }
    tracing::info!(from = from.id, to = to.id, "network switch: new node spawned");

    // 3. Wait until it answers (the agent itself gives up on a silent node after its start timeout).
    let t0 = Instant::now();
    let limit = agent.start_limit() + Duration::from_secs(30);
    loop {
        match agent.state() {
            ProcState::Running => return Ok(()),
            ProcState::Crashed => {
                agent.abandon_restart();
                let v = agent.view();
                let log = if v.crash_log.is_empty() { last_lines(&agent) } else { v.crash_log.clone() };
                return Err(Failure::new(
                    v.last_error.unwrap_or_else(|| format!("The node on {} stopped unexpectedly.", to.label)),
                    log,
                ));
            }
            ProcState::Stopped => {
                return Err(Failure::new(
                    format!("The node on {} stopped right after it started.", to.label),
                    last_lines(&agent),
                ));
            }
            ProcState::Starting | ProcState::Stopping => {}
        }
        if t0.elapsed() > limit {
            return Err(Failure::new(
                format!("The node on {} did not answer in time.", to.label),
                last_lines(&agent),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn finish(state: &Shared, id: &str, res: Result<(), Failure>) {
    let mut g = state.write().await;
    if let Some(sw) = g.console.switch.as_mut().filter(|s| s.id == id) {
        match res {
            Ok(()) => sw.phase = SwitchPhase::Done,
            Err(f) => {
                tracing::warn!(from = sw.from, to = sw.to, error = %f.error, "network switch failed");
                sw.phase = SwitchPhase::Failed;
                sw.error = Some(f.error);
                sw.log = f.log;
            }
        }
        sw.finished = Some(Instant::now());
    }
    g.console.invalidate_disk();
    stamp_power(&mut g);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::agent::{Agent, Timings};
    use crate::managed::{self, Console};
    use crate::settings::{self, ConsoleSettings, EnvOverrides};
    use crate::state::LiveState;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// FAKE_MODE: `slowstop` takes 0.5 s to honour SIGTERM, `stubborn` ignores it, `crash` dies after
    /// printing, anything else is a node that stops at once.
    const FAKE: &str = r#"#!/bin/sh
if [ "$1" = version ]; then echo 'blvm 9.9.9'; exit 0; fi
case "$FAKE_MODE" in
  slowstop) trap 'echo "got TERM"; sleep 0.5; exit 0' TERM ;;
  stubborn) trap '' TERM ;;
  crash) ;;
  *) trap 'echo "got TERM"; exit 0' TERM ;;
esac
echo "fake node up: $*"
if [ "$FAKE_MODE" = crash ]; then sleep 0.3; echo "about to crash"; exit 3; fi
while kill -0 $PPID 2>/dev/null; do sleep 0.1; done
"#;

    struct Fix {
        dir: PathBuf,
        state: Shared,
        ready: Arc<AtomicBool>,
        /// Where each network's node answers in this test: free loopback ports, never the networks' own (a node
        /// of the person running the tests would make the tests fail).
        rpc: std::collections::BTreeMap<&'static str, String>,
        _reap: crate::agent::testkit::ReapNodes,
    }

    impl Fix {
        fn rpc(&self, net: &str) -> String {
            self.rpc[net].clone()
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn timings() -> Timings {
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
            meta_tick: Duration::from_millis(100),
            stop_give_up: None,
        }
    }

    /// A managed console on `network` (regtest unless `None`), with a fake node and a readiness flag
    /// the test flips by hand.
    fn fixture(tag: &str, network: Option<&str>, fake_mode: &str, t: Timings) -> Fix {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("blvm-switch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-blvm.sh");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = EnvOverrides {
            mode: Some("managed".into()),
            node_bin: Some(bin.display().to_string()),
            data_root: Some(dir.join("root").display().to_string()),
            ..Default::default()
        };
        let mut s = ConsoleSettings { network: network.map(Into::into), ..Default::default() };
        s.child_env.insert("FAKE_MODE".into(), fake_mode.into());
        let rpc: std::collections::BTreeMap<&'static str, String> =
            networks::ALL.iter().map(|n| (n.id, format!("127.0.0.1:{}", free_port()))).collect();
        for (id, addr) in &rpc {
            s.rpc_overrides.insert((*id).into(), addr.clone());
        }
        settings::save_console(&dir.join("state"), &s).unwrap();
        let mut console = Console::load(Some(dir.join("state")), env, None);
        console.timings = t.clone();
        console.p2p_port = Some(free_port());
        console.agent = None;
        console.reresolve();
        let ready = Arc::new(AtomicBool::new(false));
        let r2 = Arc::clone(&ready);
        let cfg = console.agent_config();
        let agent = Agent::with_ready(cfg, t, Arc::new(move || r2.load(Ordering::Relaxed)));
        agent.skip_login_check(); // the fake node has no RPC server to ask
        console.agent = Some(agent);
        let mut live = LiveState::new(console.initial_rpc());
        live.console = console;
        stamp_power(&mut live);
        let _reap = crate::agent::testkit::ReapNodes::under(&dir);
        Fix { dir, state: Arc::new(RwLock::new(live)), ready, rpc, _reap }
    }

    async fn agent(f: &Fix) -> Agent {
        f.state.read().await.console.agent.clone().unwrap()
    }

    async fn sw(f: &Fix) -> Option<SwitchState> {
        f.state.read().await.console.switch.clone()
    }

    async fn phase_of(f: &Fix) -> Option<SwitchPhase> {
        sw(f).await.map(|s| s.phase)
    }

    async fn wait_phase(f: &Fix, want: SwitchPhase) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if phase_of(f).await == Some(want) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for phase {want:?}; now {:?}", sw(f).await);
    }

    async fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    async fn start_running(f: &Fix) -> Agent {
        let a = agent(f).await;
        f.ready.store(true, Ordering::Relaxed);
        a.start().await.unwrap();
        wait_until("running", || a.state() == ProcState::Running).await;
        a
    }

    fn log_text(a: &Agent) -> String {
        a.logs(None, 400).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n")
    }

    fn console_json(f: &Fix) -> ConsoleSettings {
        settings::load_console(&f.dir.join("state"))
    }

    fn kill_all(a: &Agent) {
        if let Some(pid) = a.view().pid {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
    }

    fn cleanup(f: &Fix) {
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[test]
    fn the_rules_in_one_place() {
        let reg = networks::by_id("regtest");
        let sig = networks::by_id("signet").unwrap();
        let big = 100 * MIN_FREE_BYTES;
        assert_eq!(judge(None, sig, false, big), Some(Refusal::NoNetwork), "no default network, ever");
        assert_eq!(judge(reg, sig, true, big), Some(Refusal::Busy));
        assert_eq!(judge(reg, networks::by_id("regtest").unwrap(), false, big), Some(Refusal::Same("Regtest")));
        assert_eq!(judge(reg, sig, false, MIN_FREE_BYTES - 1), Some(Refusal::NoDisk));
        assert_eq!(judge(reg, sig, false, MIN_FREE_BYTES), None);
        assert_eq!(Refusal::Busy.status(), 409);
        assert_eq!(Refusal::NoDisk.status(), 400);
        assert!(Refusal::NoNetwork.reason().contains("No network"));
        assert!(Refusal::Same("Regtest").reason().contains("Already on Regtest"));
        assert_eq!(target_of(&json!({"to":"signet"})), "signet");
        assert_eq!(target_of(&json!({"network":"signet"})), "signet");
        assert_eq!(target_of(&json!({"nope":1})), "");
    }

    #[test]
    fn a_finished_switch_lingers_for_thirty_seconds() {
        let mut s = SwitchState::new("regtest", "signet", SwitchPhase::Starting);
        assert!(s.active() && !s.expired());
        s.phase = SwitchPhase::Done;
        s.finished = Some(Instant::now());
        assert!(!s.active() && !s.expired());
        if let Some(old) = Instant::now().checked_sub(LINGER + Duration::from_secs(1)) {
            s.finished = Some(old);
            assert!(s.expired());
        }
        // The wire shape: error and log only when there is something to say.
        let v = serde_json::to_value(s.view()).unwrap();
        assert_eq!(v["phase"], "done");
        assert!(v.get("error").is_none() && v.get("log").is_none());
        assert!(v["id"].as_str().unwrap().starts_with("sw-"));
        assert!(v["started"].as_u64().unwrap() > 1_700_000_000);
        s.phase = SwitchPhase::Failed;
        s.error = Some("boom".into());
        s.log = vec!["a".into()];
        let v = serde_json::to_value(s.view()).unwrap();
        assert_eq!((v["error"].as_str(), v["log"][0].as_str()), (Some("boom"), Some("a")));
        // Two switches never share an id.
        assert_ne!(SwitchState::new("a_", "b_", SwitchPhase::Starting).id, SwitchState::new("a_", "b_", SwitchPhase::Starting).id);
    }

    #[tokio::test]
    async fn preview_describes_the_switch_and_refuses_the_impossible() {
        let f = fixture("preview", Some("regtest"), "run", timings());
        // Folders with known contents: regtest has chain data and a meta file, signet has neither.
        let root = f.dir.join("root");
        let reg = networks::by_id("regtest").unwrap();
        let reg_dir = networks::dir(&root, reg);
        std::fs::create_dir_all(reg_dir.join("data")).unwrap();
        std::fs::write(reg_dir.join("data/blk0.dat"), vec![1u8; 300_000]).unwrap();
        networks::write_meta(
            &root,
            reg,
            &networks::Meta { created: Some(10), last_used: Some(20), last_height: Some(150), last_header: None },
        )
        .unwrap();

        let (code, v) = preview(&f.state, "signet").await;
        assert_eq!(code, 200, "{v}");
        assert_eq!((v["from"].as_str(), v["to"].as_str()), (Some("regtest"), Some("signet")));
        assert_eq!(v["allowed"], true);
        assert_eq!(v["reason"], Value::Null);
        assert_eq!(v["blocked"], false);
        assert!(v["from_bytes"].as_u64().unwrap() >= 300_000, "{v}");
        assert_eq!(v["to_bytes"], 0);
        assert_eq!(v["to_height"], Value::Null, "signet has no data");
        assert_eq!(v["from_height"], 150, "from the meta file when the node is not answering");
        assert_eq!(v["from_ibd_pct"], Value::Null);
        assert_eq!(v["estimate_bytes"], Value::Null, "signet is not measured");
        assert!(v["free_bytes"].as_u64().unwrap() > MIN_FREE_BYTES, "this machine has room");
        assert_eq!(v["warnings"], json!([]));

        // A target with data reports its height and last use.
        std::fs::create_dir_all(networks::dir(&root, networks::by_id("signet").unwrap()).join("data")).unwrap();
        networks::write_meta(
            &root,
            networks::by_id("signet").unwrap(),
            &networks::Meta { created: Some(1), last_used: Some(2), last_height: Some(210_000), last_header: None },
        )
        .unwrap();
        std::fs::write(networks::dir(&root, networks::by_id("signet").unwrap()).join("data/x"), b"1").unwrap();
        // Force a fresh measurement (the cache was taken before the files existed).
        f.state.read().await.console.invalidate_disk();
        let cache = f.state.read().await.console.disk_snapshot();
        managed::run_disk_job(cache, root.clone());
        let (_, v) = preview(&f.state, "signet").await;
        assert_eq!(v["to_height"], 210_000);
        assert_eq!(v["to_last_used"], 2);

        // Testnet4 says it validates everything; mainnet carries the design's estimate.
        let (_, v) = preview(&f.state, "testnet4").await;
        assert!(v["warnings"][0].as_str().unwrap().contains("checks every block in full"), "{v}");
        let (_, v) = preview(&f.state, "mainnet").await;
        assert_eq!(v["estimate_bytes"], 800_000_000_000u64);

        // Same network, unknown network, nonsense.
        let (code, v) = preview(&f.state, "regtest").await;
        assert_eq!((code, v["allowed"].as_bool()), (200, Some(false)));
        assert!(v["reason"].as_str().unwrap().contains("Already on Regtest"), "{v}");
        let (code, v) = preview(&f.state, "narnia").await;
        assert_eq!(code, 400, "{v}");
        let (code, _) = preview(&f.state, "").await;
        assert_eq!(code, 400);

        // A switch in progress.
        f.state.write().await.console.switch = Some(SwitchState::new("regtest", "signet", SwitchPhase::Stopping));
        let (_, v) = preview(&f.state, "testnet4").await;
        assert_eq!(v["allowed"], false);
        assert!(v["reason"].as_str().unwrap().contains("already running"));
        cleanup(&f);

        // No network chosen: nothing is allowed, nothing defaulted.
        let f = fixture("preview-none", None, "run", timings());
        let (code, v) = preview(&f.state, "signet").await;
        assert_eq!((code, v["allowed"].as_bool(), v["from"].clone()), (200, Some(false), Value::Null));
        assert!(v["reason"].as_str().unwrap().contains("No network"));
        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 400, "{v}");
        assert!(f.state.read().await.console.switch.is_none());
        cleanup(&f);
    }

    #[tokio::test]
    async fn preview_reports_progress_of_a_syncing_node() {
        let f = fixture("preview-ibd", Some("regtest"), "run", timings());
        let a = start_running(&f).await;
        {
            let mut g = f.state.write().await;
            g.snapshot.connected = true;
            g.snapshot.ibd = true;
            g.snapshot.local_height = 640;
            g.snapshot.network_height = 1000;
        }
        let (_, v) = preview(&f.state, "signet").await;
        assert_eq!(v["from_ibd_pct"], 64.0, "{v}");
        assert_eq!(v["from_height"], 640);
        a.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn a_stopped_node_skips_stopping_and_the_network_is_saved_after_the_spawn() {
        let f = fixture("stopped", Some("regtest"), "run", timings());
        let a = agent(&f).await;
        assert_eq!(a.state(), ProcState::Stopped);
        assert_eq!(console_json(&f).network.as_deref(), Some("regtest"));

        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "{v}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["switch"]["phase"], "starting", "no stopping phase for a stopped node");
        assert_eq!((v["switch"]["from"].as_str(), v["switch"]["to"].as_str()), (Some("regtest"), Some("signet")));
        assert!(v["id"].as_str().unwrap().starts_with("sw-"));
        assert_eq!(v["status"]["managed"]["switch"]["id"], v["id"]);

        // The node is spawned (not answering yet): the network is saved, the page's RPC target moved.
        wait_until("signet node spawned", || a.state() == ProcState::Starting && a.view().pid.is_some()).await;
        wait_until("network saved", || console_json(&f).network.as_deref() == Some("signet")).await;
        assert_eq!(phase_of(&f).await, Some(SwitchPhase::Starting), "still waiting for the RPC");
        assert_eq!(f.state.read().await.rpc_addr, f.rpc("signet"));
        {
            // A parked snapshot of that network that says "connected" is a memory: the node has not answered yet.
            let mut g = f.state.write().await;
            g.snapshot.connected = true;
            stamp_power(&mut g);
            assert!(!g.snapshot.connected, "a node that was just spawned is not connected yet");
        }

        f.ready.store(true, Ordering::Relaxed);
        wait_phase(&f, SwitchPhase::Done).await;
        wait_until("node output", || log_text(&a).contains("--network signet")).await;
        let text = log_text(&a);
        assert!(text.contains(&format!("--rpc-addr {}", f.rpc("signet"))), "{text}");
        assert!(!text.contains("--noassumevalid"), "signet keeps the node's default assume-valid: {text}");
        assert!(text.contains("networks/signet"), "the node runs in the network's folder: {text}");
        // Status: process on the new network, switch visible, page target followed.
        let snap = f.state.read().await.snapshot.clone();
        assert_eq!(snap.rpc_addr, f.rpc("signet"));
        assert_eq!(snap.managed.as_ref().unwrap().network, Some("signet"));
        assert_eq!(snap.managed.as_ref().unwrap().switch.as_ref().unwrap().phase, SwitchPhase::Done);
        assert!(f.dir.join("root/networks/signet/meta.json").is_file(), "meta written on spawn");
        assert!(!f.dir.join("root/networks/regtest").exists(), "the old network was never started, so it has no folder");
        a.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn a_running_node_goes_through_stopping_then_starting_then_done_and_keeps_its_data() {
        let f = fixture("running", Some("regtest"), "slowstop", timings());
        let a = start_running(&f).await;
        wait_until("regtest output", || log_text(&a).contains("fake node up")).await;
        let root = f.dir.join("root");
        let reg = networks::by_id("regtest").unwrap();
        let reg_dir = networks::dir(&root, reg);
        std::fs::create_dir_all(reg_dir.join("data")).unwrap();
        std::fs::write(reg_dir.join("data/blk0.dat"), b"chain bytes").unwrap();
        a.note_height(&f.rpc("regtest"), 1234, 1300);
        a.note_height("127.0.0.1:9999", 5, 5); // another address: ignored
        let old_pid = a.view().pid.unwrap();
        f.ready.store(false, Ordering::Relaxed); // the new node must be seen as "starting" for a while

        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "{v}");
        assert_eq!(v["switch"]["phase"], "stopping");
        // Phases arrive in order. While stopping, the old network is still the saved one.
        wait_phase(&f, SwitchPhase::Stopping).await;
        assert_eq!(console_json(&f).network.as_deref(), Some("regtest"));
        assert_eq!(f.state.read().await.snapshot.managed.as_ref().unwrap().switch.as_ref().unwrap().phase, SwitchPhase::Stopping);
        wait_phase(&f, SwitchPhase::Starting).await;
        // The phase flips before the spawn: the agent first checks that the network's ports are free (B3).
        wait_until("signet node has a pid", || a.view().pid.is_some_and(|p| p != old_pid)).await;
        let new_pid = a.view().pid.expect("signet node has a pid");
        assert_ne!(new_pid, old_pid);
        wait_until("saved", || console_json(&f).network.as_deref() == Some("signet")).await;
        f.ready.store(true, Ordering::Relaxed);
        wait_phase(&f, SwitchPhase::Done).await;

        // The regtest folder is untouched, and its meta says where the node was.
        assert_eq!(std::fs::read(reg_dir.join("data/blk0.dat")).unwrap(), b"chain bytes");
        let meta = networks::read_meta(&root, reg).unwrap();
        assert_eq!(meta.last_height, Some(1234), "{meta:?}");
        assert_eq!(meta.last_header, Some(1300));
        assert!(meta.created.is_some() && meta.last_used.is_some());
        assert!(!reg_dir.join("meta.json.tmp").exists());
        // Signet got its own folder, with its own log and meta.
        let sig_dir = networks::dir(&root, networks::by_id("signet").unwrap());
        assert!(sig_dir.join("logs/node.log").is_file() && sig_dir.join("meta.json").is_file());
        let status = f.state.read().await.snapshot.clone();
        let m = status.managed.unwrap();
        let reg_row = m.networks.iter().find(|n| n.id == "regtest").unwrap();
        assert_eq!(m.network, Some("signet"));
        assert!(m.switch.is_some());
        // The network table is filled from the folders (after the cache refreshes).
        let cache = f.state.read().await.console.disk_snapshot();
        managed::run_disk_job(cache, root.clone());
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let m = f.state.read().await.snapshot.managed.clone().unwrap();
        let reg_row2 = m.networks.iter().find(|n| n.id == "regtest").unwrap();
        assert!(reg_row2.has_data && reg_row2.bytes >= 11, "{reg_row2:?} (before refresh {reg_row:?})");
        assert_eq!(reg_row2.last_height, Some(1234));
        assert!(reg_row2.last_used.is_some());
        let sig_row = m.networks.iter().find(|n| n.id == "signet").unwrap();
        assert!(!sig_row.has_data, "a fresh folder with only the node's module directory has no chain data yet");
        a.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn a_stop_that_does_not_finish_fails_the_switch_and_starts_nothing() {
        let mut t = timings();
        t.stop_wait = Duration::from_secs(30); // the agent would wait long; the switch gives up first
        t.stop_give_up = Some(Duration::from_millis(500));
        let f = fixture("stoptimeout", Some("regtest"), "stubborn", t);
        let a = start_running(&f).await;
        wait_until("node output", || log_text(&a).contains("fake node up")).await;
        let pid = a.view().pid.unwrap();

        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "{v}");
        wait_phase(&f, SwitchPhase::Failed).await;
        let s = sw(&f).await.unwrap();
        assert!(s.error.as_deref().unwrap().contains("did not stop"), "{:?}", s.error);
        assert!(s.error.as_deref().unwrap().contains("nothing was changed"));
        assert!(!s.log.is_empty() && s.log.len() <= 50, "log lines accompany the failure");
        // Nothing moved: same network saved, same config, no signet folder, no signet process.
        assert_eq!(console_json(&f).network.as_deref(), Some("regtest"));
        assert_eq!(f.state.read().await.console.settings.network.as_deref(), Some("regtest"));
        assert_eq!(a.config().network.map(|n| n.id), Some("regtest"));
        assert_eq!(f.state.read().await.rpc_addr, f.rpc("regtest"));
        assert!(!f.dir.join("root/networks/signet").exists());
        assert_eq!(a.state(), ProcState::Stopping, "still trying to stop; the console did not pretend otherwise");
        assert_eq!(a.view().pid, Some(pid));

        // The switch is over, so a node action is possible again and a new switch may be requested.
        let (code, _) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "a finished switch does not block the next one");
        wait_phase(&f, SwitchPhase::Failed).await;
        kill_all(&a);
        a.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn a_second_switch_and_node_actions_are_refused_while_one_runs() {
        let f = fixture("busy", Some("regtest"), "slowstop", timings());
        let a = start_running(&f).await;
        wait_until("node output", || log_text(&a).contains("fake node up")).await;

        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "{v}");
        let (code, v) = begin(&f.state, "testnet4").await;
        assert_eq!((code, v["error"].as_str()), (409, Some("busy")), "{v}");
        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!((code, v["error"].as_str()), (409, Some("busy")), "busy wins over same-network? {v}");
        // Start/stop/restart and settings changes are refused for the duration.
        for action in ["start", "stop", "restart", "toggle"] {
            let r = managed::node_action(&f.state, action).await;
            assert!(!r.ok, "{action}");
            assert!(r.error.unwrap().contains("switch is running"));
        }
        let (code, v) = managed::patch_settings(&f.state, &json!({"network":"testnet4"})).await;
        assert_eq!(code, 409, "{v}");
        let (code, v) = managed::setup(&f.state, &json!({"mode":"managed","network":"testnet4"})).await;
        assert_eq!(code, 409, "{v}");
        // The status says so.
        let snap = f.state.read().await.snapshot.clone();
        assert!(snap.node_busy);
        assert_eq!(snap.node_power_label, "Switching…");
        f.ready.store(true, Ordering::Relaxed);
        wait_phase(&f, SwitchPhase::Done).await;
        // Afterwards everything works again.
        let r = managed::node_action(&f.state, "stop").await;
        assert!(r.ok, "{:?}", r.error);
        a.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn a_spawn_that_fails_leaves_the_old_network_saved() {
        let f = fixture("spawnfail", Some("regtest"), "run", timings());
        // The binary vanishes after the console started: the node cannot be spawned.
        std::fs::remove_file(f.dir.join("fake-blvm.sh")).unwrap();
        let a = agent(&f).await;
        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "{v}");
        wait_phase(&f, SwitchPhase::Failed).await;
        let s = sw(&f).await.unwrap();
        assert!(s.error.as_deref().unwrap().contains("not found"), "{:?}", s.error);
        assert_eq!(console_json(&f).network.as_deref(), Some("regtest"), "persisted only after the spawn");
        assert_eq!(f.state.read().await.console.settings.network.as_deref(), Some("regtest"));
        assert_eq!(a.config().network.map(|n| n.id), Some("regtest"), "agent config is put back");
        assert_eq!(f.state.read().await.rpc_addr, f.rpc("regtest"));
        // A new console reading the same files wakes up on the old network.
        let again = Console::load(Some(f.dir.join("state")), EnvOverrides { mode: Some("managed".into()), ..Default::default() }, None);
        assert_eq!(again.network().map(|n| n.id), Some("regtest"));
        cleanup(&f);
    }

    #[tokio::test]
    async fn a_new_node_that_crashes_fails_the_switch_with_its_log_and_is_not_restarted() {
        let f = fixture("crash", Some("regtest"), "run", timings());
        let a = agent(&f).await;
        // Make the next node (signet) crash: the mode comes from the saved child_env.
        {
            let mut g = f.state.write().await;
            g.console.settings.child_env.insert("FAKE_MODE".into(), "crash".into());
            g.console.reresolve();
        }
        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "{v}");
        wait_phase(&f, SwitchPhase::Failed).await;
        let s = sw(&f).await.unwrap();
        assert!(s.error.as_deref().unwrap().contains("Exited with code 3"), "{:?}", s.error);
        assert!(!s.error.as_deref().unwrap().contains("Restarting"), "the automatic restart was cancelled: {:?}", s.error);
        assert!(s.log.iter().any(|l| l.contains("about to crash")), "{:?}", s.log);
        assert!(s.log.len() <= 50);
        // The node was spawned, so the console follows the new network; the card shows the crash.
        assert_eq!(console_json(&f).network.as_deref(), Some("signet"));
        assert_eq!(a.state(), ProcState::Crashed);
        let before = a.view().restarts;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(a.state(), ProcState::Crashed, "no restart race with 'Try again'");
        assert_eq!(a.view().restarts, before);
        assert!(a.view().last_error.unwrap().contains("Exited with code 3"));
        // Same network now, so the host refuses another switch to it; the page uses Start / Try again.
        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 400, "{v}");
        cleanup(&f);
    }

    #[tokio::test]
    async fn switching_during_a_restart_or_a_stop_does_not_bring_the_old_network_back() {
        let f = fixture("midflight", Some("regtest"), "slowstop", timings());
        let a = start_running(&f).await;
        wait_until("node output", || log_text(&a).contains("fake node up")).await;
        // A restart is queued behind the stop; the switch replaces it.
        a.restart().await.unwrap();
        assert_eq!(a.state(), ProcState::Stopping);
        let (code, v) = begin(&f.state, "signet").await;
        assert_eq!(code, 202, "{v}");
        assert_eq!(v["switch"]["phase"], "stopping");
        wait_phase(&f, SwitchPhase::Done).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(a.config().network.map(|n| n.id), Some("signet"));
        assert_eq!(a.view().network, Some("signet"));
        // The ring follows the current network's file; regtest's own file shows it ran exactly once.
        let reg_log = std::fs::read_to_string(networks::log_file(&f.dir.join("root"), networks::by_id("regtest").unwrap())).unwrap();
        assert_eq!(reg_log.matches("--network regtest").count(), 2, "one start: console line + node line: {reg_log}");
        assert!(log_text(&a).contains("--network signet"));
        a.stop_and_wait().await;
        cleanup(&f);

        // A switch while the node is still `starting` stops the half-started node and carries on.
        let f = fixture("starting", Some("regtest"), "slowstop", timings());
        let a = agent(&f).await;
        a.start().await.unwrap();
        assert_eq!(a.state(), ProcState::Starting);
        wait_until("node output", || log_text(&a).contains("fake node up")).await;
        let (code, v) = begin(&f.state, "testnet").await;
        assert_eq!(code, 202, "{v}");
        assert_eq!(v["switch"]["phase"], "stopping");
        wait_phase(&f, SwitchPhase::Starting).await;
        f.ready.store(true, Ordering::Relaxed);
        wait_phase(&f, SwitchPhase::Done).await;
        assert_eq!(a.view().network, Some("testnet"));
        a.stop_and_wait().await;
        cleanup(&f);

        // A crashed node waiting for its automatic restart is simply replaced.
        let f = fixture("crashedfirst", Some("regtest"), "crash", timings());
        let a = agent(&f).await;
        a.start().await.unwrap();
        wait_until("crashed", || a.state() == ProcState::Crashed).await;
        {
            let mut g = f.state.write().await;
            g.console.settings.child_env.insert("FAKE_MODE".into(), "run".into());
            g.console.reresolve();
        }
        f.ready.store(true, Ordering::Relaxed);
        let (code, _) = begin(&f.state, "signet").await;
        assert_eq!(code, 202);
        wait_phase(&f, SwitchPhase::Done).await;
        assert_eq!(a.view().network, Some("signet"));
        a.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn quitting_the_console_mid_switch_spawns_nothing() {
        let f = fixture("quit", Some("regtest"), "slowstop", timings());
        let a = start_running(&f).await;
        wait_until("node output", || log_text(&a).contains("fake node up")).await;
        let (code, _) = begin(&f.state, "signet").await;
        assert_eq!(code, 202);
        wait_phase(&f, SwitchPhase::Stopping).await;
        // Ctrl-C while the old node is still draining.
        a.shutdown(true).await;
        wait_phase(&f, SwitchPhase::Failed).await;
        assert!(sw(&f).await.unwrap().error.unwrap().contains("shutting down"));
        assert_eq!(a.state(), ProcState::Stopped);
        assert!(!f.dir.join("root/networks/signet").exists(), "no new node, no new folder");
        assert_eq!(console_json(&f).network.as_deref(), Some("regtest"));
        assert_eq!(a.config().network.map(|n| n.id), Some("regtest"));
        cleanup(&f);
    }

    #[tokio::test]
    async fn a_new_console_after_a_finished_switch_adopts_the_new_networks_node() {
        let f = fixture("restart", Some("regtest"), "run", timings());
        let a = agent(&f).await;
        f.ready.store(true, Ordering::Relaxed);
        let (code, _) = begin(&f.state, "signet").await;
        assert_eq!(code, 202);
        wait_phase(&f, SwitchPhase::Done).await;
        let pid = a.view().pid.unwrap();
        // The console dies (supervision ends, the node keeps running), a new one reads the same files.
        a.detach();
        let env = EnvOverrides {
            mode: Some("managed".into()),
            node_bin: Some(f.dir.join("fake-blvm.sh").display().to_string()),
            data_root: Some(f.dir.join("root").display().to_string()),
            ..Default::default()
        };
        let again = Console::load(Some(f.dir.join("state")), env, None);
        assert_eq!(again.network().map(|n| n.id), Some("signet"), "the saved network is the new one");
        assert!(again.switch.is_none(), "a switch lives in memory only");
        let a2 = again.agent.clone().unwrap();
        assert!(a2.adopt(), "the new network's node is adopted");
        assert_eq!(a2.view().pid, Some(pid));
        assert_eq!(a2.view().network, Some("signet"));
        a2.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn the_switch_follows_the_network_for_block_history() {
        let f = fixture("history", Some("regtest"), "run", timings());
        // Saved history for both networks' RPC addresses, as `feeds.json` would hold it.
        let mut saved = std::collections::BTreeMap::new();
        for (net, last) in [("regtest", 150u64), ("signet", 210_000u64)] {
            saved.insert(
                f.rpc(net),
                crate::persist::StoredView {
                    chain: Some(net.into()),
                    last: Some(last),
                    items: vec![crate::persist::StoredItem::from_item(&crate::feed::FeedItem::block(last))],
                    last_known: None,
                },
            );
        }
        f.state.write().await.restore(saved.clone());
        f.ready.store(true, Ordering::Relaxed);
        let a = agent(&f).await;
        let (code, _) = begin(&f.state, "signet").await;
        assert_eq!(code, 202);
        wait_phase(&f, SwitchPhase::Done).await;
        assert_eq!(f.state.read().await.rpc_addr, f.rpc("signet"));
        // Back again: the page's target is regtest's address, and both histories are exactly as saved.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let (code, v) = begin(&f.state, "regtest").await;
        assert_eq!(code, 202, "{v}");
        loop {
            if phase_of(&f).await == Some(SwitchPhase::Done) && f.state.read().await.rpc_addr == f.rpc("regtest") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(f.state.write().await.all_feeds(), saved, "history is keyed by RPC address and survives the round trip");
        // The regtest node restarted from its own folder (same data dir path in its arguments).
        wait_until("regtest output", || log_text(&a).matches("--network regtest").count() >= 2).await;
        a.stop_and_wait().await;
        cleanup(&f);
    }

    #[tokio::test]
    async fn the_meta_file_is_refreshed_while_the_node_runs() {
        let f = fixture("tick", Some("regtest"), "run", timings());
        let a = start_running(&f).await;
        let reg = networks::by_id("regtest").unwrap();
        let root = f.dir.join("root");
        wait_until("meta on start", || networks::read_meta(&root, reg).is_some()).await;
        let first = networks::read_meta(&root, reg).unwrap();
        assert!(first.created.is_some());
        // The poller reports a new height; the 100 ms tick (60 s in production) writes it.
        a.note_height(&f.rpc("regtest"), 77, 80);
        wait_until("height in meta", || networks::read_meta(&root, reg).is_some_and(|m| m.last_height == Some(77))).await;
        // A node that briefly says 0 does not erase it.
        a.note_height(&f.rpc("regtest"), 0, 0);
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(networks::read_meta(&root, reg).unwrap().last_height, Some(77));
        assert_eq!(networks::read_meta(&root, reg).unwrap().created, first.created, "created never changes");
        a.stop_and_wait().await;
        cleanup(&f);
    }
}
