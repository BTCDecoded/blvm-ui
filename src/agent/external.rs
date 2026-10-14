//! The console's side of an agent that lives in another container (`BLVM_UI_AGENT=external`).
//!
//! It spawns nothing. Everything goes through `<root>/control/` (see `control.rs`):
//!
//! * start / stop / restart write a new `desired.json` (generation + 1);
//! * the process view is `state.json`, with one correction: between the write and the moment the agent
//!   reports that it dealt with the new generation, the view shows what is about to happen (`starting` after a
//!   start, `stopping` after a stop or restart), so the page reacts at once and the network switch can wait on
//!   plain states;
//! * a node that has answered the console's own RPC poll is `running` even if `state.json` still says
//!   `starting`;
//! * an agent whose heartbeat is gone while a request waits for it is reported as a crash with a plain
//!   sentence, instead of `starting` forever; an agent that wrote `state.json` once and then went quiet is
//!   reported the same way when nothing is waiting (the page also reads `managed.agent.alive`).

use super::*;
use crate::control::{self, Desired, StateFile};

/// `state.json` is read at most this often.
const STATE_CACHE: Duration = Duration::from_millis(200);
/// A request nobody answered for this long, with no heartbeat, means the agent is not there.
const PENDING_GRACE_SECS: u64 = 15;
/// How often the console refreshes `meta.json` (last height) for a node it does not run.
const META_EVERY: Duration = Duration::from_secs(60);

/// Refusal of a change that needs a stopped node (a wipe, another network, folder or program) while the agent is
/// quiet: nobody can say whether a node runs, and a request written now would be carried out when the agent returns.
pub const NOT_ANSWERING: &str = "The node agent is not answering; try again when it is back.";

pub(super) const NO_AGENT: &str =
    "The node agent is not responding. Check that the node container is running, then try again.";

pub(super) struct Ext {
    st: Mutex<ExtSt>,
}

struct ExtSt {
    /// The data root `desired` and `cache` belong to.
    root: Option<PathBuf>,
    desired: Desired,
    /// Generation of a restart request the agent has not dealt with yet.
    restart_gen: Option<u64>,
    /// Generation of the last write that asked for something to happen (start, stop, restart), as opposed to
    /// one that only carried saved settings. Once the agent has dealt with it, a stopped node stays stopped.
    request_gen: u64,
    cache: Option<(Instant, Option<StateFile>)>,
    last_meta: Option<Instant>,
}

impl Ext {
    pub(super) fn new() -> Self {
        Self {
            st: Mutex::new(ExtSt {
                root: None,
                desired: Desired::default(),
                restart_gen: None,
                request_gen: 0,
                cache: None,
                last_meta: None,
            }),
        }
    }

    /// Reload `desired` when the data root changed (or on first use): the console may restart on top of an
    /// existing file, and the generation must continue from it.
    fn sync(s: &mut ExtSt, root: &Path) {
        if s.root.as_deref() != Some(root) {
            s.desired = control::read_desired(root).unwrap_or_default();
            // Not known after a restart of the console: any generation may have been a request.
            s.request_gen = s.desired.generation;
            s.root = Some(root.to_path_buf());
            s.cache = None;
            s.restart_gen = None;
        }
    }

    fn state_file(s: &mut ExtSt, root: &Path) -> Option<StateFile> {
        if let Some((at, f)) = &s.cache {
            if at.elapsed() < STATE_CACHE {
                return f.clone();
            }
        }
        let f = control::read_state(root);
        s.cache = Some((Instant::now(), f.clone()));
        f
    }

    pub(super) fn alive(&self, inner: &Arc<Inner>) -> bool {
        let root = inner.cfg.lock().unwrap().root.clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        Self::state_file(&mut s, &root).is_some_and(|f| f.alive(Inner::now()))
    }

