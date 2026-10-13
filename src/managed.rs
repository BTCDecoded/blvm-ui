//! Managed-node glue: the console's settings and mode, the node agent behind them, the extra
//! `/api/status` fields, and the logic of `/api/settings`, `/api/setup`, `/api/setup/test` and the
//! managed branch of `/api/node` (managed-node design A, B, F, K, M).
//!
//! `http.rs` only parses requests and calls into here; everything below works on [`Shared`] so it can
//! be tested without a socket.

use crate::agent::{Agent, AgentConfig, LogLine, ProcState, ProcessView, Timings};
use crate::control;
use crate::installer;
use crate::networks::{self, Network};
use crate::releases;
use crate::settings::{self, ConsoleSettings, EnvOverrides, Effective, Mode};
use crate::state::{stamp_power, LiveState, Shared, Snapshot, VersionSeen};
use crate::switch::{SwitchState, SwitchView};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where the web host looks for the node's RPC when the agent runs in another container and
/// `BLVM_UI_NODE_HOST` is not set (the Umbrel app's agent container).
pub const DEFAULT_NODE_HOST: &str = "btc-commons-node-view_agent_1";

/// How often the per-network folder sizes are measured (a `du` over a synced chain takes a moment).
const DISK_REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
pub struct DiskCache {
    at: Option<Instant>,
    root: Option<PathBuf>,
    busy: bool,
    nets: BTreeMap<&'static str, NetDisk>,
}

/// What is on disk for one network: size, whether any chain data exists, and `meta.json`.
#[derive(Debug, Clone, Default)]
pub struct NetDisk {
    pub bytes: u64,
    pub has_data: bool,
    pub meta: Option<networks::Meta>,
}

impl DiskCache {
    pub fn measured(&self) -> bool {
        self.at.is_some()
    }

    pub fn net(&self, id: &str) -> NetDisk {
        self.nets.get(id).cloned().unwrap_or_default()
    }
}

/// Everything that belongs to the console's mode: persisted settings, environment, the agent.
pub struct Console {
    pub settings: ConsoleSettings,
    pub env: EnvOverrides,
    pub eff: Effective,
    /// Where `console.json` lives. `None`: nothing is persisted (tests, the inert default).
    pub state_dir: Option<PathBuf>,
    pub exe_dir: Option<PathBuf>,
    /// What the state folder held at startup (design K: existing installs never get the wizard).
    pub facts: settings::StateDirFacts,
    pub agent: Option<Agent>,
    /// Timings for the agent this console creates (tests shorten them).
    pub timings: Timings,
    /// The network switch in progress, or the one that just finished (shown for 30 s).
    pub switch: Option<SwitchState>,
    /// The install / update job (design D); `idle` when none has run.
    pub job: installer::JobCell,
    /// Official releases (GitHub), cached.
    pub releases: Arc<releases::Cache>,
    pub installer_timings: installer::Timings,
    /// Pretend to be this platform (`linux-x86_64` ...) instead of `releases::platform()`. Tests.
    pub platform_override: Option<String>,
    /// Look for a newer release in the background (set by the real process, never by tests).
    pub auto_check: bool,
    /// Hand the node's RPC login to the process-wide RPC client (set by the real process, never by tests:
    /// the client is shared by everything in one process).
    pub publish_rpc_login: bool,
    /// The generated RPC login exists only in memory so far (it is written with the next save, and in any case
    /// before a node is started with it: see [`Console::persist_login`]).
    login_unsaved: std::sync::atomic::AtomicBool,
    disk: Arc<Mutex<DiskCache>>,
    last_pid: Option<u32>,
}

impl Console {
    /// Standalone, nothing persisted, wizard not wanted. What `LiveState::new` starts with.
    pub fn inert() -> Self {
        let settings = ConsoleSettings::default();
        let env = EnvOverrides::default();
        let mut eff = settings::resolve(&settings, &env, None);
        eff.setup_done = true;
        Self {
            settings,
            env,
            eff,
            state_dir: None,
            exe_dir: None,
            facts: settings::StateDirFacts::default(),
            agent: None,
            timings: Timings::default(),
            switch: None,
            job: installer::new_cell(),
            releases: releases::Cache::new(releases::Source::default()),
            installer_timings: installer::Timings::default(),
            platform_override: None,
            auto_check: false,
            publish_rpc_login: false,
            login_unsaved: std::sync::atomic::AtomicBool::new(false),
            disk: Arc::new(Mutex::new(DiskCache::default())),
            last_pid: None,
        }
    }

    /// Read `console.json`, apply the environment and the bundle marker.
    pub fn load(state_dir: Option<PathBuf>, env: EnvOverrides, exe_dir: Option<PathBuf>) -> Self {
        let settings = state_dir.as_deref().map(settings::load_console).unwrap_or_default();
        let facts = settings::state_dir_facts(state_dir.as_deref());
        let eff = settings::resolve_in(&settings, &env, exe_dir.as_deref(), facts);
        let mut c = Self {
            settings,
            env,
            eff,
            state_dir,
            exe_dir,
            facts,
            agent: None,
            timings: Timings::default(),
            switch: None,
            job: installer::new_cell(),
            releases: releases::Cache::new(releases::Source::default()),
            installer_timings: installer::Timings::default(),
            platform_override: None,
            auto_check: false,
            publish_rpc_login: false,
            login_unsaved: std::sync::atomic::AtomicBool::new(false),
            disk: Arc::new(Mutex::new(DiskCache::default())),
            last_pid: None,
        };
        c.releases.set_dir(c.state_dir.clone());
        c.reresolve();
        c
    }

    pub fn from_process() -> Self {
        let exe_dir = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf));
        let env = EnvOverrides::from_process();
        let dir = crate::persist::choose_state_dir(&env);
        let mut c = Self::load(dir, env, exe_dir);
        c.auto_check = c.is_managed() && c.updates_channel() == "self";
        c.publish_rpc_login = true;
        c.publish_login();
        c
    }

    /// The login of the node's RPC, when this process starts the node: `BLVM_UI_RPC_USER` / `BLVM_UI_RPC_PASS`,
    /// else the pair generated on first use. `None` for a node in another container (its agent has its own:
    /// the page's login then comes from the environment alone) and outside managed mode.
    pub fn rpc_auth(&self) -> Option<crate::agent::RpcAuth> {
        if !self.is_managed() || self.is_external() {
            return None;
        }
        let pick = |u: &Option<String>, p: &Option<String>| match (u, p) {
            (Some(user), Some(pass)) if !user.is_empty() && !pass.is_empty() => {
                Some(crate::agent::RpcAuth { user: user.clone(), pass: pass.clone() })
            }
            _ => None,
        };
        pick(&self.env.rpc_user, &self.env.rpc_pass).or_else(|| pick(&self.settings.rpc_user, &self.settings.rpc_pass))
    }

    /// Make sure a node started by this process has a login: generate one the first time and keep it.
    fn ensure_rpc_login(&mut self) {
        if !self.is_managed() || self.is_external() || self.rpc_auth().is_some() {
            return;
        }
        self.settings.rpc_user = Some("commons".into());
        self.settings.rpc_pass = Some(settings::random_hex(24));
        self.login_unsaved.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Write the generated login to `console.json` if that has not happened yet. Called before every start
    /// of a node: the node is given this password, so it must still exist after a restart of the console.
    pub fn persist_login(&self) {
        if self.login_unsaved.load(std::sync::atomic::Ordering::Relaxed) {
            if let Err(e) = self.save() {
                tracing::warn!("could not save the generated RPC login: {e}");
            }
        }
    }

    /// Tell the RPC client which login to send.
    fn publish_login(&self) {
        if self.publish_rpc_login {
            crate::rpc::set_login(self.rpc_auth().map(|a| (a.user, a.pass)));
        }
    }

    /// `self` | `store` | `off`: `BLVM_UI_UPDATES` wins over the settings file.
    pub fn updates_channel(&self) -> String {
        self.env.updates.clone().unwrap_or_else(|| self.settings.updates.clone())
    }

    pub fn platform(&self) -> String {
        self.platform_override.clone().unwrap_or_else(releases::platform)
    }

    /// The node runs under the agent in another container.
    pub fn is_external(&self) -> bool {
        self.is_managed() && self.env.agent_external
    }

    /// The installer's record, when the program in use is the one it installed.
    pub fn installed_record(&self) -> Option<releases::InstalledRecord> {
        let bin = self.eff.node_bin.as_deref()?;
        if bin != settings::installed_bin(&self.eff.data_root) {
            return None;
        }
        releases::read_installed(&self.eff.data_root)
    }

    /// An install or update is working.
    pub fn job_active(&self) -> bool {
        self.job.lock().unwrap().active()
    }

    pub fn is_managed(&self) -> bool {
        self.eff.mode == Mode::Managed
    }

    pub fn network(&self) -> Option<&'static Network> {
        self.settings.network.as_deref().and_then(networks::by_id)
    }

    /// Where the console polls this network: the override, else the default port on this machine.
    pub fn rpc_for(&self, net: &Network) -> String {
        self.settings
            .rpc_overrides
            .get(net.id)
            .filter(|a| valid_rpc_addr(a))
            .cloned()
            .unwrap_or_else(|| {
                if !self.is_external() {
                    return networks::default_rpc(net);
                }
                // The node is in another container: its RPC is on that container's address.
                let host = self.env.node_host.as_deref().filter(|h| valid_rpc_addr(&format!("{h}:1")));
                format!("{}:{}", host.unwrap_or(DEFAULT_NODE_HOST), net.rpc_port)
            })
    }

    /// The address the poller should use right now. Empty means "poll nobody": a managed console with
    /// no network chosen has nothing of its own to watch, and must not wander to a default port that
    /// belongs to somebody else's node.
    pub fn initial_rpc(&self) -> String {
        if self.is_managed() {
            return self.network().map(|n| self.rpc_for(n)).unwrap_or_default();
        }
        self.env
            .rpc
            .clone()
            .or_else(|| self.settings.rpc.clone())
            .unwrap_or_else(|| "127.0.0.1:48332".into())
    }

    pub fn agent_config(&self) -> AgentConfig {
        self.agent_config_for(self.network())
    }

    /// The agent's settings for running `net` (assume-valid and environment rules are applied per
    /// network when the node starts, from this and the network table).
    pub fn agent_config_for(&self, net: Option<&'static Network>) -> AgentConfig {
        AgentConfig {
            bin: self.eff.node_bin.clone(),
            root: self.eff.data_root.clone(),
            network: net,
            rpc_addr: net.map(|n| self.rpc_for(n)),
            assume_valid: self.settings.assume_valid.clone(),
            child_env: self.settings.child_env.clone(),
            // A node this process starts gets a login too (the agent of another container has its own).
            rpc_auth: self.rpc_auth(),
            p2p_port: None,
        }
    }

    /// Recompute the effective settings and bring the agent in line (create, update or drop it).
    pub fn reresolve(&mut self) {
        self.eff = settings::resolve_in(&self.settings, &self.env, self.exe_dir.as_deref(), self.facts);
        self.ensure_rpc_login();
        self.publish_login();
        if self.eff.mode == Mode::Managed {
            let cfg = self.agent_config();
            match &self.agent {
                Some(a) => a.set_config(cfg),
                None if self.env.agent_external => self.agent = Some(Agent::external(cfg, self.timings.clone())),
                None => self.agent = Some(Agent::new(cfg, self.timings.clone())),
            }
        } else if let Some(a) = self.agent.take() {
            a.detach();
        }
        self.invalidate_disk();
    }

    pub fn save(&self) -> std::io::Result<()> {
        let r = match &self.state_dir {
            Some(d) => settings::save_console(d, &self.settings),
            None => Ok(()),
        };
        if r.is_ok() {
            self.login_unsaved.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        r
    }

    /// A switch is running (stopping the old node or starting the new one).
    pub fn switch_active(&self) -> bool {
        self.switch.as_ref().is_some_and(SwitchState::active)
    }

    /// The cached folder facts for the network table, and for the switch preview.
    pub fn disk_snapshot(&self) -> Arc<Mutex<DiskCache>> {
        Arc::clone(&self.disk)
    }

    pub fn invalidate_disk(&self) {
        self.disk.lock().unwrap().at = None;
    }

    /// A size measurement to run on a blocking thread, when one is due.
    pub fn disk_job(&self) -> Option<(Arc<Mutex<DiskCache>>, PathBuf)> {
        self.agent.as_ref()?;
        let root = self.eff.data_root.clone();
        let mut d = self.disk.lock().unwrap();
        let due = !d.busy && (d.root.as_ref() != Some(&root) || d.at.is_none_or(|t| t.elapsed() > DISK_REFRESH));
        if due {
            d.busy = true;
            Some((Arc::clone(&self.disk), root))
        } else {
            None
        }
    }

    pub fn bundle_version(&self) -> Option<String> {
        self.env.node_version.clone().or_else(|| {
            (self.eff.node_bin_source == "bundle")
                .then(|| self.eff.bundle.as_ref().and_then(|b| b.version.clone()))
                .flatten()
        })
    }
}