    pub(super) fn view(&self, inner: &Arc<Inner>) -> ProcessView {
        let root = inner.cfg.lock().unwrap().root.clone();
        let now = Inner::now();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        let file = Self::state_file(&mut s, &root);
        let alive = file.as_ref().is_some_and(|f| f.alive(now));
        let applied = file.as_ref().map_or(0, |f| f.generation);
        let mut v = match &file {
            Some(f) => ProcessView {
                state: f.proc_state(),
                pid: f.pid,
                since: f.since,
                exit_code: f.exit_code,
                exit_signal: f.exit_signal,
                restarts: f.restarts,
                last_error: f.last_error.clone(),
                conflict: f.conflict,
                notice: None,
                crash_log: f.crash_log.clone(),
                adopted: f.adopted,
                network: f.network.as_deref().and_then(networks::by_id).map(|n| n.id),
            },
            None => ProcessView {
                state: ProcState::Stopped,
                pid: None,
                since: None,
                exit_code: None,
                exit_signal: None,
                restarts: 0,
                last_error: None,
                conflict: false,
                notice: None,
                crash_log: Vec::new(),
                adopted: false,
                network: None,
            },
        };
        let d = &s.desired;
        let active = v.state.is_active();
        if d.generation > applied {
            // The agent has not dealt with our last request yet.
            if !alive && now.saturating_sub(d.updated) > PENDING_GRACE_SECS {
                v.state = ProcState::Crashed;
                v.pid = None;
                v.since = Some(d.updated);
                v.last_error = Some(NO_AGENT.into());
            } else if d.running && !active {
                v.state = ProcState::Starting;
                v.pid = None;
                v.since = Some(d.updated);
                v.exit_code = None;
                v.exit_signal = None;
                v.last_error = None;
                v.conflict = false;
                v.crash_log.clear();
                v.network = d.network.as_deref().and_then(networks::by_id).map(|n| n.id).or(v.network);
            } else if active && (!d.running || s.restart_gen == Some(d.generation)) {
                v.state = ProcState::Stopping;
                v.since = Some(d.updated);
            }
        } else if file.is_some() && !alive {
            // Nothing is pending, but the agent that wrote this file has gone quiet (its container stopped or
            // keeps crashing): the last state it wrote is no longer true, whatever it says.
            v.state = ProcState::Crashed;
            v.pid = None;
            v.last_error = Some(NO_AGENT.into());
        } else if v.state == ProcState::Starting && inner.rpc_ok.load(Ordering::Relaxed) {
            // The console's own poll got an answer from the node before the agent's next heartbeat.
            v.state = ProcState::Running;
        }
        v
    }

    pub(super) fn binary_found(&self, inner: &Arc<Inner>) -> Option<bool> {
        let root = inner.cfg.lock().unwrap().root.clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        Self::state_file(&mut s, &root).map(|f| f.binary_found)
    }

    /// The platform facts (page size) the agent wrote, if it is a version that writes them.
    pub(super) fn platform_facts(&self, inner: &Arc<Inner>) -> Option<crate::sysinfo::Facts> {
        let root = inner.cfg.lock().unwrap().root.clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        Self::state_file(&mut s, &root).and_then(|f| f.platform)
    }

    pub(super) fn installed(&self, inner: &Arc<Inner>) -> Option<Installed> {
        let root = inner.cfg.lock().unwrap().root.clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        let f = Self::state_file(&mut s, &root)?;
        let version = f.installed_version.clone()?;
        Some(Installed {
            path: PathBuf::from(f.binary.clone().unwrap_or_default()),
            mtime: None,
            self_report: f.installed_report.clone().or_else(|| Some(format!("blvm {version}"))),
            version: Some(version),
            error: None,
        })
    }