/// Measure every network folder (allocated bytes, has chain data, `meta.json`) and store the result.
pub fn run_disk_job(cache: Arc<Mutex<DiskCache>>, root: PathBuf) {
    let mut nets = BTreeMap::new();
    for n in networks::ALL.iter() {
        let dir = networks::dir(&root, n);
        let bytes = if dir.is_dir() { crate::node_ctl::dir_allocated_bytes(&dir) } else { 0 };
        nets.insert(
            n.id,
            NetDisk { bytes, has_data: chain_has_data(&dir), meta: networks::read_meta(&root, n) },
        );
    }
    let mut d = cache.lock().unwrap();
    d.nets = nets;
    d.root = Some(root);
    d.at = Some(Instant::now());
    d.busy = false;
}

/// Anything in `<dir>/data` besides the node's module folder: the node's own cwd-relative
/// `data/modules/...` appears on the first start, before a single block is stored.
fn chain_has_data(dir: &Path) -> bool {
    std::fs::read_dir(dir.join("data"))
        .is_ok_and(|rd| rd.flatten().any(|e| e.file_name() != "modules"))
}

// ---- status ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct NetRow {
    pub id: &'static str,
    pub label: &'static str,
    pub rpc: String,
    pub bytes: u64,
    /// From `networks/<id>/meta.json` (the live height for the running network).
    pub last_height: Option<u64>,
    pub last_used: Option<u64>,
    pub has_data: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateView {
    /// A newer official release than the installed one exists (never for a custom build).
    pub available: bool,
    /// Newest release tag known from the last check (`v0.1.60`).
    pub latest: Option<String>,
    pub channel: String,
    /// `idle | download | verify | test | install | restart | done | failed`.
    pub phase: &'static str,
    pub error: Option<String>,
    pub tag: Option<String>,
    pub bytes: u64,
    pub total: u64,
    pub rolled_back: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledView {
    pub label: String,
    pub tag: Option<String>,
    pub self_report: Option<String>,
    pub sha256: Option<String>,
    pub source: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentView {
    pub external: bool,
    pub alive: bool,
}

/// `managed` in `/api/status` (design F). Extras beyond F: `installed_version`, `crash_log`, and
/// `process.exit_signal` / `process.conflict`. Paths: only the data root (the log file is named by
/// `/api/node/logs`, the node binary by nothing: the status is readable by every page on this host).
#[derive(Debug, Clone, Serialize)]
pub struct ManagedView {
    pub process: ProcessView,
    /// `None` until a network is chosen (there is no default).
    pub network: Option<&'static str>,
    pub data_root: String,
    pub free_bytes: u64,
    pub networks: Vec<NetRow>,
    /// The one user-facing version of the binary on disk (design M.3), the same text on every card:
    /// the bundle's release tag (`v0.1.60`), else the binary's own report (`blvm 0.1.0`, with
    /// `custom_build`). `None` until it has been read. The RPC subversion is `blvm_version` at the
    /// top of the status and is only a secondary "node reports ..." line.
    pub node_version: Option<String>,
    pub custom_build: bool,
    /// `X.Y.Z` from `<bin> version` (design M.2).
    pub installed_version: Option<String>,
    pub installed: Option<InstalledView>,
    pub update: UpdateView,
    pub switch: Option<SwitchView>,
    pub agent: AgentView,
    pub crash_log: Vec<String>,
}

fn managed_view(c: &Console, view: ProcessView, agent: &Agent, live_height: Option<u64>) -> ManagedView {
    let installed = agent.installed();
    // The one user-facing version (design M.3): the release tag the installer recorded for the program in
    // use, else the bundle's tag, else the program's own report as a custom build.
    let record = c.installed_record();
    let bundle_version = c.bundle_version().map(|v| if v.starts_with('v') { v } else { format!("v{v}") });
    let self_version = installed.as_ref().and_then(|i| i.version.clone());
    let release_tag = record.as_ref().map(|r| r.tag.clone());
    let (node_version, custom_build, source) = match (&release_tag, &bundle_version, &self_version) {
        (Some(t), _, _) => (Some(t.clone()), false, "release"),
        (None, Some(b), _) => (Some(b.clone()), false, "bundle"),
        (None, None, Some(s)) => (Some(format!("blvm {s}")), true, "custom"),
        (None, None, None) => (None, false, "custom"),
    };
    let tag_shown = release_tag.clone().or_else(|| bundle_version.clone());
    let self_report = installed.as_ref().and_then(|i| i.self_report.clone());
    let installed_view = (self_report.is_some() || record.is_some()).then(|| InstalledView {
        label: match (&tag_shown, &self_report) {
            (Some(b), _) => b.clone(),
            (None, Some(r)) => format!("Custom build ({r})"),
            (None, None) => "Custom build".into(),
        },
        tag: tag_shown.clone(),
        self_report: self_report.clone(),
        sha256: record.as_ref().map(|r| r.sha256.clone()),
        source,
    });
    let job = c.job.lock().unwrap().shown(crate::persist::unix_now());
    let latest_tag = c.releases.peek_latest();
    let update_available = match (&record, &latest_tag) {
        (Some(r), Some(l)) => releases::version_gt(l, &r.tag),
        _ => false,
    };
    let disk = c.disk.lock().unwrap();
    let networks_rows = networks::ALL
        .iter()
        .map(|n| {
            let d = disk.net(n.id);
            let meta = d.meta.unwrap_or_default();
            let mut row = NetRow {
                id: n.id,
                label: n.label,
                rpc: c.rpc_for(n),
                bytes: d.bytes,
                last_height: meta.last_height,
                last_used: meta.last_used,
                has_data: d.has_data,
            };
            // The network the node is on right now: what the poll just read beats the file.
            if c.network().is_some_and(|cur| cur.id == n.id) {
                if let Some(h) = live_height {
                    row.last_height = Some(h);
                }
                if view.state.is_active() {
                    row.last_used = Some(crate::persist::unix_now());
                }
            }
            row
        })
        .collect();
    drop(disk);
    ManagedView {
        crash_log: view.crash_log.clone(),
        process: view,
        network: c.network().map(|n| n.id),
        data_root: c.eff.data_root.display().to_string(),
        free_bytes: crate::state::volume_space_at(&c.eff.data_root).1,
        networks: networks_rows,
        node_version,
        custom_build,
        installed_version: self_version,
        installed: installed_view,
        update: UpdateView {
            available: update_available,
            latest: latest_tag,
            channel: c.updates_channel(),
            phase: job.phase,
            error: job.error.clone(),
            tag: job.tag.clone(),
            bytes: job.bytes,
            total: job.total,
            rolled_back: job.rolled_back,
        },
        switch: c.switch.as_ref().map(SwitchState::view),
        agent: AgentView { external: agent.is_external(), alive: agent.alive() },
    }
}

fn stamp_console_fields(snap: &mut Snapshot, c: &Console) {
    snap.mode = c.eff.mode.as_str();
    snap.setup_done = c.eff.setup_done;
    // A node program this console can run: on disk here, or (agent in another container) one the agent says
    // it has, for instance the one baked into its image.
    snap.bundle_available = c.eff.node_bin.as_deref().is_some_and(|p| p.is_file())
        || (c.is_external() && c.agent.as_ref().is_some_and(|a| a.binary_found() == Some(true)));
    snap.modules_pages = vec!["mining".to_string()];
}

/// Fill the console-level fields of the snapshot and, in managed mode, let the agent's state decide
/// what the node looks like while its RPC is not answering. Called from `stamp_power`, i.e. after
/// every poll and every power action.
pub fn stamp(g: &mut LiveState) {
    let c = &mut g.console;
    let snap = &mut g.snapshot;
    stamp_console_fields(snap, c);
    let Some(agent) = c.agent.clone() else {
        snap.managed = None;
        return;
    };
    if c.switch.as_ref().is_some_and(SwitchState::expired) {
        c.switch = None;
    }
    let view = agent.view();

    // Somebody else's node answers on the port our node was supposed to have (design B, B3 of the U0/U1
    // review): whatever the poller read is that program's, not ours. Show nothing of it.
    if view.conflict {
        let (ui_uptime, ui_version, rpc) = (snap.ui_uptime.clone(), snap.ui_version.clone(), snap.rpc_addr.clone());
        *snap = Snapshot { ui_uptime, ui_version, ..Snapshot::disconnected(&rpc) };
        stamp_console_fields(snap, c);
    }

    // A new process invalidates what the old one said about itself (design M.1).
    if view.pid != c.last_pid {
        c.last_pid = view.pid;
        if snap.blvm_version != "—" {
            snap.blvm_version_seen = Some(VersionSeen {
                version: snap.blvm_version.clone(),
                at: crate::persist::unix_now(),
            });
            snap.blvm_version = "—".into();
        }
    }

    // The process ended a moment ago: the last poll's "connected" is a memory, not a fact.
    let just_ended = matches!(view.state, ProcState::Stopped | ProcState::Crashed)
        && view.since.is_some_and(|t| crate::persist::unix_now().saturating_sub(t) < 3);
    if just_ended {
        snap.connected = false;
        snap.frozen = false;
    }

    // A node that was just spawned has not answered yet: a parked snapshot of this network (restored
    // by a network switch) that says "connected" is a memory, not a fact. The poll that reaches the
    // new node sets the flag before it builds its snapshot, so a live answer is never hidden.
    if view.state == ProcState::Starting && !agent.rpc_flag().load(std::sync::atomic::Ordering::Relaxed) {
        snap.connected = false;
        snap.frozen = false;
    }

    // Design M.1, everywhere: a version is shown only next to a live answer. A failed start or an exit
    // forces `connected = false` above without a pid change to notice, so park it here.
    if !snap.connected && snap.blvm_version != "—" {
        snap.blvm_version_seen = Some(VersionSeen {
            version: snap.blvm_version.clone(),
            at: snap.last_seen.unwrap_or_else(crate::persist::unix_now),
        });
        snap.blvm_version = "—".into();
    }

    snap.show_manual = false;
    snap.node_busy = matches!(view.state, ProcState::Starting | ProcState::Stopping);
    snap.node_running = view.state.is_active() || snap.connected || snap.frozen;
    snap.node_power_label = match view.state {
        ProcState::Starting => "Starting…".into(),
        ProcState::Stopping => "Stopping…".into(),
        ProcState::Running => "Turn Node Off".into(),
        ProcState::Stopped | ProcState::Crashed => {
            if snap.node_running {
                "Turn Node Off".into()
            } else {
                "Turn Node On".into()
            }
        }
    };
    if let Some(net) = c.network() {
        snap.rpc_addr = c.rpc_for(net);
        if !snap.connected {
            snap.network = net.chain.to_string();
        }
    } else {
        snap.rpc_addr = "—".into();
    }
    if !snap.connected {
        let (health, label, status): (&'static str, &str, &str) = match view.state {
            ProcState::Starting => ("heal", "Starting", "Starting"),
            ProcState::Stopping => ("heal", "Stopping", "Stopping"),
            ProcState::Stopped => ("dead", "Node stopped", "Down"),
            ProcState::Crashed => ("dead", "Node crashed", "Down"),
            ProcState::Running => ("heal", "Looking", "Looking"),
        };
        if !snap.frozen {
            snap.health = health;
            snap.health_label = label.to_string();
            snap.node_status = status.to_string();
        }
        if matches!(view.state, ProcState::Stopped | ProcState::Crashed) {
            snap.node_running = false;
            snap.frozen = false;
            snap.node_power_label = "Turn Node On".into();
        }
        if view.state == ProcState::Crashed {
            snap.error = view.last_error.clone();
        }
    }
    // A switch owns the node while it runs: nothing else should poke it, and the page says why.
    if c.switch_active() {
        snap.node_busy = true;
        snap.node_power_label = "Switching…".into();
    }
    let live_height = (snap.connected && snap.local_height > 0).then_some(snap.local_height);
    snap.managed = Some(managed_view(c, view, &agent, live_height));
}

// ---- boot and housekeeping ------------------------------------------------------------------

/// Run once after the poller exists: adopt a node that outlived the previous console, else start
/// one when the user asked for it (`start_on_launch`) and did not leave it stopped (`node_wanted`).
///
/// The user's last Start or Stop is what comes back after any restart. With the agent in another container
/// that agent restores its own `desired.json` by itself, so the console then writes nothing at all; it only
/// asks for a first start when there is no such file yet.
pub async fn boot(state: &Shared) {
    let (agent, start, root) = {
        let g = state.read().await;
        let c = &g.console;
        // Set up once, started by itself ever after: a restart of the app or the computer repeats nothing.
        (
            c.agent.clone(),
            c.settings.start_on_boot_effective(true)
                && c.settings.node_wanted_effective()
                && c.eff.setup_done
                && c.network().is_some(),
            c.eff.data_root.clone(),
        )
    };
    let Some(agent) = agent else { return };
    agent.refresh_installed(false).await;
    // With the agent in another container its answer takes a heartbeat to appear; give it a moment before
    // deciding that nothing is running.
    if agent.is_external() {
        for _ in 0..10 {
            if agent.alive() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
    let binary_ok = match agent.binary_found() {
        Some(found) => found,
        None => state.read().await.console.eff.node_bin.as_deref().is_some_and(Path::is_file),
    };
    if agent.adopt() {
        tracing::info!("managed node: re-adopted the running node");
    } else if agent.is_external() && control::read_desired(&root).is_some() {
        tracing::info!("managed node: the node agent restores what was asked last, nothing to write");
    } else if start && binary_ok {
        state.read().await.console.persist_login();
        match agent.start().await {
            Ok(()) => tracing::info!("managed node: started (start_on_boot)"),
            Err(e) => tracing::warn!("managed node: could not start: {e}"),
        }
    } else if start {
        tracing::info!("managed node: no node program yet, not starting");
    }
    let mut g = state.write().await;
    stamp_power(&mut g);
}

/// Cheap periodic work for the 1 s tick: re-read the node's version when the binary changed, and
/// re-measure the network folders every minute (on a blocking thread).
pub async fn housekeeping(state: &Shared) {
    let (agent, job) = {
        let g = state.read().await;
        (g.console.agent.clone(), g.console.disk_job())
    };
    if let Some(a) = agent {
        if a.installed_is_stale() {
            tokio::spawn(async move { a.refresh_installed(false).await });
        }
    }
    if let Some((cache, root)) = job {
        tokio::task::spawn_blocking(move || run_disk_job(cache, root));
    }
    // Look for a newer release now and then (the cache keeps it to one request per ten minutes).
    let check = {
        let g = state.read().await;
        let c = &g.console;
        (c.auto_check && c.is_managed() && c.updates_channel() == "self" && c.releases.needs_check()).then(|| c.releases.clone())
    };
    if let Some(cache) = check {
        tokio::spawn(async move {
            cache.get(false).await;
        });
    }
}

/// Console is quitting: stop the node (default) or leave it running.
pub async fn shutdown(state: &Shared) {
    let (agent, stop) = {
        let g = state.read().await;
        (g.console.agent.clone(), g.console.settings.stop_node_on_exit)
    };
    if let Some(a) = agent {
        if stop && a.state().is_active() {
            tracing::info!("managed node: stopping it before the console quits (up to 200 s)");
        }
        a.shutdown(stop).await;
    }
}

// ---- validation helpers ---------------------------------------------------------------------

/// `host:port` with a plausible host (name, IPv4, or `[IPv6]`) and a port in 1..=65535.
pub fn valid_rpc_addr(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 255 {
        return false;
    }
    let Some((host, port)) = s.rsplit_once(':') else {
        return false;
    };
    if !port.parse::<u16>().is_ok_and(|p| p > 0) {
        return false;
    }
    if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return v6.parse::<std::net::Ipv6Addr>().is_ok();
    }
    !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

fn valid_data_root(s: &str) -> Result<PathBuf, String> {
    let s = s.trim();
    if s.is_empty() || s.len() > 4000 || s.contains('\0') {
        return Err("data_root must be a folder path.".into());
    }
    let p = if let Some(rest) = s.strip_prefix("~/") {
        match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(rest),
            None => return Err("Cannot expand ~ here; use a full path.".into()),
        }
    } else {
        PathBuf::from(s)
    };
    if !p.is_absolute() {
        return Err("data_root must be a full path (starting with / or ~/).".into());
    }
    if p.parent().is_none() {
        return Err("data_root cannot be the root of the disk.".into());
    }
    Ok(p)
}

fn valid_node_bin(s: &str) -> Result<PathBuf, String> {
    let s = s.trim();
    if s.is_empty() || s.contains('\0') {
        return Err("node_bin must be a file path.".into());
    }
    let p = PathBuf::from(s);
    if !p.is_absolute() {
        return Err("node_bin must be a full path.".into());
    }
    if !p.is_file() {
        return Err(format!("No file at {}.", p.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&p).map(|m| m.permissions().mode() & 0o111 == 0).unwrap_or(true) {
            return Err(format!("{} is not executable.", p.display()));
        }
    }
    Ok(p)
}

fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 64
        && k.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !k.as_bytes()[0].is_ascii_digit()
        && !k.starts_with("LD_")
        && !k.starts_with("DYLD_")
        && k != "PATH"
}

fn err(code: u16, msg: impl Into<String>) -> (u16, Value) {
    let m = msg.into();
    (code, json!({ "ok": false, "error": m }))
}

fn bool_field(body: &Value, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|k| body.get(*k).and_then(Value::as_bool))
}

// ---- GET/PUT /api/settings ------------------------------------------------------------------

/// The settings as the page sees them: effective values, plus which ones the environment pins.
pub fn settings_json(c: &Console) -> Value {
    let s = &c.settings;
    json!({
        "version": s.version,
        "setup_done": c.eff.setup_done,
        "mode": c.eff.mode.as_str(),
        "rpc": c.env.rpc.clone().or_else(|| s.rpc.clone()),
        "network": s.network,
        "data_root": c.eff.data_root.display().to_string(),
        "node_bin": c.eff.node_bin.as_ref().map(|p| p.display().to_string()),
        "start_on_boot": s.start_on_boot_effective(c.is_managed()),
        "start_on_launch": s.start_on_boot_effective(c.is_managed()),
        "modules_selected": s.modules_selected,
        "stop_node_on_exit": s.stop_node_on_exit,
        "keep_node_running_on_exit": !s.stop_node_on_exit,
        "modules_disabled": s.modules_disabled,
        "updates": c.updates_channel(),
        "rpc_overrides": s.rpc_overrides,
        "assume_valid": s.assume_valid,
        "child_env": s.child_env,
        "env_locked": c.eff.env_locked,
        "sources": {
            "mode": c.eff.mode_source,
            "node_bin": c.eff.node_bin_source,
            "data_root": c.eff.data_root_source,
        },
    })
}

/// Refusal for a mode change when the environment fixes the mode (the Umbrel app always runs its own node).
const MODE_LOCKED: &str = "This app always runs its own node, so it cannot be switched to watching another one. (The mode is fixed by BLVM_UI_MODE.)";
const JOB_RUNNING: &str = "The node is being installed or updated. Wait for that to finish.";

fn node_is_active(c: &Console) -> bool {
    c.switch_active() || c.agent.as_ref().is_some_and(|a| a.state().is_active())
}

/// Apply a patch to `console.json`. Returns `(http status, body)`.
pub async fn patch_settings(state: &Shared, body: &Value) -> (u16, Value) {
    let Some(obj) = body.as_object() else {
        return err(400, "Send a JSON object.");
    };
    let mut g = state.write().await;
    let active = node_is_active(&g.console);
    let mut s = g.console.settings.clone();
    let before_rpc = g.console.initial_rpc();
    let mut needs_stopped = false;

    for (k, v) in obj {
        match k.as_str() {
            "mode" => {
                let Some(m) = v.as_str().and_then(Mode::parse) else {
                    return err(400, "mode must be \"standalone\" or \"managed\".");
                };
                if g.console.eff.env_locked.contains(&"mode") && m != g.console.eff.mode {
                    return err(409, MODE_LOCKED);
                }
                if Some(m.as_str()) != s.mode.as_deref() {
                    s.mode = Some(m.as_str().into());
                    needs_stopped |= m != g.console.eff.mode;
                }
            }
            "rpc" => {
                if v.is_null() {
                    s.rpc = None;
                } else if let Some(a) = v.as_str().filter(|a| valid_rpc_addr(a)) {
                    s.rpc = Some(a.trim().to_string());
                } else {
                    return err(400, "rpc must look like host:port.");
                }
            }
            "network" => {
                if v.is_null() {
                    s.network = None;
                } else if let Some(n) = v.as_str().and_then(networks::by_id) {
                    s.network = Some(n.id.into());
                } else {
                    return err(400, "Unknown network.");
                }
                needs_stopped |= s.network != g.console.settings.network;
            }
            "data_root" => {
                if v.is_null() {
                    s.data_root = None;
                } else {
                    match v.as_str().map(valid_data_root) {
                        Some(Ok(p)) => {
                            if g.console.env.data_root.is_some() && p != g.console.eff.data_root {
                                return err(409, "The data folder is set by the BLVM_UI_DATA_ROOT environment variable.");
                            }
                            s.data_root = Some(p.display().to_string());
                        }
                        Some(Err(e)) => return err(400, e),
                        None => return err(400, "data_root must be a string."),
                    }
                }
                needs_stopped |= s.data_root != g.console.settings.data_root;
            }
            "node_bin" => {
                if v.is_null() {
                    s.node_bin = None;
                } else {
                    match v.as_str().map(valid_node_bin) {
                        Some(Ok(p)) => {
                            if g.console.env.node_bin.is_some() && Some(&p) != g.console.eff.node_bin.as_ref() {
                                return err(409, "The node binary is set by the BLVM_UI_NODE_BIN environment variable.");
                            }
                            s.node_bin = Some(p.display().to_string());
                        }
                        Some(Err(e)) => return err(400, e),
                        None => return err(400, "node_bin must be a string."),
                    }
                }
                needs_stopped |= s.node_bin != g.console.settings.node_bin;
            }
            "start_on_boot" | "start_on_launch" => match v.as_bool() {
                Some(b) => s.start_on_boot = Some(b),
                None => return err(400, format!("{k} must be true or false.")),
            },
            "stop_node_on_exit" => match v.as_bool() {
                Some(b) => s.stop_node_on_exit = b,
                None => return err(400, "stop_node_on_exit must be true or false."),
            },
            "keep_node_running_on_exit" => match v.as_bool() {
                Some(b) => s.stop_node_on_exit = !b,
                None => return err(400, "keep_node_running_on_exit must be true or false."),
            },
            "setup_done" => match v.as_bool() {
                Some(b) => s.setup_done = b,
                None => return err(400, "setup_done must be true or false."),
            },
            "modules_disabled" => {
                let Some(list) = v.as_array().filter(|a| a.len() <= 64) else {
                    return err(400, "modules_disabled must be a list of module names.");
                };
                let mut names = Vec::new();
                for n in list {
                    match n.as_str().filter(|n| !n.is_empty() && n.len() <= 80) {
                        Some(n) => names.push(n.to_string()),
                        None => return err(400, "modules_disabled must be a list of module names."),
                    }
                }
                s.modules_disabled = names;
            }
            "updates" => match v.as_str() {
                Some(u @ ("self" | "store" | "off")) => {
                    if g.console.env.updates.as_deref().is_some_and(|e| e != u) {
                        return err(409, "The update setting is set by the BLVM_UI_UPDATES environment variable.");
                    }
                    s.updates = u.into();
                }
                _ => return err(400, "updates must be \"self\", \"store\" or \"off\"."),
            },
            "rpc_overrides" => {
                let Some(map) = v.as_object() else {
                    return err(400, "rpc_overrides must map network ids to host:port.");
                };
                let mut out = BTreeMap::new();
                for (id, a) in map {
                    match (networks::by_id(id), a.as_str().filter(|a| valid_rpc_addr(a))) {
                        (Some(n), Some(a)) => {
                            out.insert(n.id.to_string(), a.trim().to_string());
                        }
                        _ => return err(400, "rpc_overrides must map network ids to host:port."),
                    }
                }
                needs_stopped |= out != g.console.settings.rpc_overrides;
                s.rpc_overrides = out;
            }
            "assume_valid" => {
                if v.is_null() {
                    s.assume_valid = None;
                } else if let Some(a) = v.as_str().filter(|a| crate::agent::valid_assume_valid(a)) {
                    s.assume_valid = Some(a.trim().to_string());
                } else {
                    return err(400, "assume_valid must be \"default\", \"off\", a block height or a block hash.");
                }
            }
            "child_env" => {
                let Some(map) = v.as_object().filter(|m| m.len() <= 64) else {
                    return err(400, "child_env must map variable names to values.");
                };
                let mut out = BTreeMap::new();
                for (name, val) in map {
                    match val.as_str().filter(|x| x.len() <= 4096 && !x.contains('\0')) {
                        Some(x) if valid_env_name(name) => {
                            out.insert(name.clone(), x.to_string());
                        }
                        _ => return err(400, format!("child_env: {name} is not allowed.")),
                    }
                }
                s.child_env = out;
            }
            // Read-only or unknown: ignored, so the page can send back what it read.
            _ => {}
        }
    }
    if needs_stopped && active {
        return err(409, "Stop the node first, then change this setting.");
    }
    if needs_stopped && g.console.job_active() {
        return err(409, JOB_RUNNING);
    }
    // A crashed node waiting for its automatic restart counts as idle above, but it must not come back
    // on the new network, folder or binary: the change cancels the pending restart (S1 of the review).
    if needs_stopped {
        if let Some(a) = &g.console.agent {
            a.abandon_restart();
        }
    }

    g.console.settings = s;
    if let Err(e) = g.console.save() {
        return err(500, format!("Could not save settings: {e}"));
    }
    g.console.reresolve();
    let after_rpc = g.console.initial_rpc();
    if after_rpc != before_rpc {
        g.switch_rpc(after_rpc);
    }
    stamp_power(&mut g);
    (200, settings_json(&g.console))
}

// ---- POST /api/setup ------------------------------------------------------------------------

/// The wizard's result. Validates, writes `console.json`, creates the folders, and starts the node
/// only when asked to. A network is required for managed mode; there is no default.
pub async fn setup(state: &Shared, body: &Value) -> (u16, Value) {
    let Some(obj) = body.as_object() else {
        return err(400, "Send a JSON object.");
    };
    let Some(mode) = obj.get("mode").and_then(Value::as_str).and_then(Mode::parse) else {
        return err(400, "mode must be \"managed\" or \"standalone\".");
    };
    let start = bool_field(body, &["start", "start_now"]).unwrap_or(false);
    let stop_on_exit = match bool_field(body, &["stop_on_exit", "stop_node_on_exit"]) {
        Some(b) => Some(b),
        None => bool_field(body, &["keep_node_running_on_exit"]).map(|keep| !keep),
    };

    let mut g = state.write().await;
    if g.console.eff.env_locked.contains(&"mode") && mode != g.console.eff.mode {
        return err(409, MODE_LOCKED);
    }
    if node_is_active(&g.console) {
        return err(409, "Stop the node before running setup again.");
    }
    if g.console.job_active() {
        return err(409, JOB_RUNNING);
    }
    let before_rpc = g.console.initial_rpc();
    let mut s = g.console.settings.clone();
    s.mode = Some(mode.as_str().into());
    s.setup_done = true;
    if let Some(b) = stop_on_exit {
        s.stop_node_on_exit = b;
    }

    match mode {
        Mode::Standalone => {
            match obj.get("rpc") {
                None | Some(Value::Null) => {}
                Some(v) => match v.as_str().filter(|a| valid_rpc_addr(a)) {
                    Some(a) => s.rpc = Some(a.trim().to_string()),
                    None => return err(400, "rpc must look like host:port."),
                },
            }
        }
        Mode::Managed => {
            let net = match obj.get("network") {
                None | Some(Value::Null) => return err(400, "Pick a network first."),
                Some(v) => match v.as_str().map(str::trim).filter(|n| !n.is_empty()) {
                    None => return err(400, "Pick a network first."),
                    Some(n) => match networks::by_id(n) {
                        Some(net) => net,
                        None => return err(400, format!("Unknown network \"{n}\".")),
                    },
                },
            };
            s.network = Some(net.id.into());
            if let Some(v) = obj.get("data_root").filter(|v| !v.is_null()) {
                match v.as_str().map(valid_data_root) {
                    Some(Ok(p)) => {
                        if g.console.env.data_root.is_some() && p != g.console.eff.data_root {
                            return err(409, "The data folder is set by the BLVM_UI_DATA_ROOT environment variable.");
                        }
                        s.data_root = Some(p.display().to_string());
                    }
                    Some(Err(e)) => return err(400, e),
                    None => return err(400, "data_root must be a string."),
                }
            }
            if let Some(v) = obj.get("node_bin").filter(|v| !v.is_null()) {
                match v.as_str().map(valid_node_bin) {
                    Some(Ok(p)) => {
                        if g.console.env.node_bin.is_some() && Some(&p) != g.console.eff.node_bin.as_ref() {
                            return err(409, "The node binary is set by the BLVM_UI_NODE_BIN environment variable.");
                        }
                        s.node_bin = Some(p.display().to_string());
                    }
                    Some(Err(e)) => return err(400, e),
                    None => return err(400, "node_bin must be a string."),
                }
            }
            if let Some(list) = obj.get("modules").and_then(Value::as_array) {
                s.modules_selected = list
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|n| !n.is_empty() && n.len() <= 80)
                    .take(64)
                    .map(str::to_string)
                    .collect();
            }
            // The wizard only offers "Run a node for me" when a binary is known; check anyway. With the
            // agent in another container the program may live only there (the agent says so).
            let probe = settings::resolve_in(&s, &g.console.env, g.console.exe_dir.as_deref(), g.console.facts);
            let agent_has_one = g.console.agent.as_ref().and_then(Agent::binary_found) == Some(true);
            // Choosing a network does not need a program; starting does. A setup that only records the choice
            // ("finish without starting") is fine without one: the program can be installed from Node later.
            match &probe.node_bin {
                _ if !start => {}
                Some(b) if b.is_file() => {}
                _ if agent_has_one => {}
                Some(b) => return err(400, format!("The node binary {} does not exist.", b.display())),
                None => {
                    return err(
                        400,
                        "There is no node program yet. Install one first (the setup downloads the official release), or set BLVM_UI_NODE_BIN.",
                    )
                }
            }
            let net_dir = networks::dir(&probe.data_root, net);
            if let Err(e) = std::fs::create_dir_all(net_dir.join("logs"))
                .and_then(|_| std::fs::create_dir_all(probe.data_root.join("control")))
            {
                return err(500, format!("Cannot create {}: {e}", net_dir.display()));
            }
        }
    }

    // The wizard decides what runs now: a pending automatic restart after a crash must not start a node
    // the user just said not to start (or on another network). Starting is done below, on request.
    if let Some(a) = &g.console.agent {
        a.abandon_restart();
    }
    g.console.settings = s;
    if let Err(e) = g.console.save() {
        return err(500, format!("Could not save settings: {e}"));
    }
    g.console.reresolve();
    let after_rpc = g.console.initial_rpc();
    if after_rpc != before_rpc {
        g.switch_rpc(after_rpc);
    }
    let agent = g.console.agent.clone();
    stamp_power(&mut g);
    drop(g);

    if mode == Mode::Managed {
        remember_wanted(state, start).await;
    }
    let mut start_error = None;
    if mode == Mode::Managed && start {
        if let Some(a) = agent {
            if let Err(e) = a.start().await {
                start_error = Some(e);
            }
        }
    }
    let mut g = state.write().await;
    stamp_power(&mut g);
    let status = serde_json::to_value(&g.snapshot).unwrap_or(Value::Null);
    match start_error {
        None => (200, json!({ "ok": true, "status": status })),
        Some(e) => (200, json!({ "ok": false, "error": e, "status": status })),
    }
}

// ---- POST /api/setup/test -------------------------------------------------------------------

pub async fn setup_test(body: &Value) -> Value {
    let Some(addr) = body.get("rpc").and_then(Value::as_str).map(str::trim).filter(|a| valid_rpc_addr(a)) else {
        return json!({ "ok": false, "error": "Enter the address as host:port, for example 127.0.0.1:8332." });
    };
    let user = body.get("user").and_then(Value::as_str).filter(|u| !u.is_empty());
    let pass = body.get("pass").and_then(Value::as_str).unwrap_or("");
    let creds = user.map(|u| (u, pass));
    match crate::rpc::probe_once(addr, creds).await {
        Ok(answers) => {
            let chain = answers.first().cloned().flatten();
            let Some(chain) = chain else {
                return json!({ "ok": false, "error": "The node answered, but not to getblockchaininfo." });
            };
            let height = chain.get("blocks").and_then(Value::as_u64);
            let name = chain.get("chain").and_then(Value::as_str);
            let version = answers
                .get(1)
                .cloned()
                .flatten()
                .and_then(|n| n.get("subversion").and_then(Value::as_str).map(crate::state::parse_blvm_version))
                .map(|v| v.trim_start_matches('v').to_string());
            json!({ "ok": true, "chain": name, "height": height, "version": version })
        }
        Err(e) => json!({ "ok": false, "error": e.to_string() }),
    }
}

// ---- managed branch of /api/node ------------------------------------------------------------

#[derive(Debug)]
pub struct NodeOutcome {
    pub ok: bool,
    pub message: String,
    pub error: Option<String>,
}

/// Remember whether the user wants the node running (see [`ConsoleSettings::node_wanted`]).
pub async fn remember_wanted(state: &Shared, wanted: bool) {
    let mut g = state.write().await;
    if g.console.settings.node_wanted == Some(wanted) {
        return;
    }
    g.console.settings.node_wanted = Some(wanted);
    if let Err(e) = g.console.save() {
        tracing::warn!("could not save console.json: {e}");
    }
}

/// `start | stop | restart | toggle` (and the legacy `on | off`). Returns at once; the process state
/// in the status shows `starting` / `stopping` until it settles.
pub async fn node_action(state: &Shared, action: &str) -> NodeOutcome {
    let (agent, has_network, switching) = {
        let g = state.read().await;
        (g.console.agent.clone(), g.console.network().is_some(), g.console.switch_active())
    };
    let fail = |m: &str| NodeOutcome { ok: false, message: String::new(), error: Some(m.to_string()) };
    let Some(agent) = agent else {
        return fail("This console is not managing a node.");
    };
    if switching {
        return fail("A network switch is running. Wait for it to finish.");
    }
    if state.read().await.console.job_active() {
        return fail("The node is being installed or updated. Wait for that to finish.");
    }
    let action = match action {
        "on" => "start",
        "off" => "stop",
        "toggle" => {
            if matches!(agent.state(), ProcState::Running | ProcState::Starting) {
                "stop"
            } else {
                "start"
            }
        }
        a => a,
    };
    if matches!(action, "start" | "restart") {
        state.read().await.console.persist_login();
    }
    let out = match action {
        "start" => {
            if !has_network {
                return fail("Choose a network first.");
            }
            match agent.state() {
                ProcState::Running | ProcState::Starting => NodeOutcome {
                    ok: true,
                    message: "The node is already running.".into(),
                    error: None,
                },
                ProcState::Stopping => return fail("The node is still stopping. Try again in a moment."),
                _ => match agent.start().await {
                    Ok(()) => NodeOutcome { ok: true, message: "Starting the node.".into(), error: None },
                    Err(e) => NodeOutcome { ok: false, message: String::new(), error: Some(e) },
                },
            }
        }
        "stop" => {
            if agent.state().is_active() {
                agent.request_stop();
                NodeOutcome {
                    ok: true,
                    message: "Stopping the node. Saving chain state first.".into(),
                    error: None,
                }
            } else {
                // Also cancels a pending automatic restart after a crash.
                agent.request_stop();
                NodeOutcome { ok: true, message: "The node is already off.".into(), error: None }
            }
        }
        "restart" => {
            if !has_network {
                return fail("Choose a network first.");
            }
            match agent.restart().await {
                Ok(()) => NodeOutcome { ok: true, message: "Restarting the node.".into(), error: None },
                Err(e) => NodeOutcome { ok: false, message: String::new(), error: Some(e) },
            }
        }
        other => return fail(&format!("Unknown action {other}.")),
    };
    // What the user asked for is what comes back after a restart of the app (a failed start was still asked for).
    match action {
        "start" | "restart" => remember_wanted(state, true).await,
        "stop" => remember_wanted(state, false).await,
        _ => {}
    }
    let mut g = state.write().await;
    g.console.invalidate_disk();
    stamp_power(&mut g);
    out
}

// ---- GET /api/node/releases -----------------------------------------------------------------

/// What GitHub says is newest, what is installed, and whether this console may install (design F).
pub async fn releases_json(state: &Shared, force: bool) -> Value {
    let (cache, channel, platform, record, pinned, managed, has_program) = {
        let g = state.read().await;
        let c = &g.console;
        // Is there a node program at all (here, or in the node container)? "Custom" is a program the installer
        // did not put there; with none at all nothing is custom.
        let has_program = c.eff.node_bin.as_deref().is_some_and(Path::is_file)
            || c.agent.as_ref().and_then(Agent::binary_found) == Some(true);
        (
            c.releases.clone(),
            c.updates_channel(),
            c.platform(),
            c.installed_record().filter(|_| releases::installed_bin_ok(&c.eff.data_root)),
            c.env.node_bin.is_some(),
            c.is_managed(),
            has_program,
        )
    };
    let fetched = cache.get(force).await;
    releases::info_json(&releases::InfoInputs {
        managed,
        channel: &channel,
        platform: &platform,
        installed: record.as_ref(),
        custom: record.is_none() && has_program,
        bin_pinned_by_env: pinned,
        fetched: &fetched,
    })
}

// ---- GET /api/node/logs ---------------------------------------------------------------------

pub async fn logs(state: &Shared, after: Option<u64>, limit: usize) -> Option<Value> {
    let agent = state.read().await.console.agent.clone()?;
    let lines: Vec<LogLine> = agent.logs(after, limit);
    let file = agent.config().log_path().map(|p| p.display().to_string());
    Some(json!({ "lines": lines, "file": file }))
}

// ---------------------------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::control;
    use crate::state::LiveState;
    use std::os::unix::fs::PermissionsExt;
    use tokio::sync::RwLock;

    const FAKE: &str = "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\ntrap 'exit 0' TERM\necho up\nwhile :; do sleep 0.1; done\n";

    struct Fix {
        dir: PathBuf,
        state: Shared,
    }

    fn fixture(tag: &str, env: EnvOverrides, with_bin: bool) -> Fix {
        let dir = std::env::temp_dir().join(format!("blvm-managed-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-blvm.sh");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = env;
        if with_bin && env.node_bin.is_none() {
            env.node_bin = Some(bin.display().to_string());
        }
        if env.data_root.is_none() {
            env.data_root = Some(dir.join("root").display().to_string());
        }
        let console = Console::load(Some(dir.join("state")), env, None);
        let mut live = LiveState::new(console.initial_rpc());
        live.console = console;
        Fix { dir, state: Arc::new(RwLock::new(live)) }
    }

    fn managed_env() -> EnvOverrides {
        EnvOverrides { mode: Some("managed".into()), ..Default::default() }
    }

    #[tokio::test]
    async fn a_managed_console_without_a_network_polls_nobody() {
        let f = fixture("nopoll", managed_env(), true);
        assert_eq!(f.state.read().await.rpc_addr, "", "no default port to wander to");
        crate::state::poll_once(&f.state).await; // returns at once, opens no connection
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let snap = f.state.read().await.snapshot.clone();
        assert!(!snap.connected);
        assert_eq!(snap.rpc_addr, "—");
        assert_eq!(snap.managed.as_ref().unwrap().network, None);
        // Once a network is chosen, polling follows it.
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        assert_eq!(f.state.read().await.rpc_addr, "127.0.0.1:18443");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn setup_rejects_a_missing_or_unknown_network() {
        let f = fixture("noneed", managed_env(), true);
        for body in [
            json!({"mode":"managed","start":true}),
            json!({"mode":"managed","network":null,"start":true}),
            json!({"mode":"managed","network":"","start":true}),
            json!({"mode":"managed","network":"   "}),
        ] {
            let (code, v) = setup(&f.state, &body).await;
            assert_eq!(code, 400, "{body}");
            assert_eq!(v["ok"], false);
            assert_eq!(v["error"], "Pick a network first.");
        }
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"main"})).await;
        assert_eq!(code, 400, "ids are console names, not RPC chain names");
        assert!(v["error"].as_str().unwrap().contains("Unknown network"));
        let (code, _) = setup(&f.state, &json!({"mode":"sideways","network":"regtest"})).await;
        assert_eq!(code, 400);
        let (code, _) = setup(&f.state, &json!("not an object")).await;
        assert_eq!(code, 400);

        // Nothing was written or started.
        assert!(!f.dir.join("state/console.json").exists());
        assert!(!f.dir.join("root").exists());
        let g = f.state.read().await;
        assert_eq!(g.console.agent.as_ref().unwrap().state(), ProcState::Stopped);
        assert!(g.console.settings.network.is_none());
        drop(g);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn managed_setup_without_a_binary_is_rejected() {
        let f = fixture("nobin", managed_env(), false);
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","start":true})).await;
        assert_eq!(code, 400);
        assert!(v["error"].as_str().unwrap().contains("no node program"), "{v}");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn setup_writes_settings_creates_folders_and_starts_only_when_asked() {
        let f = fixture("ok", managed_env(), true);
        let (code, v) = setup(
            &f.state,
            &json!({"mode":"managed","network":"regtest","start":false,"stop_on_exit":false,"modules":["blvm-zmq"]}),
        )
        .await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["status"]["managed"]["process"]["state"], "stopped");
        assert_eq!(v["status"]["managed"]["network"], "regtest");
        assert_eq!(v["status"]["setup_done"], true);
        assert_eq!(v["status"]["rpc_addr"], "127.0.0.1:18443");
        assert!(f.dir.join("root/networks/regtest/logs").is_dir());
        let saved = settings::load_console(&f.dir.join("state"));
        assert!(saved.setup_done);
        assert_eq!(saved.network.as_deref(), Some("regtest"));
        assert_eq!(saved.mode.as_deref(), Some("managed"));
        assert!(!saved.stop_node_on_exit);

        // start_now (alias) starts it.
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","start_now":true})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["status"]["managed"]["process"]["state"], "starting");
        let agent = f.state.read().await.console.agent.clone().unwrap();
        // Changing the network or running setup while it runs is refused.
        let (code, _) = setup(&f.state, &json!({"mode":"managed","network":"signet"})).await;
        assert_eq!(code, 409);
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn setup_standalone_validates_the_rpc_address() {
        let f = fixture("sa", EnvOverrides::default(), false);
        let (code, v) = setup(&f.state, &json!({"mode":"standalone","rpc":"nonsense"})).await;
        assert_eq!(code, 400, "{v}");
        let (code, v) = setup(&f.state, &json!({"mode":"standalone","rpc":"127.0.0.1:8332"})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["status"]["mode"], "standalone");
        assert_eq!(v["status"]["managed"], Value::Null);
        assert_eq!(v["status"]["rpc_addr"], "127.0.0.1:8332");
        assert_eq!(settings::load_console(&f.dir.join("state")).rpc.as_deref(), Some("127.0.0.1:8332"));
        // Skip: standalone with the current target.
        let (code, v) = setup(&f.state, &json!({"mode":"standalone"})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["status"]["setup_done"], true);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn the_environment_pins_the_mode() {
        let f = fixture("locked", managed_env(), true);
        let (code, v) = setup(&f.state, &json!({"mode":"standalone"})).await;
        assert_eq!(code, 409, "{v}");
        let (code, _) = patch_settings(&f.state, &json!({"mode":"standalone"})).await;
        assert_eq!(code, 409);
        let g = f.state.read().await;
        assert_eq!(settings_json(&g.console)["env_locked"], json!(["mode", "node_bin", "data_root"]));
        drop(g);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn settings_patch_persists_validates_and_guards_a_running_node() {
        let f = fixture("patch", managed_env(), true);
        let (code, v) = patch_settings(&f.state, &json!({"start_on_launch":true,"keep_node_running_on_exit":true,"updates":"store"})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["start_on_launch"], true);
        assert_eq!(v["stop_node_on_exit"], false);
        assert_eq!(v["updates"], "store");
        let saved = settings::load_console(&f.dir.join("state"));
        assert!(saved.start_on_boot == Some(true) && !saved.stop_node_on_exit);

        for bad in [
            json!({"updates":"sometimes"}),
            json!({"network":"narnia"}),
            json!({"rpc":"no-port"}),
            json!({"assume_valid":"maybe"}),
            json!({"child_env":{"PATH":"/evil"}}),
            json!({"child_env":{"LD_PRELOAD":"/evil.so"}}),
            json!({"child_env":{"lower":"x"}}),
            json!({"data_root":"relative/path"}),
            json!({"node_bin":"/definitely/not/here"}),
            json!({"start_on_launch":"yes"}),
        ] {
            let (code, v) = patch_settings(&f.state, &bad).await;
            assert_eq!(code, 400, "{bad} -> {v}");
        }
        // A rejected patch changed nothing.
        assert_eq!(settings::load_console(&f.dir.join("state")), saved);

        let (code, v) = patch_settings(&f.state, &json!({"network":"regtest","child_env":{"RUST_LOG":"info"},"assume_valid":"off"})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["network"], "regtest");

        // Unknown and read-only keys are ignored, so the page can echo what it read.
        let (code, _) = patch_settings(&f.state, &json!({"env_locked":[],"sources":{},"bogus":1,"start_on_launch":false})).await;
        assert_eq!(code, 200);

        // With a node running, network/data/mode changes are refused.
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.start().await.unwrap();
        let (code, v) = patch_settings(&f.state, &json!({"network":"signet"})).await;
        assert_eq!(code, 409, "{v}");
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200, "same value is not a change");
        let (code, _) = patch_settings(&f.state, &json!({"stop_node_on_exit":true})).await;
        assert_eq!(code, 200, "harmless settings stay editable");
        agent.stop_and_wait().await;
        let (code, v) = patch_settings(&f.state, &json!({"network":"signet"})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(f.state.read().await.rpc_addr, "127.0.0.1:38332", "polling follows the network");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn node_actions_need_a_network_and_report_state() {
        let f = fixture("act", managed_env(), true);
        let r = node_action(&f.state, "start").await;
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("Choose a network first."));
        let r = node_action(&f.state, "frobnicate").await;
        assert!(!r.ok);

        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        let r = node_action(&f.state, "start").await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(f.state.read().await.snapshot.managed.as_ref().unwrap().process.state, ProcState::Starting);
        let snap = f.state.read().await.snapshot.clone();
        assert_eq!(snap.node_power_label, "Starting…");
        assert!(snap.node_busy);
        assert_eq!(snap.health_label, "Starting");
        assert_eq!(snap.network, "regtest");

        let r = node_action(&f.state, "start").await;
        assert!(r.ok && r.message.contains("already"));
        let r = node_action(&f.state, "toggle").await; // running or starting -> stop
        assert!(r.ok);
        assert_eq!(f.state.read().await.snapshot.managed.as_ref().unwrap().process.state, ProcState::Stopping);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.stop_and_wait().await;
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let snap = f.state.read().await.snapshot.clone();
        assert_eq!(snap.health_label, "Node stopped");
        assert!(!snap.node_running);
        assert_eq!(snap.node_power_label, "Turn Node On");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn boot_starts_the_node_only_when_asked_and_only_with_a_network() {
        // start_on_launch without a network: nothing starts.
        let f = fixture("boot1", managed_env(), true);
        {
            let mut g = f.state.write().await;
            g.console.settings.start_on_boot = Some(true);
            g.console.reresolve();
        }
        boot(&f.state).await;
        let agent = f.state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent.state(), ProcState::Stopped);

        // With a network chosen it starts.
        {
            let mut g = f.state.write().await;
            g.console.settings.network = Some("regtest".into());
            g.console.reresolve();
        }
        boot(&f.state).await;
        assert_eq!(agent.state(), ProcState::Starting);
        agent.stop_and_wait().await;

        // Switched off, it does not. (On is the default in managed mode; see the next test.)
        let f2 = fixture("boot2", managed_env(), true);
        {
            let mut g = f2.state.write().await;
            g.console.settings.start_on_boot = Some(false);
            g.console.settings.network = Some("regtest".into());
            g.console.reresolve();
        }
        boot(&f2.state).await;
        let agent2 = f2.state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent2.state(), ProcState::Stopped);
        let _ = std::fs::remove_dir_all(&f.dir);
        let _ = std::fs::remove_dir_all(&f2.dir);
    }

    #[tokio::test]
    async fn status_carries_mode_setup_and_managed_fields() {
        // Standalone: managed is null, setup_done follows the rules, everything else stays.
        let f = fixture("st-sa", EnvOverrides::default(), false);
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let v = serde_json::to_value(&f.state.read().await.snapshot).unwrap();
        assert_eq!(v["mode"], "standalone");
        assert_eq!(v["setup_done"], true, "no node program to offer: nothing to set up beyond the RPC target");
        assert_eq!(v["managed"], Value::Null);
        assert_eq!(v["modules_pages"], json!(["mining"]));
        assert_eq!(v["bundle_available"], false);
        assert!(v.get("connected").is_some() && v.get("blvm_version").is_some(), "old fields still there");

        // Managed, stopped, no network yet.
        let f2 = fixture("st-m", managed_env(), true);
        {
            let mut g = f2.state.write().await;
            stamp_power(&mut g);
        }
        let v = serde_json::to_value(&f2.state.read().await.snapshot).unwrap();
        assert_eq!(v["mode"], "managed");
        assert_eq!(v["setup_done"], false);
        assert_eq!(v["bundle_available"], true);
        let m = &v["managed"];
        assert_eq!(m["process"]["state"], "stopped");
        assert_eq!(m["network"], Value::Null);
        assert_eq!(m["networks"].as_array().unwrap().len(), 5);
        assert_eq!(m["networks"][0]["id"], "mainnet");
        assert_eq!(m["switch"], Value::Null);
        assert_eq!(m["update"]["phase"], "idle");
        assert_eq!(m["agent"], json!({"external": false, "alive": true}));
        assert!(m["data_root"].as_str().unwrap().ends_with("root"));
        // Standalone, a node program beside the console, fresh state folder: the wizard is wanted ...
        let f3 = fixture("st-sa-bin", EnvOverrides::default(), true);
        {
            let mut g = f3.state.write().await;
            stamp_power(&mut g);
        }
        let v = serde_json::to_value(&f3.state.read().await.snapshot).unwrap();
        assert_eq!((v["mode"].as_str(), v["setup_done"].as_bool(), v["bundle_available"].as_bool()), (Some("standalone"), Some(false), Some(true)));
        let _ = std::fs::remove_dir_all(&f.dir);
        let _ = std::fs::remove_dir_all(&f2.dir);
        let _ = std::fs::remove_dir_all(&f3.dir);
    }

    #[tokio::test]
    async fn b1_a_pre_u1_state_dir_gets_no_wizard_through_the_status() {
        // Review B1: the state folder of a console from before the wizard (only `feeds.json`), no
        // environment, and a node program available. /api/status must say setup_done = true.
        let dir = std::env::temp_dir().join(format!("blvm-managed-b1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::write(dir.join("state").join(crate::persist::FILE), br#"{"version":1,"views":{}}"#).unwrap();
        let bin = dir.join("fake-blvm.sh");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let env = EnvOverrides { node_bin: Some(bin.display().to_string()), ..Default::default() };
        let console = Console::load(Some(dir.join("state")), env, None);
        let mut live = LiveState::new(console.initial_rpc());
        live.console = console;
        stamp_power(&mut live);
        let v = serde_json::to_value(&live.snapshot).unwrap();
        assert_eq!(v["mode"], "standalone");
        assert_eq!(v["setup_done"], true, "an existing standalone install must not get the wizard");
        assert_eq!(v["bundle_available"], true, "Settings still offers \"Use a managed node\"");
        assert!(!dir.join("state/console.json").exists(), "reading the state wrote nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn installed_version_comes_from_the_binary_not_the_rpc() {
        let f = fixture("ver", managed_env(), true);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.refresh_installed(false).await;
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let v = serde_json::to_value(&f.state.read().await.snapshot).unwrap();
        let m = &v["managed"];
        assert_eq!(m["installed_version"], "9.9.9");
        assert_eq!(m["node_version"], "blvm 9.9.9", "custom builds read as the binary's own report on every card");
        assert_eq!(m["custom_build"], true);
        assert_eq!(m["installed"]["self_report"], "blvm 9.9.9");
        assert_eq!(m["installed"]["label"], "Custom build (blvm 9.9.9)");
        assert_eq!(m["installed"]["source"], "custom");
        assert_eq!(v["blvm_version"], "—", "the RPC has not answered, so no version is claimed");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn a_bundle_version_labels_the_release() {
        let env = EnvOverrides { mode: Some("managed".into()), node_version: Some("0.1.60".into()), ..Default::default() };
        let f = fixture("bundle", env, true);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.refresh_installed(false).await;
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let v = serde_json::to_value(&f.state.read().await.snapshot).unwrap();
        assert_eq!(v["managed"]["node_version"], "v0.1.60");
        assert_eq!(v["managed"]["custom_build"], false);
        assert_eq!(v["managed"]["installed"]["source"], "bundle");
        assert_eq!(v["managed"]["installed"]["self_report"], "blvm 9.9.9");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn setup_test_checks_the_address_first_and_reports_refusal() {
        let v = setup_test(&json!({"rpc":"nonsense"})).await;
        assert_eq!(v["ok"], false);
        let v = setup_test(&json!({})).await;
        assert_eq!(v["ok"], false);
        // Nothing listens on port 1.
        let v = setup_test(&json!({"rpc":"127.0.0.1:1"})).await;
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("127.0.0.1:1"), "{v}");
    }

    #[tokio::test]
    async fn setup_test_reads_chain_height_and_version_from_a_node() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = s.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            // The login given in the test must arrive as Basic auth for "u:p".
            let authed = req.contains("Authorization: Basic dTpw");
            let body = if authed {
                r#"[{"jsonrpc":"2.0","id":0,"result":{"chain":"testnet4","blocks":52619}},{"jsonrpc":"2.0","id":1,"result":{"subversion":"/BitcoinCommons:0.1.13/"}}]"#
            } else {
                r#"[{"jsonrpc":"2.0","id":0,"error":{"code":-1,"message":"no"}}]"#
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).await.unwrap();
        });
        let v = setup_test(&json!({"rpc": addr.to_string(), "user":"u", "pass":"p"})).await;
        assert_eq!(v["ok"], true, "{v}");
        assert_eq!(v["chain"], "testnet4");
        assert_eq!(v["height"], 52619);
        assert_eq!(v["version"], "0.1.13");
    }

    #[test]
    fn rpc_address_validation() {
        for ok in ["127.0.0.1:8332", "localhost:48332", "my-node.local:1", "[::1]:8332", "host_name:65535"] {
            assert!(valid_rpc_addr(ok), "{ok}");
        }
        for bad in ["", "8332", "host", "host:", "host:0", "host:70000", ":8332", "http://h:1", "a b:1", "h:1/x", "[::1:8332"] {
            assert!(!valid_rpc_addr(bad), "{bad}");
        }
    }

    #[test]
    fn env_name_rules_block_loader_variables() {
        assert!(valid_env_name("RUST_LOG") && valid_env_name("BLVM_SCHNORR_PER_SIG"));
        for bad in ["", "path", "PATH", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "1ABC", "A-B", "A B"] {
            assert!(!valid_env_name(bad), "{bad}");
        }
    }

    // ---- review (U0/U1) tests ----

    const CRASHER: &str = "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\necho \"up in $(pwd)\"\nsleep 0.3\nexit 3\n";

    async fn crash_once_on_regtest(tag: &str) -> Fix {
        let f = fixture(tag, managed_env(), true);
        std::fs::write(f.dir.join("fake-blvm.sh"), CRASHER).unwrap();
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        assert!(node_action(&f.state, "start").await.ok);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        for _ in 0..200 {
            if agent.state() == ProcState::Crashed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(agent.state(), ProcState::Crashed);
        assert!(agent.view().last_error.unwrap_or_default().contains("Restarting"));
        f
    }

    #[tokio::test]
    async fn review_network_change_during_crash_backoff_does_not_start_the_new_network() {
        let f = crash_once_on_regtest("rv-bk-net").await;
        let (code, v) = patch_settings(&f.state, &json!({"network":"signet"})).await;
        // Either refuse the change (409) or cancel the pending restart. Wait past the 5 s backoff.
        tokio::time::sleep(Duration::from_millis(6500)).await;
        let started_signet = f.dir.join("root/networks/signet/logs/node.log").exists();
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
        assert!(
            code == 409 || !started_signet,
            "settings accepted network=signet ({code} {v}) while a crash restart was pending, and the automatic restart then started a node on signet"
        );
    }

    #[tokio::test]
    async fn review_setup_with_start_false_during_crash_backoff_does_not_start_a_node() {
        let f = crash_once_on_regtest("rv-bk-setup").await;
        let (code, _) = setup(&f.state, &json!({"mode":"managed","network":"signet","start":false})).await;
        tokio::time::sleep(Duration::from_millis(6500)).await;
        let agent = f.state.read().await.console.agent.clone().unwrap();
        let spawned_on_signet = f.dir.join("root/networks/signet/logs/node.log").exists();
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
        assert!(code == 409 || !spawned_on_signet, "setup said start=false ({code}) but the pending crash restart started the node anyway");
    }

    #[tokio::test]
    async fn review_version_is_dash_whenever_not_connected_even_after_a_failed_start() {
        let f = fixture("rv-ver", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        {
            // Something answers on 127.0.0.1:18443 (another node), so the last poll was live.
            let mut g = f.state.write().await;
            g.snapshot.connected = true;
            g.snapshot.blvm_version = "v0.1.13".into();
        }
        std::fs::remove_file(f.dir.join("fake-blvm.sh")).unwrap();
        let r = node_action(&f.state, "start").await;
        assert!(!r.ok);
        let snap = f.state.read().await.snapshot.clone();
        let _ = std::fs::remove_dir_all(&f.dir);
        assert!(!snap.connected, "the failed start forces connected=false");
        assert_eq!(snap.blvm_version, "—", "connected=false but blvm_version is still a value");
    }

    #[tokio::test]
    async fn review_setup_without_a_network_writes_nothing_and_starts_nothing() {
        let f = fixture("rv-nonet", managed_env(), true);
        for body in [
            json!({"mode":"managed","start":true,"stop_on_exit":true,"modules":[]}),
            json!({"mode":"managed","network":null,"start":true}),
            json!({"mode":"managed","network":"  ","start_now":true}),
            json!({"mode":"managed","network":7,"start":true}),
            json!({"mode":"managed","network":["regtest"],"start":true}),
        ] {
            let (code, v) = setup(&f.state, &body).await;
            assert_eq!(code, 400, "{body} -> {v}");
        }
        assert!(!f.dir.join("state").exists(), "no console.json");
        assert!(!f.dir.join("root").exists(), "no data folders");
        let g = f.state.read().await;
        let a = g.console.agent.clone().unwrap();
        assert_eq!(a.state(), ProcState::Stopped);
        assert_eq!(a.view().pid, None);
        assert!(!g.console.eff.setup_done);
        drop(g);
        // And boot with start_on_launch still starts nothing without a network.
        {
            let mut g = f.state.write().await;
            g.console.settings.start_on_boot = Some(true);
            g.console.settings.setup_done = true;
            g.console.reresolve();
        }
        boot(&f.state).await;
        assert_eq!(a.state(), ProcState::Stopped);
        assert!(!node_action(&f.state, "restart").await.ok);
        assert!(!node_action(&f.state, "toggle").await.ok);
        assert!(!node_action(&f.state, "on").await.ok);
        assert_eq!(a.state(), ProcState::Stopped);
        assert!(!f.dir.join("root/networks").exists());
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    // ---- fix pass for the U0/U1 review (S1, S2, S10, B3 in the status, nits) ----

    #[tokio::test]
    async fn s1_a_setting_that_needs_no_stop_leaves_the_pending_restart_alone() {
        let f = crash_once_on_regtest("s1-keep").await;
        let (code, _) = patch_settings(&f.state, &json!({"start_on_launch": true})).await;
        assert_eq!(code, 200);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        // The automatic restart still happens (the fake node crashes only once).
        for _ in 0..400 {
            if agent.state().is_active() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(agent.state().is_active(), "a harmless setting must not cancel the restart: {:?}", agent.view());
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn s1_cancelling_the_pending_restart_keeps_the_crash_visible() {
        let f = crash_once_on_regtest("s1-visible").await;
        let (code, _) = patch_settings(&f.state, &json!({"network":"signet"})).await;
        assert_eq!(code, 200);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        let v = agent.view();
        assert_eq!(v.state, ProcState::Crashed, "still shown as crashed, with its log");
        assert!(!v.last_error.clone().unwrap_or_default().contains("Restarting"), "no promise of a restart: {:?}", v.last_error);
        assert!(!v.crash_log.is_empty());
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn s10_one_version_per_card_the_installed_one_and_the_rpc_one_apart() {
        // Bundle release: node_version is the release tag whether the node runs or not; the RPC's own
        // subversion is only `blvm_version` (shown as "node reports ..."), live only.
        let env = EnvOverrides { mode: Some("managed".into()), node_version: Some("0.1.60".into()), ..Default::default() };
        let f = fixture("s10", env, true);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.refresh_installed(false).await;
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let stopped = serde_json::to_value(&f.state.read().await.snapshot).unwrap();
        assert_eq!(stopped["managed"]["node_version"], "v0.1.60");
        assert_eq!(stopped["managed"]["installed"]["label"], "v0.1.60");
        assert_eq!(stopped["blvm_version"], "—");

        // Custom build (no bundle version): the binary's own report, same text in node_version and label.
        let f2 = fixture("s10b", managed_env(), true);
        let agent2 = f2.state.read().await.console.agent.clone().unwrap();
        agent2.refresh_installed(false).await;
        {
            let mut g = f2.state.write().await;
            g.snapshot.connected = true;
            g.snapshot.blvm_version = "v0.1.13".into();
            // a live answer from a node on the default port, not from a process of ours
            stamp_power(&mut g);
            let v = serde_json::to_value(&g.snapshot).unwrap();
            assert_eq!(v["managed"]["node_version"], "blvm 9.9.9");
            assert_eq!(v["managed"]["custom_build"], true);
            assert_eq!(v["managed"]["installed"]["label"], "Custom build (blvm 9.9.9)");
            assert_eq!(v["managed"]["installed_version"], "9.9.9");
        }
        let _ = std::fs::remove_dir_all(&f.dir);
        let _ = std::fs::remove_dir_all(&f2.dir);
    }

    /// A program that is not our node holds the RPC port of regtest.
    async fn conflict_fixture(tag: &str) -> (Fix, std::net::TcpListener) {
        let f = fixture(tag, managed_env(), true);
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (code, v) = patch_settings(&f.state, &json!({"network":"regtest","rpc_overrides":{"regtest": format!("127.0.0.1:{port}")}})).await;
        assert_eq!(code, 200, "{v}");
        (f, l)
    }

    #[tokio::test]
    async fn b3_a_foreign_node_on_the_port_is_refused_and_its_data_never_shown_as_ours() {
        let (f, l) = conflict_fixture("b3-status").await;
        let port = l.local_addr().unwrap().port();
        {
            // The poller is talking to that other program and has read a height, peers and a version.
            let mut g = f.state.write().await;
            g.snapshot.connected = true;
            g.snapshot.local_height = 777;
            g.snapshot.peers = 8;
            g.snapshot.blvm_version = "v0.1.13".into();
            g.snapshot.network = "regtest".into();
        }
        let r = node_action(&f.state, "start").await;
        assert!(!r.ok);
        let e = r.error.unwrap();
        assert!(e.contains(&format!("port {port} is in use by pid {}", std::process::id())), "{e}");
        let g = f.state.read().await;
        let v = serde_json::to_value(&g.snapshot).unwrap();
        assert_eq!(v["managed"]["process"]["state"], "crashed");
        assert_eq!(v["managed"]["process"]["conflict"], true);
        assert_eq!(v["managed"]["process"]["pid"], Value::Null);
        assert_eq!(v["connected"], false, "the other program's answers are not ours");
        assert_eq!((v["local_height"].as_u64(), v["peers"].as_u64()), (Some(0), Some(0)));
        assert_eq!(v["blvm_version"], "—");
        assert_eq!(v["node_running"], false);
        assert_eq!(v["node_power_label"], "Turn Node On");
        assert_eq!(v["error"].as_str(), Some(e.as_str()));
        assert_eq!(v["mode"], "managed", "console fields survive the reset");
        assert_eq!(v["managed"]["network"], "regtest");
        assert!(!f.dir.join("root/networks/regtest/logs/node.log").exists(), "nothing was spawned");
        drop(g);
        // The same through the toggle alias and restart.
        assert!(!node_action(&f.state, "toggle").await.ok);
        assert!(!node_action(&f.state, "restart").await.ok);
        drop(l);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn nit_the_status_names_the_data_root_and_no_other_paths() {
        let f = fixture("paths", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let text = serde_json::to_string(&f.state.read().await.snapshot).unwrap();
        let root = f.dir.join("root").display().to_string();
        assert!(text.contains(&root), "managed mode shows the data root");
        let bin = f.dir.join("fake-blvm.sh").display().to_string();
        assert!(!text.contains(&bin), "the node binary's path is not in the status");
        assert!(!text.contains("node.log"), "neither is the log file path");
        let v: Value = serde_json::from_str(&text).unwrap();
        assert!(v["managed"].get("binary").is_none() && v["managed"].get("log_file").is_none());
        let _ = std::fs::remove_dir_all(&f.dir);

        // Standalone: no path of this machine at all.
        let f = fixture("paths-sa", EnvOverrides::default(), true);
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let text = serde_json::to_string(&f.state.read().await.snapshot).unwrap();
        assert!(!text.contains(&f.dir.display().to_string()), "standalone status shows no folder: {text}");
        let _ = std::fs::remove_dir_all(&f.dir);
    }


    // ---- persistence and auto-start (U4/U6) ----

    /// A data folder as the wizard leaves it: `console.json` saved, nothing else asked.
    fn saved_console(tag: &str, env: EnvOverrides) -> (PathBuf, Console) {
        let dir = std::env::temp_dir().join(format!("blvm-persist-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-blvm.sh");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = env;
        env.data_root = Some(dir.join("root").display().to_string());
        env.node_bin.get_or_insert(bin.display().to_string());
        let saved = ConsoleSettings {
            setup_done: true,
            mode: Some("managed".into()),
            network: Some("regtest".into()),
            ..Default::default()
        };
        settings::save_console(&dir.join("state"), &saved).unwrap();
        // No env mode: the saved file alone says "managed", as after a restart of the app.
        let c = Console::load(Some(dir.join("state")), env, None);
        (dir, c)
    }

    fn shared(c: Console) -> Shared {
        let mut live = LiveState::new(c.initial_rpc());
        live.console = c;
        Arc::new(RwLock::new(live))
    }

    #[tokio::test]
    async fn a_restart_with_a_saved_console_json_shows_no_wizard_and_starts_the_node() {
        let (dir, c) = saved_console("inproc", EnvOverrides::default());
        assert_eq!(c.eff.mode, Mode::Managed);
        assert_eq!(c.eff.mode_source, "file");
        assert!(c.eff.setup_done, "no wizard on a configured console");
        assert!(c.settings.start_on_boot_effective(true), "start on boot is the managed default");
        assert_eq!(c.network().map(|n| n.id), Some("regtest"));
        assert_eq!(c.initial_rpc(), "127.0.0.1:18443");
        let state = shared(c);
        {
            let mut g = state.write().await;
            stamp_power(&mut g);
            let snap = &g.snapshot;
            assert_eq!((snap.mode, snap.setup_done), ("managed", true));
        }
        boot(&state).await;
        let agent = state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent.state(), ProcState::Starting, "started with no user action");
        agent.stop_and_wait().await;

        // Switched off in the settings: a restart does not start it.
        let saved = ConsoleSettings {
            setup_done: true,
            mode: Some("managed".into()),
            network: Some("regtest".into()),
            start_on_boot: Some(false),
            ..Default::default()
        };
        settings::save_console(&dir.join("state"), &saved).unwrap();
        let env = EnvOverrides {
            data_root: Some(dir.join("root").display().to_string()),
            node_bin: Some(dir.join("fake-blvm.sh").display().to_string()),
            ..Default::default()
        };
        let state2 = shared(Console::load(Some(dir.join("state")), env, None));
        boot(&state2).await;
        let agent2 = state2.read().await.console.agent.clone().unwrap();
        assert_eq!(agent2.state(), ProcState::Stopped);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn boot_does_not_start_a_node_that_has_no_program_yet() {
        let dir = std::env::temp_dir().join(format!("blvm-persist-noprog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let saved = ConsoleSettings { setup_done: true, network: Some("regtest".into()), ..Default::default() };
        settings::save_console(&dir.join("state"), &saved).unwrap();
        let env = EnvOverrides {
            mode: Some("managed".into()),
            data_root: Some(dir.join("root").display().to_string()),
            ..Default::default()
        };
        let state = shared(Console::load(Some(dir.join("state")), env, None));
        boot(&state).await;
        let agent = state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent.state(), ProcState::Stopped, "no crash banner for a node that was never installed");
        assert_eq!(agent.view().last_error, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn with_an_external_agent_boot_writes_the_wish_instead_of_spawning() {
        let env = EnvOverrides { agent_external: true, node_host: Some("node.internal".into()), ..Default::default() };
        let (dir, c) = saved_console("ext", env);
        assert!(c.is_external());
        // The poll target is the other container, on the network's own RPC port.
        assert_eq!(c.initial_rpc(), "node.internal:18443");
        let state = shared(c);
        let root = dir.join("root");
        // The agent already runs the node (what it wrote before the console restarted): adopt, write nothing.
        std::fs::create_dir_all(root.join("control")).unwrap();
        let running = control::StateFile {
            heartbeat: crate::persist::unix_now(),
            state: "running".into(),
            pid: Some(4242),
            network: Some("regtest".into()),
            binary_found: true,
            ..Default::default()
        };
        control::write_state(&root, &running).unwrap();
        boot(&state).await;
        assert!(control::read_desired(&root).is_none(), "the agent's node is left alone");
        let agent = state.read().await.console.agent.clone().unwrap();
        assert!(agent.is_external() && agent.alive());
        assert_eq!(agent.state(), ProcState::Running);

        // Nothing running: write desired.json with run = true. (A fresh console on the same folder.)
        control::write_state(&root, &control::StateFile { heartbeat: crate::persist::unix_now(), binary_found: true, ..Default::default() }).unwrap();
        let env2 = EnvOverrides { agent_external: true, data_root: Some(root.display().to_string()), ..Default::default() };
        let state2 = shared(Console::load(Some(dir.join("state")), env2, None));
        boot(&state2).await;
        let d = control::read_desired(&root).expect("desired.json written");
        assert_eq!((d.running, d.network.as_deref(), d.generation), (true, Some("regtest"), 1));
        {
            let mut g = state2.write().await;
            stamp_power(&mut g);
            let snap = g.snapshot.clone();
            assert!(snap.bundle_available, "the agent has a program even though this container does not");
            let m = snap.managed.unwrap();
            assert!(m.agent.external && m.agent.alive);
            assert_eq!(m.process.state, ProcState::Starting);
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_node_host_default_and_override_apply_only_to_an_external_agent() {
        let base = EnvOverrides { mode: Some("managed".into()), ..Default::default() };
        let regtest = networks::by_id("regtest").unwrap();
        let c = Console::load(None, base.clone(), None);
        assert_eq!(c.rpc_for(regtest), "127.0.0.1:18443", "in-process: loopback");
        let c = Console::load(None, EnvOverrides { agent_external: true, ..base.clone() }, None);
        assert_eq!(c.rpc_for(regtest), "btc-commons-node-view_agent_1:18443");
        let c = Console::load(None, EnvOverrides { agent_external: true, node_host: Some("agent.local".into()), ..base.clone() }, None);
        assert_eq!(c.rpc_for(regtest), "agent.local:18443");
        // An override in the settings still wins.
        let mut c = Console::load(None, EnvOverrides { agent_external: true, ..base.clone() }, None);
        c.settings.rpc_overrides.insert("regtest".into(), "10.0.0.7:1234".into());
        assert_eq!(c.rpc_for(regtest), "10.0.0.7:1234");
        // Standalone ignores it all.
        let c = Console::load(None, EnvOverrides { agent_external: true, node_host: Some("x".into()), ..Default::default() }, None);
        assert!(!c.is_external());
    }

    #[tokio::test]
    async fn the_boot_setting_has_two_names_and_a_managed_default() {
        // Read: the old name still loads.
        let old: ConsoleSettings = serde_json::from_str(r#"{"start_on_launch":false,"setup_done":true}"#).unwrap();
        assert_eq!(old.start_on_boot, Some(false));
        assert!(!old.start_on_boot_effective(true));
        assert!(ConsoleSettings::default().start_on_boot_effective(true), "default on in managed mode");
        // Write: both names, read back as start_on_boot.
        let f = fixture("boot-names", managed_env(), true);
        let (_, v) = patch_settings(&f.state, &json!({"start_on_launch": false})).await;
        assert_eq!((v["start_on_boot"].clone(), v["start_on_launch"].clone()), (json!(false), json!(false)));
        let (_, v) = patch_settings(&f.state, &json!({"start_on_boot": true})).await;
        assert_eq!(v["start_on_boot"], true);
        let saved = settings::load_console(&f.dir.join("state"));
        assert_eq!(saved.start_on_boot, Some(true));
        let raw = std::fs::read_to_string(f.dir.join("state/console.json")).unwrap();
        assert!(raw.contains("\"start_on_boot\""), "{raw}");
        // A fresh managed console reports it on.
        let g = fixture("boot-default", managed_env(), true);
        assert_eq!(settings_json(&g.state.read().await.console)["start_on_boot"], true);
        let (c, _) = patch_settings(&g.state, &json!({"start_on_boot": "yes"})).await;
        assert_eq!(c, 400);
        let _ = std::fs::remove_dir_all(&f.dir);
        let _ = std::fs::remove_dir_all(&g.dir);
    }

    #[tokio::test]
    async fn the_updates_setting_obeys_the_environment() {
        let env = EnvOverrides { updates: Some("off".into()), ..managed_env() };
        let f = fixture("upd-env", env, true);
        let g = f.state.read().await;
        let v = settings_json(&g.console);
        assert_eq!(v["updates"], "off");
        assert!(v["env_locked"].as_array().unwrap().iter().any(|k| k == "updates"));
        drop(g);
        let (c, _) = patch_settings(&f.state, &json!({"updates":"self"})).await;
        assert_eq!(c, 409);
        let (c, _) = patch_settings(&f.state, &json!({"updates":"off"})).await;
        assert_eq!(c, 200);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// REVIEW GAP: in external mode `boot` writes `running: true` even when the user had stopped the node, so an
    /// app or Umbrel restart undoes the user's Stop. (Expected to FAIL until fixed.)
    #[tokio::test]
    async fn review_a_user_stop_survives_a_console_restart_with_an_external_agent() {
        let env = EnvOverrides { agent_external: true, node_host: Some("node.internal".into()), ..Default::default() };
        let (dir, c) = saved_console("review-stop", env);
        let root = dir.join("root");
        let stopped = control::Desired { generation: 7, network: Some("regtest".into()), running: false, updated: crate::persist::unix_now(), ..Default::default() };
        control::write_desired(&root, &stopped).unwrap();
        control::write_state(&root, &control::StateFile { heartbeat: crate::persist::unix_now(), generation: 7, binary_found: true, ..Default::default() }).unwrap();
        let state = shared(c);
        boot(&state).await;
        let d = control::read_desired(&root).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!d.running, "the user stopped the node; a console restart turned it back on (generation {})", d.generation);
    }

    /// REVIEW (documents current behaviour): "Skip" / "Finish without a node" on Umbrel posts mode=standalone, which
    /// the env-locked managed mode refuses with a message about an environment variable.
    #[tokio::test]
    async fn review_skip_on_umbrel_is_refused_with_an_env_var_message() {
        let dir = std::env::temp_dir().join(format!("blvm-review-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = EnvOverrides { mode: Some("managed".into()), agent_external: true, data_root: Some(dir.join("root").display().to_string()), ..Default::default() };
        let state = shared(Console::load(Some(dir.join("state")), env, None));
        let (code, v) = setup(&state, &json!({"mode": "standalone", "start": false, "stop_on_exit": true, "modules": []})).await;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(code, 409);
        assert!(v["error"].as_str().unwrap().contains("BLVM_UI_MODE"), "{v}");
    }

    // ---- review fixes: the user's Stop sticks (B2), the RPC login (B1), setup without a program, Skip ----

    fn reload_env(dir: &Path) -> EnvOverrides {
        EnvOverrides {
            data_root: Some(dir.join("root").display().to_string()),
            node_bin: Some(dir.join("fake-blvm.sh").display().to_string()),
            ..Default::default()
        }
    }

    async fn console_json(dir: &Path) -> ConsoleSettings {
        settings::load_console(&dir.join("state"))
    }

    #[tokio::test]
    async fn a_stop_survives_a_restart_of_the_console_and_a_start_survives_it_too() {
        let (dir, c) = saved_console("sticks", EnvOverrides::default());
        let state = shared(c);
        boot(&state).await;
        let agent = state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent.state(), ProcState::Starting, "the first boot starts the node: nothing was ever refused");

        // The user stops it.
        let out = node_action(&state, "stop").await;
        assert!(out.ok, "{out:?}");
        agent.wait_inactive(Duration::from_secs(10)).await;
        assert_eq!(console_json(&dir).await.node_wanted, Some(false), "saved at once");

        // The app starts again: "start when the console opens" is on (the default) but the node stays off.
        let state2 = shared(Console::load(Some(dir.join("state")), reload_env(&dir), None));
        assert!(state2.read().await.console.settings.start_on_boot_effective(true));
        boot(&state2).await;
        let agent2 = state2.read().await.console.agent.clone().unwrap();
        assert_eq!(agent2.state(), ProcState::Stopped, "a restart does not undo Stop");

        // The user starts it, the app restarts, and the node comes back.
        let out = node_action(&state2, "start").await;
        assert!(out.ok, "{out:?}");
        assert_eq!(console_json(&dir).await.node_wanted, Some(true));
        agent2.stop_and_wait().await; // (the process goes, the wish stays: a quit is not a Stop)
        let state3 = shared(Console::load(Some(dir.join("state")), reload_env(&dir), None));
        boot(&state3).await;
        let agent3 = state3.read().await.console.agent.clone().unwrap();
        assert_eq!(agent3.state(), ProcState::Starting, "start -> restart -> running");
        agent3.stop_and_wait().await;

        // "Start when the console opens" off: nothing starts, whatever was wanted.
        {
            let mut g = state3.write().await;
            g.console.settings.start_on_boot = Some(false);
            g.console.save().unwrap();
        }
        let state4 = shared(Console::load(Some(dir.join("state")), reload_env(&dir), None));
        boot(&state4).await;
        assert_eq!(state4.read().await.console.agent.clone().unwrap().state(), ProcState::Stopped);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn an_older_console_json_without_the_wish_still_starts_the_node() {
        let (dir, c) = saved_console("nowish", EnvOverrides::default());
        assert_eq!(c.settings.node_wanted, None);
        assert!(c.settings.node_wanted_effective());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn with_an_external_agent_the_console_writes_nothing_when_the_agent_has_a_wish_already() {
        let ext = || EnvOverrides { agent_external: true, node_host: Some("node.internal".into()), ..Default::default() };
        // desired.json exists and says "running": the console leaves it (and its generation) alone.
        let (dir, c) = saved_console("ext-keep", ext());
        let root = dir.join("root");
        let running = control::Desired { generation: 4, network: Some("regtest".into()), running: true, updated: crate::persist::unix_now(), ..Default::default() };
        control::write_desired(&root, &running).unwrap();
        control::write_state(&root, &control::StateFile { heartbeat: crate::persist::unix_now(), generation: 4, state: "stopped".into(), binary_found: true, ..Default::default() }).unwrap();
        boot(&shared(c)).await;
        assert_eq!(control::read_desired(&root).unwrap().generation, 4, "nothing written");
        let _ = std::fs::remove_dir_all(&dir);

        // No desired.json and the user had stopped: nothing is written either.
        let (dir, mut c) = saved_console("ext-stopped", ext());
        let root = dir.join("root");
        c.settings.node_wanted = Some(false);
        control::write_state(&root, &control::StateFile { heartbeat: crate::persist::unix_now(), binary_found: true, ..Default::default() }).unwrap();
        boot(&shared(c)).await;
        assert!(control::read_desired(&root).is_none(), "the user's Stop is the answer");
        let _ = std::fs::remove_dir_all(&dir);

        // Stop through the page, restart the web container, restart the agent: the node stays off at every step.
        let (dir, c) = saved_console("ext-flow", ext());
        let root = dir.join("root");
        control::write_state(&root, &control::StateFile { heartbeat: crate::persist::unix_now(), binary_found: true, ..Default::default() }).unwrap();
        let state = shared(c);
        boot(&state).await;
        assert!(control::read_desired(&root).unwrap().running, "the first boot asks for a start");
        let out = node_action(&state, "stop").await;
        assert!(out.ok, "{out:?}");
        let stopped = control::read_desired(&root).unwrap();
        assert!(!stopped.running);
        assert_eq!(console_json(&dir).await.node_wanted, Some(false));
        let state2 = shared(Console::load(Some(dir.join("state")), EnvOverrides { data_root: Some(root.display().to_string()), ..ext() }, None));
        boot(&state2).await;
        assert_eq!(control::read_desired(&root).unwrap(), stopped, "web restart: desired.json untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn setup_remembers_whether_the_node_was_to_run_and_a_switch_wants_it_running() {
        let f = fixture("wish-setup", managed_env(), true);
        let (code, _) = setup(&f.state, &json!({"mode":"managed","network":"regtest","start":false})).await;
        assert_eq!(code, 200);
        assert_eq!(f.state.read().await.console.settings.node_wanted, Some(false), "\"don't start it\" is a choice too");
        let agent = f.state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent.state(), ProcState::Stopped);
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","start":true})).await;
        assert_eq!((code, v["ok"].clone()), (200, json!(true)), "{v}");
        assert_eq!(f.state.read().await.console.settings.node_wanted, Some(true));
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn setup_without_a_program_records_the_choice_but_will_not_start() {
        let f = fixture("noprog-setup", managed_env(), false);
        // Finish without starting: fine, nothing needs a program yet.
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"testnet4","start":false,"stop_on_exit":true,"modules":[]})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["ok"], true);
        {
            let g = f.state.read().await;
            assert_eq!(g.console.settings.network.as_deref(), Some("testnet4"));
            assert!(g.console.eff.setup_done && g.console.settings.setup_done);
            assert_eq!(g.console.agent.as_ref().unwrap().state(), ProcState::Stopped);
            assert!(g.snapshot.mode == "managed" || g.console.is_managed());
        }
        assert_eq!(console_json(&f.dir).await.network.as_deref(), Some("testnet4"));
        // Starting without a program is still refused, and writes nothing new.
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"signet","start":true})).await;
        assert_eq!(code, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("no node program"));
        assert_eq!(console_json(&f.dir).await.network.as_deref(), Some("testnet4"));
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn skip_on_umbrel_marks_setup_done_without_touching_the_mode() {
        let dir = std::env::temp_dir().join(format!("blvm-skip-umbrel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = EnvOverrides { mode: Some("managed".into()), agent_external: true, data_root: Some(dir.join("root").display().to_string()), ..Default::default() };
        let state = shared(Console::load(Some(dir.join("state")), env, None));
        assert!(!state.read().await.console.eff.setup_done, "fresh Umbrel install: the wizard shows");
        let (code, v) = patch_settings(&state, &json!({"setup_done": true})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["setup_done"], true);
        assert_eq!(v["mode"], "managed", "the mode is still the app's own");
        {
            let g = state.read().await;
            assert!(g.console.eff.setup_done && g.console.is_managed() && g.console.is_external());
        }
        assert!(console_json(&dir).await.setup_done);
        // And the old way, posting a mode, says plainly why not.
        let (code, v) = setup(&state, &json!({"mode":"standalone"})).await;
        assert_eq!(code, 409);
        let e = v["error"].as_str().unwrap();
        assert!(e.contains("always runs its own node") && e.contains("BLVM_UI_MODE"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn setup_and_settings_changes_wait_for_a_running_install() {
        let f = fixture("job-setup", managed_env(), true);
        f.state.read().await.console.job.lock().unwrap().phase = "download";
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest"})).await;
        assert_eq!(code, 409, "{v}");
        assert!(v["error"].as_str().unwrap().contains("installed or updated"));
        let (code, _) = patch_settings(&f.state, &json!({"network":"signet"})).await;
        assert_eq!(code, 409);
        // Things that do not move the node are still allowed.
        let (code, _) = patch_settings(&f.state, &json!({"start_on_boot":false})).await;
        assert_eq!(code, 200);
        f.state.read().await.console.job.lock().unwrap().phase = "idle";
        let (code, _) = patch_settings(&f.state, &json!({"network":"signet"})).await;
        assert_eq!(code, 200);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn a_console_that_starts_the_node_itself_gives_it_a_generated_login_once() {
        let (dir, c) = saved_console("login-gen", EnvOverrides::default());
        let auth = c.rpc_auth().expect("a managed console in this process has a login for its node");
        assert_eq!(auth.user, "commons");
        assert!(auth.pass.len() >= 32 && auth.pass.bytes().all(|b| b.is_ascii_hexdigit()), "random, long, plain");
        assert_eq!(c.agent_config().rpc_auth, Some(auth.clone()), "the agent starts the node with it");
        // In memory only until something needs it ...
        assert_eq!(console_json(&dir).await.rpc_pass, None);
        c.persist_login();
        let saved = console_json(&dir).await;
        assert_eq!((saved.rpc_user.as_deref(), saved.rpc_pass.as_deref()), (Some("commons"), Some(auth.pass.as_str())));
        let mode = std::fs::metadata(dir.join("state/console.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the file holds a password");
        // ... and the next run of the console finds the same one (a node it adopts keeps working).
        let again = Console::load(Some(dir.join("state")), reload_env(&dir), None);
        assert_eq!(again.rpc_auth(), Some(auth.clone()));
        // The page never sees it.
        let shown = settings_json(&again).to_string();
        assert!(!shown.contains(&auth.pass) && !shown.contains("rpc_pass"), "{shown}");
        let mut g = LiveState::new(again.initial_rpc());
        g.console = again;
        stamp_power(&mut g);
        assert!(!serde_json::to_string(&g.snapshot).unwrap().contains(&auth.pass));
        // The environment wins over the saved pair.
        let env = EnvOverrides { rpc_user: Some("me".into()), rpc_pass: Some("mine".into()), ..reload_env(&dir) };
        let c3 = Console::load(Some(dir.join("state")), env, None);
        assert_eq!(c3.rpc_auth(), Some(crate::agent::RpcAuth { user: "me".into(), pass: "mine".into() }));
        // Half a pair in the environment is no pair.
        let env = EnvOverrides { rpc_user: Some("me".into()), ..reload_env(&dir) };
        assert_eq!(Console::load(Some(dir.join("state")), env, None).rpc_auth(), Some(auth));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn only_a_node_started_by_this_process_gets_a_login_from_it() {
        // Standalone: somebody else's node, no login of ours.
        assert_eq!(Console::load(None, EnvOverrides::default(), None).rpc_auth(), None);
        // Umbrel (agent in another container): the agent has its own login from its environment.
        let env = EnvOverrides { mode: Some("managed".into()), agent_external: true, ..Default::default() };
        let c = Console::load(None, env, None);
        assert_eq!(c.rpc_auth(), None);
        assert_eq!(c.agent_config().rpc_auth, None);
        assert_eq!(c.settings.rpc_pass, None, "nothing is generated that no node would use");
    }

    #[tokio::test]
    async fn a_node_started_through_the_console_runs_with_the_config_and_the_login() {
        let f = fixture("login-start", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        let pass = f.state.read().await.console.rpc_auth().unwrap().pass;
        let out = node_action(&f.state, "start").await;
        assert!(out.ok, "{out:?}");
        let agent = f.state.read().await.console.agent.clone().unwrap();
        for _ in 0..200 {
            if agent.logs(None, 50).iter().any(|l| l.text.starts_with("up")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let toml = std::fs::read_to_string(f.dir.join("root/networks/regtest/blvm.toml")).unwrap();
        assert!(toml.contains(&format!("password = \"{pass}\"")) && toml.contains("required = true"), "{toml}");
        assert_eq!(console_json(&f.dir).await.rpc_pass.as_deref(), Some(pass.as_str()), "saved before the node got it");
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn an_agent_that_went_quiet_is_shown_as_gone_even_with_nothing_pending() {
        let env = EnvOverrides { agent_external: true, node_host: Some("node.internal".into()), ..Default::default() };
        let (dir, c) = saved_console("agent-gone", env);
        let root = dir.join("root");
        let now = crate::persist::unix_now();
        let running = |heartbeat| control::StateFile {
            heartbeat,
            generation: 3,
            state: "running".into(),
            pid: Some(4242),
            network: Some("regtest".into()),
            binary_found: true,
            ..Default::default()
        };
        control::write_desired(&root, &control::Desired { generation: 3, network: Some("regtest".into()), running: true, updated: now - 600, ..Default::default() }).unwrap();
        control::write_state(&root, &running(now)).unwrap();
        let state = shared(c);
        let snap = |state: &Shared| {
            let state = state.clone();
            async move {
                let mut g = state.write().await;
                stamp_power(&mut g);
                g.snapshot.managed.clone().unwrap()
            }
        };
        let m = snap(&state).await;
        assert!(m.agent.external && m.agent.alive);
        assert_eq!(m.process.state, ProcState::Running);

        // The agent's container stops: the heartbeat ages, nothing is pending, and the page is told.
        control::write_state(&root, &running(now - 120)).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await; // (state.json is cached for 200 ms)
        let m = snap(&state).await;
        assert!(!m.agent.alive);
        assert_eq!(m.process.state, ProcState::Crashed);
        assert!(m.process.last_error.as_deref().unwrap().contains("not responding"), "{:?}", m.process.last_error);
        assert_eq!(m.process.pid, None);

        // It comes back.
        control::write_state(&root, &running(crate::persist::unix_now())).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let m = snap(&state).await;
        assert!(m.agent.alive);
        assert_eq!(m.process.state, ProcState::Running);
        let _ = std::fs::remove_dir_all(dir);
    }
}