    /// Replace `desired.json`. `restart` also bumps `restart_id`: the agent restarts a running node. `start` bumps
    /// `start_id`: the agent starts a node that is not running.
    fn write(&self, inner: &Arc<Inner>, running: bool, restart: bool, start: bool) -> Result<(), String> {
        let cfg = inner.cfg.lock().unwrap().clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &cfg.root);
        Self::write_locked(&mut s, inner, &cfg, running, restart, start, true)
    }

    /// [`Self::write`] with the lock already held. `request` is false for a write that only carries saved settings.
    fn write_locked(
        s: &mut ExtSt,
        inner: &Arc<Inner>,
        cfg: &AgentConfig,
        running: bool,
        restart: bool,
        start: bool,
        request: bool,
    ) -> Result<(), String> {
        // A start or restart is checked against the wipe hold here, under the same lock `confirm_stopped` reads
        // `desired.running` with: a start that passed the check in `Agent::start` just before the hold went on
        // either wrote `running: true` first (the wipe then says "stop the node first") or is refused now.
        if request && running && inner.hold_starts.load(Ordering::SeqCst) {
            return Err(super::WIPING_MSG.into());
        }
        let now = Inner::now();
        let prev = &s.desired;
        let d = Desired {
            generation: prev.generation + 1,
            network: cfg.network.map(|n| n.id.to_string()).or_else(|| prev.network.clone()),
            running,
            binary: match &cfg.bin {
                Some(p) => p.strip_prefix(&cfg.root).unwrap_or(p).display().to_string(),
                None => control::DEFAULT_BINARY.into(),
            },
            env: cfg.child_env.clone(),
            assume_valid: cfg.assume_valid.clone(),
            prune_keep_blocks: cfg.prune_keep_blocks,
            stop_grace_secs: inner.stop_wait().as_secs().max(1),
            rpc_port: cfg.rpc().as_deref().and_then(port_of),
            restart_id: prev.restart_id + u64::from(restart),
            start_id: prev.start_id + u64::from(start),
            updated: now,
        };
        control::write_desired(&cfg.root, &d).map_err(|e| {
            format!("Cannot write {}: {e}", control::dir(&cfg.root).join(control::DESIRED_FILE).display())
        })?;
        s.restart_gen = restart.then_some(d.generation);
        if request {
            s.request_gen = d.generation;
        }
        s.desired = d;
        s.cache = None;
        Ok(())
    }

    /// A setting changed (assume-valid, pruning, environment ...): put the resolved config into `desired.json`
    /// without a `restart_id` or `start_id` bump and with `running` as it is. The running node is left alone (the
    /// agent only restarts on a new `restart_id` or network), a node that is not running stays as it is (the agent
    /// starts one only for a Start, see `Desired::start_id`), but a start the agent makes by itself later, after
    /// a crash or a restart of its container, uses what was saved. Nothing is written before the first request,
    /// or when the file already says the same. `running` is read and written under one lock: a Stop in between
    /// cannot be overwritten.
    pub(super) fn sync_desired(&self, inner: &Arc<Inner>) {
        let cfg = inner.cfg.lock().unwrap().clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &cfg.root);
        let d = &s.desired;
        let network = cfg.network.map(|n| n.id.to_string()).or_else(|| d.network.clone());
        let binary = match &cfg.bin {
            Some(p) => p.strip_prefix(&cfg.root).unwrap_or(p).display().to_string(),
            None => control::DEFAULT_BINARY.into(),
        };
        let same = d.network == network
            && d.binary == binary
            && d.env == cfg.child_env
            && d.assume_valid == cfg.assume_valid
            && d.prune_keep_blocks == cfg.prune_keep_blocks
            && d.rpc_port == cfg.rpc().as_deref().and_then(port_of);
        if d.generation == 0 || same {
            return;
        }
        let running = d.running;
        if let Err(e) = Self::write_locked(&mut s, inner, &cfg, running, false, false, false) {
            tracing::warn!("node agent (external): {e}");
        }
    }

    /// The agent has gone quiet while something could still be running or waiting for it: it wrote
    /// `state.json` once and the heartbeat is stale, or the console asked for something and no agent ever
    /// answered. A change that needs a stopped node must wait (the console cannot see the node, and what it
    /// wrote would be carried out when the agent returns).
    pub(super) fn quiet(&self, inner: &Arc<Inner>) -> bool {
        let root = inner.cfg.lock().unwrap().root.clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        s.cache = None;
        let generation = s.desired.generation;
        match Self::state_file(&mut s, &root) {
            Some(f) => !f.alive(Inner::now()),
            None => generation > 0,
        }
    }

    fn desired_running(&self, inner: &Arc<Inner>) -> bool {
        let root = inner.cfg.lock().unwrap().root.clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        s.desired.running
    }

    pub(super) fn start(&self, inner: &Arc<Inner>) -> Result<(), String> {
        match self.view(inner).state {
            ProcState::Running | ProcState::Starting => return Ok(()),
            ProcState::Stopping => return Err("The node is still stopping. Try again in a moment.".into()),
            ProcState::Stopped | ProcState::Crashed => {}
        }
        if inner.cfg.lock().unwrap().network.is_none() {
            return Err("Choose a network first.".into());
        }
        // A reading from before this start must not count as "the new node answers".
        inner.rpc_ok.store(false, Ordering::Relaxed);
        *inner.height.lock().unwrap() = None;
        self.write(inner, true, false, true)?;
        inner.ensure_tailer();
        Ok(())
    }

    pub(super) fn request_stop(&self, inner: &Arc<Inner>) {
        let state = self.view(inner).state;
        if !state.is_active() && state != ProcState::Crashed && !self.desired_running(inner) {
            return; // already off, and nobody asked for it to be on
        }
        if let Err(e) = self.write(inner, false, false, false) {
            tracing::warn!("node agent (external): {e}");
        }
    }

    pub(super) fn restart(&self, inner: &Arc<Inner>) -> Result<(), String> {
        match self.view(inner).state {
            ProcState::Starting | ProcState::Running | ProcState::Stopping => {
                inner.rpc_ok.store(false, Ordering::Relaxed);
                self.write(inner, true, true, false)
            }
            ProcState::Stopped | ProcState::Crashed => self.start(inner),
        }
    }

    /// A crashed node waiting for the agent's automatic restart: ask for it to stay off. Only a live agent can
    /// have a restart pending: for one that went quiet "crashed" is the console's own conclusion, the node may
    /// well be running, and a request to stay off would stop it the moment the agent returns.
    pub(super) fn abandon_restart(&self, inner: &Arc<Inner>) {
        if self.alive(inner) && self.view(inner).state == ProcState::Crashed {
            if let Err(e) = self.write(inner, false, false, false) {
                tracing::warn!("node agent (external): {e}");
            }
        }
    }

    /// May the chain data be deleted? The console cannot see the node process, so the agent has to say so:
    /// it must be alive (a fresh heartbeat), `state.json` must belong to the last request written to
    /// `desired.json` (the agent dealt with that generation), and the node must not be starting, running or
    /// stopping. A crashed node that the agent has already dealt with has no process either.
    pub(super) fn confirm_stopped(&self, inner: &Arc<Inner>) -> Result<(), String> {
        let root = inner.cfg.lock().unwrap().root.clone();
        let mut s = self.st.lock().unwrap();
        Self::sync(&mut s, &root);
        s.cache = None; // read the file now, not a copy from 200 ms ago
        let Some(f) = Self::state_file(&mut s, &root) else {
            return Err(NOT_ANSWERING.into());
        };
        if !f.alive(Inner::now()) {
            return Err(NOT_ANSWERING.into());
        }
        // `desired.json` must not ask for the node either: a node that exited by itself while the file still says
        // "run" would be started again when the agent's container restarts.
        if s.desired.running || f.proc_state().is_active() {
            return Err(super::STOP_FIRST_MSG.into());
        }
        // The agent must have dealt with the last request. A save after that (settings only, no start or stop)
        // does not matter: it cannot start a node (`Desired::start_id`), so a stopped node is still stopped
        // while the agent has not yet read it.
        if f.generation < s.request_gen {
            return Err(super::STOP_FIRST_MSG.into());
        }
        Ok(())
    }

    /// Refresh `meta.json` (the agent, which cannot see the chain height, leaves `last_height` alone).
    pub(super) fn touch_meta(&self, inner: &Arc<Inner>, network: &'static str) {
        {
            let mut s = self.st.lock().unwrap();
            if s.last_meta.is_some_and(|t| t.elapsed() < META_EVERY) {
                return;
            }
            s.last_meta = Some(Instant::now());
        }
        inner.write_meta(network);
    }
}
