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
use crate::sysinfo;
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
    /// Pretend the memory page size is this many bytes instead of reading it. Tests.
    pub page_size_override: Option<u64>,
    /// Look for a newer release in the background (set by the real process, never by tests).
    pub auto_check: bool,
    /// Hand the node's RPC login to the process-wide RPC client (set by the real process, never by tests:
    /// the client is shared by everything in one process).
    pub publish_rpc_login: bool,
    /// The generated RPC login exists only in memory so far (it is written with the next save, and in any case
    /// before a node is started with it: see [`Console::persist_login`]).
    login_unsaved: std::sync::atomic::AtomicBool,
    /// Peer-to-peer port for the node instead of the network's own, on loopback only. Tests only: a node in a
    /// test must not take a port that a real node of the person running the tests may be using.
    pub p2p_port: Option<u16>,
    disk: Arc<Mutex<DiskCache>>,
    last_pid: Option<u32>,
    /// Networks whose chain data is being deleted right now (the delete runs without the console's lock, so
    /// everything that could start a node or change the settings checks this).
    pub wiping: std::collections::BTreeSet<&'static str>,
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
            page_size_override: None,
            auto_check: false,
            publish_rpc_login: false,
            login_unsaved: std::sync::atomic::AtomicBool::new(false),
            p2p_port: None,
            disk: Arc::new(Mutex::new(DiskCache::default())),
            last_pid: None,
            wiping: Default::default(),
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
            page_size_override: None,
            auto_check: false,
            publish_rpc_login: false,
            login_unsaved: std::sync::atomic::AtomicBool::new(false),
            p2p_port: None,
            disk: Arc::new(Mutex::new(DiskCache::default())),
            last_pid: None,
            wiping: Default::default(),
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

    /// The login was created in this run and has not been saved yet: no node can have been started with it.
    pub fn login_is_new(&self) -> bool {
        self.login_unsaved.load(std::sync::atomic::Ordering::Relaxed)
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

    /// Platform facts of the computer this process runs on (memory page size and what follows from it).
    pub fn local_facts(&self) -> sysinfo::Facts {
        sysinfo::Facts::for_page(&self.platform(), self.page_size_override.or_else(sysinfo::page_size))
    }

    /// Platform facts of the computer that runs (or would run) the node: what the agent in the node container
    /// wrote to `state.json` when it is another container, else this process's own.
    pub fn node_facts(&self) -> sysinfo::Facts {
        let agent = if self.is_external() { self.agent.as_ref().and_then(Agent::platform_facts) } else { None };
        agent.unwrap_or_else(|| self.local_facts())
    }

    /// The node runs under the agent in another container.
    pub fn is_external(&self) -> bool {
        self.is_managed() && self.env.agent_external
    }

    /// The P2P port the node of this network listens on, for display. With the agent in another container it
    /// reads the same `BLVM_AGENT_P2P_PORT_<NET>` variables as the agent (the Umbrel app gives both containers
    /// the same values); a node this process starts itself uses the network's own port.
    pub fn p2p_port_for(&self, net: &Network) -> u16 {
        if self.is_external() {
            networks::effective_p2p_port(net)
        } else {
            net.p2p_port
        }
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
            // This network's own setting, else the global one (an older console.json).
            assume_valid: match net {
                Some(n) => self.settings.assume_valid_for(n.id),
                None => self.settings.assume_valid.clone(),
            },
            // This network's own block-pruning setting (`None`: keep every block).
            prune_keep_blocks: net.and_then(|n| self.settings.prune_for(n.id)),
            child_env: self.settings.child_env.clone(),
            // A node this process starts gets a login too (the agent of another container has its own).
            rpc_auth: self.rpc_auth(),
            p2p_port: self.p2p_port,
            p2p_listen_port: None,
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

    /// Measure one network's folder now and put the result into the cached table (after its chain data was deleted,
    /// so the status does not show the old size and heights until the next full measurement).
    pub fn refresh_net_disk(&self, root: &Path, net: &Network) {
        let dir = networks::dir(root, net);
        let bytes = if dir.is_dir() { crate::node_ctl::dir_allocated_bytes(&dir) } else { 0 };
        let d = NetDisk { bytes, has_data: chain_has_data(&dir), meta: networks::read_meta(root, net) };
        self.disk.lock().unwrap().nets.insert(net.id, d);
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
    /// The port a node on this network listens on for peers (the network's own unless the Umbrel app moved it).
    pub p2p_port: u16,
    pub bytes: u64,
    /// From `networks/<id>/meta.json` (the live height for the running network).
    pub last_height: Option<u64>,
    pub last_used: Option<u64>,
    pub has_data: bool,
    /// The console's block-pruning setting for this network (`None`: keeps every block).
    pub prune_keep_blocks: Option<u64>,
    /// The chain data of this network is being deleted right now.
    pub wiping: bool,
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

/// What the node container's agent reported about its computer.
#[derive(Debug, Clone, Serialize)]
pub struct AgentPlatformView {
    pub page_size: Option<u64>,
    pub runs_here: bool,
    pub reason: Option<String>,
}

/// `managed.platform` in `/api/status`: the memory page size, and whether the official node release can start.
#[derive(Debug, Clone, Serialize)]
pub struct PlatformView {
    /// Page size of the console's own process, in bytes.
    pub page_size: Option<u64>,
    /// Page size of the computer that runs the node (the agent's when it is in another container).
    pub node_page_size: Option<u64>,
    pub runs_here: bool,
    pub runs_here_reason: Option<String>,
    /// What the node container's agent wrote to `state.json`; `None` when the node runs in this process.
    pub agent: Option<AgentPlatformView>,
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
    pub platform: PlatformView,
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
                p2p_port: c.p2p_port_for(n),
                bytes: d.bytes,
                last_height: meta.last_height,
                last_used: meta.last_used,
                has_data: d.has_data,
                prune_keep_blocks: c.settings.prune_for(n.id),
                wiping: c.wiping.contains(n.id),
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
        platform: {
            let local = c.local_facts();
            let remote = if c.is_external() { agent.platform_facts() } else { None };
            let node = remote.clone().unwrap_or_else(|| local.clone());
            PlatformView {
                page_size: local.page_size,
                node_page_size: node.page_size,
                runs_here: node.runs_here,
                runs_here_reason: node.reason.clone(),
                agent: remote.map(|f| AgentPlatformView { page_size: f.page_size, runs_here: f.runs_here, reason: f.reason }),
            }
        },
    }
}

/// The sidebar pages that installed and enabled modules add (design L). `enabled_modules` are module ids, with or
/// without the `blvm-` prefix. A module that adds no page, or one this console does not know, adds nothing.
pub fn modules_pages_for<S: AsRef<str>>(enabled_modules: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in enabled_modules {
        let id = m.as_ref();
        let id = id.strip_prefix("blvm-").unwrap_or(id);
        let page = match id {
            "stratum-v2" | "datum" | "gridpool" => "mining",
            _ => continue,
        };
        if !out.iter().any(|p| p == page) {
            out.push(page.to_string());
        }
    }
    out
}

fn stamp_console_fields(snap: &mut Snapshot, c: &Console) {
    snap.mode = c.eff.mode.as_str();
    snap.setup_done = c.eff.setup_done;
    // A node program this console can run: on disk here, or (agent in another container) one the agent says
    // it has, for instance the one baked into its image.
    snap.bundle_available = c.eff.node_bin.as_deref().is_some_and(|p| p.is_file())
        || (c.is_external() && c.agent.as_ref().is_some_and(|a| a.binary_found() == Some(true)));
    // This host has no module store yet, so nothing can be installed: no module pages, in either mode.
    snap.modules_pages = modules_pages_for::<String>(&[]);
    snap.show_logs = c.settings.show_logs;
    stamp_disk(snap, c);
}

/// Managed mode: the chain size is what the network's folder takes on disk (the cached measurement behind
/// `managed.networks[].bytes`), never the node's `size_on_disk`: the official `blvm` computes that as about 1 MB
/// per block, an estimate that was off by a factor of 2 to 8 on real chains. The free space is that of the
/// volume holding the data root. Before the first measurement the figure is empty rather than the estimate.
fn stamp_disk(snap: &mut Snapshot, c: &Console) {
    if !c.is_managed() {
        return;
    }
    let bytes = c.network().map_or(0, |n| c.disk.lock().unwrap().net(n.id).bytes);
    let (num, unit, label) = crate::state::format_bytes_parts(bytes);
    snap.disk_used_num = num;
    snap.disk_used_unit = unit;
    snap.disk_used_label = label;
    snap.disk_used_bytes = bytes;
    snap.disk_used_source = "folder";
    let (total, free) = crate::state::volume_space_at(&c.eff.data_root);
    let (free_num, free_unit, free_label) = crate::state::format_bytes_parts(free);
    snap.disk_free_num = free_num;
    snap.disk_free_unit = free_unit;
    snap.disk_free_label = free_label;
    snap.disk_free_bytes = free;
    snap.disk_total_bytes = total;
    snap.disk_vol_used_label = crate::state::format_bytes_parts(total.saturating_sub(free)).2;
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
    // The console's block-pruning setting for this network decides `pruning.enabled`, also while the node is down.
    // What the node said about its own pruning counts only while it answers.
    let keep = c.network().and_then(|n| c.settings.prune_for(n.id));
    if !snap.connected {
        snap.pruning.forget_node();
    }
    crate::state::finish_pruning_with_node(&mut snap.pruning, keep, snap.local_height, snap.network_height);
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
    // A login that only exists since this run (an install from before nodes had one): save it now, and let the
    // agent restart a node it adopts that still answers without a password, instead of stopping it as broken.
    let login_is_new = state.read().await.console.login_is_new();
    if login_is_new && !agent.is_external() {
        state.read().await.console.persist_login();
        agent.restart_adopted_node_with_login(true);
    }
    let adopted = agent.adopt();
    if !adopted {
        agent.restart_adopted_node_with_login(false);
    }
    if adopted {
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
        "show_logs": s.show_logs,
        "modules_disabled": s.modules_disabled,
        "updates": c.updates_channel(),
        "rpc_overrides": s.rpc_overrides,
        "assume_valid": s.assume_valid,
        "assume_valid_by_network": s.assume_valid_by_network,
        "assume_valid_defaults": assume_valid_defaults_json(),
        "assume_valid_effective": assume_valid_effective_json(c),
        "prune_by_network": s.prune_by_network,
        "prune_min_blocks": networks::PRUNE_MIN_BLOCKS,
        "prune_effective": prune_effective_json(c),
        "p2p_ports": networks::ALL.iter().map(|n| (n.id.to_string(), c.p2p_port_for(n))).collect::<BTreeMap<_, _>>(),
        "child_env": s.child_env,
        "env_locked": c.eff.env_locked,
        "sources": {
            "mode": c.eff.mode_source,
            "node_bin": c.eff.node_bin_source,
            "data_root": c.eff.data_root_source,
        },
    })
}

/// `{ "<net id>": { "height": <node's built-in default>, "forced_off": <console runs it with full validation> } }`.
fn assume_valid_defaults_json() -> Value {
    let mut m = serde_json::Map::new();
    for n in networks::ALL.iter() {
        m.insert(
            n.id.to_string(),
            json!({ "height": networks::assume_valid_default_height(n), "forced_off": n.full_validation }),
        );
    }
    Value::Object(m)
}

/// What the node on the current network will be started with: `{ network, value, argv }`
/// (`null` before a network is chosen). `value` is the stored setting (`null`: the network's default).
fn assume_valid_effective_json(c: &Console) -> Value {
    let Some(n) = c.network() else { return Value::Null };
    let value = c.settings.assume_valid_for(n.id);
    let argv = crate::agent::assume_valid_args(value.as_deref(), n);
    let mut out = json!({ "network": n.id, "value": value, "argv": argv });
    // The node reads this variable after the command line, so a value set under Child environment wins.
    if c.settings.child_env.contains_key("BLVM_ASSUME_VALID_HEIGHT") {
        out["overridden_by_env"] = json!(true);
        out["note"] = json!(ASSUME_VALID_ENV_NOTE);
    }
    out
}

const ASSUME_VALID_ENV_NOTE: &str = "The variable BLVM_ASSUME_VALID_HEIGHT is set under Child environment. When it holds a block height, the node uses that instead of this setting.";

/// What the node on the current network will be started with as far as pruning goes:
/// `{ network, keep_blocks: <u64|null>, has_data, has_pruned_data }` (`null` before a network is chosen).
/// `has_data` and `has_pruned_data` are read from the folder now (not from the cached sizes), because the page
/// decides on them whether to warn or ask first. `has_pruned_data`: chain data exists AND a node was once started
/// pruned on it, so going back to a full node needs the data deleted.
fn prune_effective_json(c: &Console) -> Value {
    let Some(n) = c.network() else { return Value::Null };
    let dir = networks::dir(&c.eff.data_root, n);
    let has_data = chain_has_data(&dir);
    json!({
        "network": n.id,
        "keep_blocks": c.settings.prune_for(n.id),
        "has_data": has_data,
        "has_pruned_data": has_data && ran_pruned(&c.eff.data_root, n),
    })
}

/// A node was started with block pruning on this network's folder (`meta.json` `ran_pruned`).
fn ran_pruned(root: &Path, net: &Network) -> bool {
    networks::read_meta(root, net).is_some_and(|m| m.ran_pruned == Some(true))
}

/// One assume-valid value from a request: a string the node accepts (trimmed), else `None`.
fn parse_assume_valid(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|a| valid_assume_valid_per_network(a)).map(str::to_string)
}

/// `default`, `off` or a block height (1-9 digits). A block hash is not accepted for the per-network keys: the
/// node reads one as height 0, so nothing would be skipped. [`crate::agent::valid_assume_valid`] (with the
/// hash) is kept for the older global `assume_valid` key and for files written by older consoles.
fn valid_assume_valid_per_network(v: &str) -> bool {
    v == "default" || v == "off" || (!v.is_empty() && v.len() <= 9 && v.bytes().all(|b| b.is_ascii_digit()))
}

const ASSUME_VALID_MSG: &str = "must be \"default\", \"off\" or a block height.";

/// Shown when block pruning is switched on (or lowered) for a network whose chain data already exists.
const PRUNE_CONVERT_WARNING: &str = "The node converts the existing store on its next start: it deletes old blocks one by one from the start of the chain and does not answer meanwhile. Minutes on Testnet4, possibly hours on mainnet.";
/// Sent when the number of kept blocks is raised.
const PRUNE_RAISE_NOTE: &str = "Blocks the node has already deleted do not come back. Only the blocks that arrive from now on are kept.";

/// A `prune_by_network` / `prune_keep_blocks` value: `Some(None)` for `null` (keep every block), `Some(Some(n))`
/// for a whole number within the allowed range, `None` for anything else.
fn parse_prune(v: &Value) -> Option<Option<u64>> {
    if v.is_null() {
        return Some(None);
    }
    v.as_u64().filter(|n| (networks::PRUNE_MIN_BLOCKS..=networks::PRUNE_MAX_BLOCKS).contains(n)).map(Some)
}

fn prune_range_msg(what: &str) -> String {
    format!(
        "{what} must be null (keep every block) or a whole number from {} to {}.",
        networks::PRUNE_MIN_BLOCKS,
        networks::PRUNE_MAX_BLOCKS
    )
}

/// What a change of the pruning setting means for chain data that is already on disk.
#[derive(Default)]
struct PruneOutcome {
    /// Networks whose chain data must be deleted (pruned -> full node, confirmed).
    wipe: Vec<&'static Network>,
    /// The node will convert an existing store on its next start (slow).
    warning: bool,
    /// The number of kept blocks went up.
    raised: bool,
}

impl PruneOutcome {
    /// Fields of the response for the changes above.
    fn add_to(&self, body: &mut Value) {
        if let Some(m) = body.as_object_mut() {
            if self.warning {
                m.insert("warning".into(), json!(PRUNE_CONVERT_WARNING));
            }
            if self.raised {
                m.insert("note".into(), json!(PRUNE_RAISE_NOTE));
            }
        }
    }
}

/// Decide what changing one network's pruning setting from `old` to `new` needs, given the chain data under
/// `root`. A node that was pruned and is switched to a full node would silently advertise blocks it no longer
/// has, so that change is refused (409, `needs: "wipe"`) until the caller confirms the deletion of the data.
/// "Was pruned" means a node was actually started with pruning on that folder (`meta.json` `ran_pruned`): a
/// setting that was saved and never used, over a full store, is taken back with a plain save.
fn plan_prune_change(
    root: &Path,
    net: &'static Network,
    old: Option<u64>,
    new: Option<u64>,
    confirm_wipe: bool,
    out: &mut PruneOutcome,
) -> Result<(), (u16, Value)> {
    let has_data = || chain_has_data(&networks::dir(root, net));
    match (old, new) {
        (Some(_), None) if has_data() && ran_pruned(root, net) => {
            if !confirm_wipe {
                return Err((
                    409,
                    json!({
                        "ok": false,
                        "needs": "wipe",
                        "error": format!(
                            "A pruned node cannot go back to keeping every block: the blocks it deleted are gone. \
                             The chain data for {} has to be deleted and downloaded again.",
                            net.label
                        ),
                    }),
                ));
            }
            out.wipe.push(net);
        }
        (None, Some(_)) if has_data() => out.warning = true,
        (Some(o), Some(n)) if n < o && has_data() => out.warning = true,
        (Some(o), Some(n)) if n > o => out.raised = true,
        _ => {}
    }
    Ok(())
}

/// Tests: how long the delete under a data folder takes (a big store on a slow disk).
#[cfg(test)]
static SLOW_WIPE: Mutex<BTreeMap<PathBuf, Duration>> = Mutex::new(BTreeMap::new());

/// Delete `<root>/networks/<id>/data` and nothing else (`meta.json`, `logs/` and `blvm.toml` stay) and forget the
/// heights and `ran_pruned` in `meta.json`.
fn wipe_chain_data(root: &Path, net: &Network) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(d) = SLOW_WIPE.lock().unwrap().get(root).copied() {
        std::thread::sleep(d); // a big store on a slow disk
    }
    match std::fs::remove_dir_all(networks::dir(root, net).join("data")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(mut m) = networks::read_meta(root, net) {
        m.last_height = None;
        m.last_header = None;
        m.ran_pruned = None;
        let _ = networks::write_meta(root, net, &m);
    }
    Ok(())
}

async fn wipe_all(root: &Path, nets: &[&'static Network]) -> Result<(), (u16, Value)> {
    for net in nets {
        let (r, n) = (root.to_path_buf(), *net);
        match tokio::task::spawn_blocking(move || wipe_chain_data(&r, n)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err((500, json!({ "ok": false, "error": format!("Could not delete the {} chain data: {e}", net.label) }))),
            Err(e) => return Err((500, json!({ "ok": false, "error": format!("Could not delete the {} chain data: {e}", net.label) }))),
        }
    }
    Ok(())
}

/// First half of a wipe, with the console locked, in this order: stop anything from starting a node (and cancel a
/// pending automatic restart), look again whether a node process can exist, mark the networks as being wiped.
/// The caller then lets go of the lock for the delete and calls [`end_wipe`]. `Err` is the 409 to answer with;
/// nothing is left marked then.
fn begin_wipe(c: &mut Console, nets: &[&'static Network]) -> Result<(), (u16, Value)> {
    if c.job_active() {
        return Err((409, json!({ "ok": false, "error": JOB_RUNNING })));
    }
    if let Some(a) = &c.agent {
        // (1) No start of any kind from here on, and a crashed node waiting for its restart stays down.
        a.hold_starts(true);
        a.abandon_restart();
        // (2) Only now is "no node" a fact: a restart that fired a moment ago shows up as a process or a pending start.
        let verdict = if c.switch_active() { Err(crate::agent::STOP_FIRST_MSG.to_string()) } else { a.confirm_stopped() };
        if let Err(msg) = verdict {
            a.hold_starts(false);
            return Err((409, json!({ "ok": false, "error": msg })));
        }
    }
    for n in nets {
        c.wiping.insert(n.id);
    }
    Ok(())
}

/// Second half of a wipe, with the console locked again after the delete (done or failed): starts are allowed
/// again, the folder facts are measured anew, and the chain data's heights and Latest Blocks history are
/// forgotten for each wiped network.
fn end_wipe(g: &mut crate::state::LiveState, root: &Path, nets: &[&'static Network]) {
    for n in nets {
        g.console.wiping.remove(n.id);
    }
    if g.console.wiping.is_empty() {
        if let Some(a) = &g.console.agent {
            a.hold_starts(false);
        }
    }
    for n in nets {
        g.console.refresh_net_disk(root, n);
        let addr = g.console.rpc_for(n);
        g.forget_chain(&addr, n.chain);
    }
    g.console.invalidate_disk();
}

/// Refusal for a mode change when the environment fixes the mode (the Umbrel app always runs its own node).
const MODE_LOCKED: &str = "This app always runs its own node, so it cannot be switched to watching another one. (The mode is fixed by BLVM_UI_MODE.)";
const JOB_RUNNING: &str = "The node is being installed or updated. Wait for that to finish.";

fn node_is_active(c: &Console) -> bool {
    c.switch_active() || c.agent.as_ref().is_some_and(|a| a.state().is_active())
}

/// Held from `begin_wipe` to `end_wipe`. If the section between them is left any other way (a panic, an early
/// return added later, a dropped future), its drop does what `end_wipe` does, in a task of its own because
/// dropping cannot wait for the console lock: `wiping` is cleared and starts are allowed again. On the normal path
/// [`WipeGuard::finish`] runs `end_wipe` and disarms it.
struct WipeGuard {
    state: Shared,
    root: PathBuf,
    nets: Vec<&'static Network>,
    armed: bool,
}

impl WipeGuard {
    fn new(state: &Shared, root: &Path, nets: &[&'static Network]) -> Self {
        Self { state: state.clone(), root: root.to_path_buf(), nets: nets.to_vec(), armed: true }
    }

    fn finish(mut self, g: &mut crate::state::LiveState) {
        end_wipe(g, &self.root, &self.nets);
        self.armed = false;
    }
}

impl Drop for WipeGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        tracing::error!("the chain data delete section was left without finishing; releasing the hold");
        let (state, root, nets) = (self.state.clone(), self.root.clone(), self.nets.clone());
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn(async move {
                    let mut g = state.write().await;
                    end_wipe(&mut g, &root, &nets);
                });
            }
            Err(_) => {
                if let Ok(mut g) = state.try_write() {
                    end_wipe(&mut g, &root, &nets);
                }
            }
        }
    }
}

/// Tests: panic in the wipe section of the request (after the delete, before `end_wipe`) for this data folder.
#[cfg(test)]
static PANIC_AFTER_WIPE: Mutex<std::collections::BTreeSet<PathBuf>> = Mutex::new(std::collections::BTreeSet::new());

/// Run a state-changing request to completion in a task of its own. hyper drops the handler's future when the
/// client goes away (a page reload, a closed tab, the page's own fetch timeout), and a future dropped between
/// `begin_wipe` and `end_wipe` would leave the console refusing everything ("being deleted") until it restarts and
/// the setting unsaved. The task finishes whatever happens to the handler; the next poll shows the result.
async fn run_to_completion<F>(job: F) -> (u16, Value)
where
    F: std::future::Future<Output = (u16, Value)> + Send + 'static,
{
    match tokio::spawn(job).await {
        Ok(out) => out,
        Err(e) => err(500, format!("The request could not be completed: {e}")),
    }
}

/// Settings after a wipe: the delete ran without the console's lock, so something may have been saved meanwhile
/// (a Stop writes `node_wanted`). Start from what is saved now and apply only the keys this request changed
/// (`patched` against `before`, the settings the request started from).
fn merge_after_wipe(now: &ConsoleSettings, before: &ConsoleSettings, patched: ConsoleSettings) -> ConsoleSettings {
    let (Ok(Value::Object(mut now_v)), Ok(Value::Object(before_v)), Ok(Value::Object(patched_v))) =
        (serde_json::to_value(now), serde_json::to_value(before), serde_json::to_value(&patched))
    else {
        return patched;
    };
    for (k, v) in &patched_v {
        if before_v.get(k) != Some(v) {
            now_v.insert(k.clone(), v.clone());
        }
    }
    for k in before_v.keys() {
        if !patched_v.contains_key(k) {
            now_v.remove(k);
        }
    }
    serde_json::from_value(Value::Object(now_v)).unwrap_or(patched)
}

/// Apply a patch to `console.json`. Returns `(http status, body)`.
pub async fn patch_settings(state: &Shared, body: &Value) -> (u16, Value) {
    let (state, body) = (state.clone(), body.clone());
    run_to_completion(async move { patch_settings_inner(&state, &body).await }).await
}

async fn patch_settings_inner(state: &Shared, body: &Value) -> (u16, Value) {
    let Some(obj) = body.as_object() else {
        return err(400, "Send a JSON object.");
    };
    let mut g = state.write().await;
    if !g.console.wiping.is_empty() {
        return err(409, crate::agent::WIPING_MSG);
    }
    let active = node_is_active(&g.console);
    let mut s = g.console.settings.clone();
    let before = s.clone();
    let before_rpc = g.console.initial_rpc();
    let mut needs_stopped = false;
    let confirm_wipe = obj.get("confirm_wipe").and_then(Value::as_bool).unwrap_or(false);
    let mut prune = PruneOutcome::default();
    // Pruning changes are judged after the loop, against the data folder this patch leaves in force.
    let mut prune_changes: Vec<(&'static Network, Option<u64>, Option<u64>)> = Vec::new();

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
            "show_logs" => match v.as_bool() {
                Some(b) => s.show_logs = b,
                None => return err(400, "show_logs must be true or false."),
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
            "assume_valid_by_network" => {
                let Some(map) = v.as_object() else {
                    return err(400, "assume_valid_by_network must map network ids to a value (or null to remove it).");
                };
                // A patch: listed networks change, the others keep their entry; `null` removes one.
                let mut out = s.assume_valid_by_network.clone();
                for (id, val) in map {
                    let Some(n) = networks::by_id(id) else {
                        return err(400, format!("assume_valid_by_network: unknown network \"{id}\"."));
                    };
                    if val.is_null() {
                        out.remove(n.id);
                    } else if let Some(a) = parse_assume_valid(val) {
                        out.insert(n.id.to_string(), a);
                    } else {
                        return err(400, format!("assume_valid_by_network.{id} {ASSUME_VALID_MSG}"));
                    }
                }
                s.assume_valid_by_network = out;
            }
            "prune_by_network" => {
                let Some(map) = v.as_object() else {
                    return err(400, "prune_by_network must map network ids to a number of blocks (or null to keep every block).");
                };
                // A patch: listed networks change, the others keep their entry; `null` removes one.
                let mut out = s.prune_by_network.clone();
                for (id, val) in map {
                    let Some(n) = networks::by_id(id) else {
                        return err(400, format!("prune_by_network: unknown network \"{id}\"."));
                    };
                    let Some(new) = parse_prune(val) else {
                        return err(400, prune_range_msg(&format!("prune_by_network.{id}")));
                    };
                    prune_changes.push((n, g.console.settings.prune_for(n.id), new));
                    match new {
                        Some(k) => out.insert(n.id.to_string(), k),
                        None => out.remove(n.id),
                    };
                }
                s.prune_by_network = out;
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
    // The data folder after this patch (a patch that moves it and removes pruning is judged on the new one).
    let new_root = settings::resolve_in(&s, &g.console.env, g.console.exe_dir.as_deref(), g.console.facts).data_root;
    for (n, old, new) in prune_changes {
        if let Err(refusal) = plan_prune_change(&new_root, n, old, new, confirm_wipe, &mut prune) {
            return refusal;
        }
    }
    if needs_stopped && active {
        return err(409, "Stop the node first, then change this setting.");
    }
    if needs_stopped && g.console.job_active() {
        return err(409, JOB_RUNNING);
    }
    // Deleting chain data needs the node stopped (and no install or update running).
    if !prune.wipe.is_empty() && active {
        return err(409, crate::agent::STOP_FIRST_MSG);
    }
    if !prune.wipe.is_empty() && g.console.job_active() {
        return err(409, JOB_RUNNING);
    }
    // With the agent in another container that has gone quiet nobody can say whether a node runs, and what is
    // written now would be carried out when the agent returns (the node on the new network, folder or program).
    if needs_stopped || !prune.wipe.is_empty() {
        if let Some(msg) = g.console.agent.as_ref().and_then(Agent::quiet_refusal) {
            return err(409, msg);
        }
    }
    // A crashed node waiting for its automatic restart counts as idle above, but it must not come back
    // on the new network, folder or binary: the change cancels the pending restart (S1 of the review).
    if needs_stopped {
        if let Some(a) = &g.console.agent {
            a.abandon_restart();
        }
    }
    if !prune.wipe.is_empty() {
        // Cancel a pending restart, look again that no node exists, mark the wipe; then delete without the
        // console's lock (a status poll must not wait for the delete) and finish with the lock again.
        if let Err(refusal) = begin_wipe(&mut g.console, &prune.wipe) {
            return refusal;
        }
        let guard = WipeGuard::new(state, &new_root, &prune.wipe);
        drop(g);
        let result = wipe_all(&new_root, &prune.wipe).await;
        g = state.write().await;
        #[cfg(test)]
        if PANIC_AFTER_WIPE.lock().unwrap().contains(&new_root) {
            panic!("test: the wipe section unwinds");
        }
        guard.finish(&mut g);
        if let Err(refusal) = result {
            return refusal;
        }
        s = merge_after_wipe(&g.console.settings, &before, s);
    }

    g.console.settings = s;
    if let Err(e) = g.console.save() {
        return err(500, format!("Could not save settings: {e}"));
    }
    g.console.reresolve();
    // An agent in another container reads `desired.json` when it starts a node by itself (after a crash or a
    // restart of its container): a saved value has to be in it already, but the running node is not restarted.
    if let Some(a) = &g.console.agent {
        a.sync_desired();
    }
    let after_rpc = g.console.initial_rpc();
    if after_rpc != before_rpc {
        g.switch_rpc(after_rpc);
    }
    stamp_power(&mut g);
    let mut body = settings_json(&g.console);
    prune.add_to(&mut body);
    (200, body)
}

// ---- POST /api/setup ------------------------------------------------------------------------

/// The wizard's result. Validates, writes `console.json`, creates the folders, and starts the node
/// only when asked to. A network is required for managed mode; there is no default.
pub async fn setup(state: &Shared, body: &Value) -> (u16, Value) {
    let (state, body) = (state.clone(), body.clone());
    run_to_completion(async move { setup_inner(&state, &body).await }).await
}

async fn setup_inner(state: &Shared, body: &Value) -> (u16, Value) {
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
    if !g.console.wiping.is_empty() {
        return err(409, crate::agent::WIPING_MSG);
    }
    if node_is_active(&g.console) {
        return err(409, "Stop the node before running setup again.");
    }
    if g.console.job_active() {
        return err(409, JOB_RUNNING);
    }
    let before_rpc = g.console.initial_rpc();
    let confirm_wipe = obj.get("confirm_wipe").and_then(Value::as_bool).unwrap_or(false);
    let mut prune = PruneOutcome::default();
    let mut wipe_root: Option<PathBuf> = None;
    let mut s = g.console.settings.clone();
    let before = s.clone();
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
            // Assume-valid for the chosen network ("default", "off" or a height; `null` clears the entry).
            match obj.get("assume_valid") {
                None => {}
                Some(Value::Null) => {
                    s.assume_valid_by_network.remove(net.id);
                }
                Some(v) => match parse_assume_valid(v) {
                    Some(a) => {
                        s.assume_valid_by_network.insert(net.id.to_string(), a);
                    }
                    None => return err(400, format!("assume_valid {ASSUME_VALID_MSG}")),
                },
            }
            // Block pruning for the chosen network: `null` keeps every block, a number keeps the newest blocks.
            // Left out: unchanged. The same rules as the Settings page apply to chain data already on disk.
            if let Some(v) = obj.get("prune_keep_blocks") {
                let Some(new) = parse_prune(v) else {
                    return err(400, prune_range_msg("prune_keep_blocks"));
                };
                let root = settings::resolve_in(&s, &g.console.env, g.console.exe_dir.as_deref(), g.console.facts).data_root;
                let old = s.prune_for(net.id);
                if let Err(refusal) = plan_prune_change(&root, net, old, new, confirm_wipe, &mut prune) {
                    return refusal;
                }
                match new {
                    Some(k) => s.prune_by_network.insert(net.id.to_string(), k),
                    None => s.prune_by_network.remove(net.id),
                };
                wipe_root = Some(root);
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

    // A quiet agent in another container (see `patch_settings`): another network, folder or program, or a
    // wipe, has to wait until it answers.
    if mode == Mode::Managed
        && (s.network != before.network || s.data_root != before.data_root || s.node_bin != before.node_bin || !prune.wipe.is_empty())
    {
        if let Some(msg) = g.console.agent.as_ref().and_then(Agent::quiet_refusal) {
            return err(409, msg);
        }
    }
    // The wizard decides what runs now: a pending automatic restart after a crash must not start a node
    // the user just said not to start (or on another network). Starting is done below, on request.
    if let Some(a) = &g.console.agent {
        a.abandon_restart();
    }
    if let (false, Some(root)) = (prune.wipe.is_empty(), &wipe_root) {
        // Same order as in `patch_settings`: look again that no node exists, delete without the lock.
        if let Err(refusal) = begin_wipe(&mut g.console, &prune.wipe) {
            return refusal;
        }
        let guard = WipeGuard::new(state, root, &prune.wipe);
        drop(g);
        let result = wipe_all(root, &prune.wipe).await;
        g = state.write().await;
        #[cfg(test)]
        if PANIC_AFTER_WIPE.lock().unwrap().contains(root) {
            panic!("test: the wipe section unwinds");
        }
        guard.finish(&mut g);
        if let Err(refusal) = result {
            return refusal;
        }
        s = merge_after_wipe(&g.console.settings, &before, s);
    }
    g.console.settings = s;
    if let Err(e) = g.console.save() {
        return err(500, format!("Could not save settings: {e}"));
    }
    g.console.reresolve();
    // A setup that does not start the node still has to reach an agent in another container (see
    // `patch_settings`); a start writes `desired.json` itself.
    if !start {
        if let Some(a) = &g.console.agent {
            a.sync_desired();
        }
    }
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
    let mut body = match start_error {
        None => json!({ "ok": true, "status": status }),
        Some(e) => json!({ "ok": false, "error": e, "status": status }),
    };
    prune.add_to(&mut body);
    (200, body)
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
    let (agent, has_network, switching, wiping) = {
        let g = state.read().await;
        (g.console.agent.clone(), g.console.network().is_some(), g.console.switch_active(), !g.console.wiping.is_empty())
    };
    let fail = |m: &str| NodeOutcome { ok: false, message: String::new(), error: Some(m.to_string()) };
    let Some(agent) = agent else {
        return fail("This console is not managing a node.");
    };
    if switching {
        return fail("A network switch is running. Wait for it to finish.");
    }
    if wiping && action != "stop" && action != "off" {
        return fail(crate::agent::WIPING_MSG);
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
    let (cache, channel, platform, record, pinned, managed, has_program, facts) = {
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
            c.node_facts(),
        )
    };
    let fetched = cache.get(force).await;
    let mut v = releases::info_json(&releases::InfoInputs {
        managed,
        channel: &channel,
        platform: &platform,
        installed: record.as_ref(),
        custom: record.is_none() && has_program,
        bin_pinned_by_env: pinned,
        fetched: &fetched,
    });
    // Whether the official release can start on the computer that would run the node (16 KB memory pages: no).
    // `can_install` stays as it is: the page shows the warning and holds the download back.
    if let Some(o) = v.as_object_mut() {
        o.insert("runs_here".into(), json!(facts.runs_here));
        o.insert("runs_here_reason".into(), json!(facts.reason));
        o.insert("page_size".into(), json!(facts.page_size));
    }
    v
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

    const FAKE: &str = "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\ntrap 'exit 0' TERM\necho up\nwhile kill -0 $PPID 2>/dev/null; do sleep 0.1; done\n";

    struct Fix {
        dir: PathBuf,
        state: Shared,
        _reap: crate::agent::testkit::ReapNodes,
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
        let mut console = Console::load(Some(dir.join("state")), env, None);
        crate::agent::testkit::isolate(&mut console); // free ports, never the networks' own
        let mut live = LiveState::new(console.initial_rpc());
        live.console = console;
        let _reap = crate::agent::testkit::ReapNodes::under(&dir);
        Fix { dir, state: Arc::new(RwLock::new(live)), _reap }
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
        assert_eq!(f.state.read().await.rpc_addr, crate::agent::testkit::rpc_of(&f.state, "regtest").await);
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
        assert_eq!(v["status"]["rpc_addr"], crate::agent::testkit::rpc_of(&f.state, "regtest").await);
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
        let target = format!("127.0.0.1:{}", crate::agent::testkit::free_port());
        let (code, v) = setup(&f.state, &json!({"mode":"standalone","rpc":target})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["status"]["mode"], "standalone");
        assert_eq!(v["status"]["managed"], Value::Null);
        assert_eq!(v["status"]["rpc_addr"], target.as_str());
        assert_eq!(settings::load_console(&f.dir.join("state")).rpc.as_deref(), Some(target.as_str()));
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
        assert_eq!(
            f.state.read().await.rpc_addr,
            crate::agent::testkit::rpc_of(&f.state, "signet").await,
            "polling follows the network"
        );
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn assume_valid_is_set_per_network_validated_and_resolved_into_the_agent_config() {
        let f = fixture("avmap", managed_env(), true);
        let (code, v) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200, "{v}");

        // The three read-only fields of the settings.
        assert_eq!(v["assume_valid_by_network"], json!({}));
        assert_eq!(v["assume_valid_defaults"]["mainnet"], json!({"height": 912683, "forced_off": false}));
        assert_eq!(v["assume_valid_defaults"]["testnet"], json!({"height": 4550000, "forced_off": false}));
        assert_eq!(v["assume_valid_defaults"]["signet"], json!({"height": 267665, "forced_off": false}));
        assert_eq!(v["assume_valid_defaults"]["testnet4"], json!({"height": 0, "forced_off": true}));
        assert_eq!(v["assume_valid_defaults"]["regtest"], json!({"height": 0, "forced_off": true}));
        assert_eq!(v["assume_valid_effective"], json!({"network":"regtest","value":null,"argv":["--noassumevalid"]}));

        // Refused: not an object, an unknown network, a bad value. The message names the culprit; nothing is saved.
        let before = settings::load_console(&f.dir.join("state"));
        for (bad, names) in [
            (json!({"assume_valid_by_network":"800000"}), "assume_valid_by_network"),
            (json!({"assume_valid_by_network":{"narnia":"800000"}}), "narnia"),
            (json!({"assume_valid_by_network":{"mainnet":"maybe"}}), "mainnet"),
            (json!({"assume_valid_by_network":{"mainnet":"1234567890"}}), "mainnet"),
            (json!({"assume_valid_by_network":{"mainnet":800000}}), "mainnet"),
            (json!({"assume_valid_by_network":{"signet":"1","mainnet":"x"}}), "mainnet"),
            // A block hash is only read from older files and the older global key: the node's hash form skips nothing.
            (json!({"assume_valid_by_network":{"mainnet":"ab".repeat(32)}}), "mainnet"),
            (json!({"assume_valid_by_network":{"signet":"1","mainnet":"AB".repeat(32)}}), "mainnet"),
        ] {
            let (code, v) = patch_settings(&f.state, &bad).await;
            assert_eq!(code, 400, "{bad} -> {v}");
            assert!(v["error"].as_str().unwrap().contains(names), "{bad} -> {v}");
        }
        assert_eq!(settings::load_console(&f.dir.join("state")), before, "a rejected patch changed nothing");
        // The older global key still takes a hash (files and scripts from before), and the card reads it back.
        let hash = "ab".repeat(32);
        let (code, v) = patch_settings(&f.state, &json!({"assume_valid": hash})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["assume_valid"], json!(hash));
        let (code, v) = patch_settings(&f.state, &json!({"assume_valid": null})).await;
        assert_eq!(code, 200, "{v}");
        let before = settings::load_console(&f.dir.join("state"));

        // The node reads BLVM_ASSUME_VALID_HEIGHT after the command line: the effective block says so.
        assert!(v["assume_valid_effective"].get("overridden_by_env").is_none(), "{v}");
        let (code, v) = patch_settings(&f.state, &json!({"child_env":{"BLVM_ASSUME_VALID_HEIGHT":"5"}})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["assume_valid_effective"]["overridden_by_env"], json!(true), "{v}");
        assert!(v["assume_valid_effective"]["note"].as_str().unwrap().contains("BLVM_ASSUME_VALID_HEIGHT"), "{v}");
        assert_eq!(v["assume_valid_effective"]["argv"], json!(["--noassumevalid"]), "the argv is still what the console passes");
        let (code, v) = patch_settings(&f.state, &json!({"child_env":{}})).await;
        assert_eq!(code, 200, "{v}");
        assert!(v["assume_valid_effective"].get("overridden_by_env").is_none(), "{v}");
        assert_eq!(settings::load_console(&f.dir.join("state")), before);

        // Saved, resolved into the agent's config for that network only, and no node needs to be stopped.
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.start().await.unwrap();
        let (code, v) = patch_settings(&f.state, &json!({"assume_valid_by_network":{"mainnet":"800000","regtest":"off"}})).await;
        assert_eq!(code, 200, "a running node does not block this setting: {v}");
        assert_eq!(v["assume_valid_by_network"], json!({"mainnet":"800000","regtest":"off"}));
        assert_eq!(v["assume_valid_effective"], json!({"network":"regtest","value":"off","argv":["--noassumevalid"]}));
        let saved = settings::load_console(&f.dir.join("state"));
        assert_eq!(saved.assume_valid_by_network.get("mainnet").map(String::as_str), Some("800000"));
        {
            let g = f.state.read().await;
            let cfg = |id| g.console.agent_config_for(networks::by_id(id));
            assert_eq!(cfg("mainnet").assume_valid.as_deref(), Some("800000"));
            assert_eq!(cfg("regtest").assume_valid.as_deref(), Some("off"));
            assert_eq!(cfg("signet").assume_valid, None, "no entry and no global: the network's default");
            assert_eq!(g.console.agent_config().assume_valid.as_deref(), Some("off"), "the current network's own value");
        }
        assert_eq!(agent.config().assume_valid.as_deref(), Some("off"), "the agent has it for its next start");

        // A patch only touches the networks it names; `null` removes an entry; the global one is the fallback.
        let (code, v) = patch_settings(&f.state, &json!({"assume_valid_by_network":{"mainnet":null,"signet":"270000"},"assume_valid":"700000"})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["assume_valid_by_network"], json!({"regtest":"off","signet":"270000"}));
        {
            let g = f.state.read().await;
            let cfg = |id| g.console.agent_config_for(networks::by_id(id));
            assert_eq!(cfg("mainnet").assume_valid.as_deref(), Some("700000"), "removed entry: the global value");
            assert_eq!(cfg("regtest").assume_valid.as_deref(), Some("off"), "an entry beats the global value");
        }
        let (code, v) = patch_settings(&f.state, &json!({"assume_valid_by_network":{"regtest":null}})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["assume_valid_effective"], json!({"network":"regtest","value":"700000","argv":["--assumevalid","700000"]}));
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// The Network and Settings pages print the port the node listens on. With the agent in another container
    /// that is the one the Umbrel app gave it through `BLVM_AGENT_P2P_PORT_<NET>`; everywhere else the network's own.
    #[tokio::test]
    async fn settings_and_network_rows_carry_the_p2p_port() {
        let _lock = networks::P2P_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for n in networks::ALL.iter() {
            std::env::remove_var(networks::p2p_env_name(n));
        }
        let defaults = json!({"mainnet":8333,"testnet4":48333,"testnet":18333,"signet":38333,"regtest":18444});
        let external = || Console::load(None, EnvOverrides { mode: Some("managed".into()), agent_external: true, ..Default::default() }, None);
        let in_process = || Console::load(None, managed_env(), None);
        let rows = |c: &Console| {
            let agent = c.agent.clone().unwrap();
            let v = serde_json::to_value(managed_view(c, agent.view(), &agent, None)).unwrap();
            v["networks"].as_array().unwrap().iter().map(|r| (r["id"].as_str().unwrap().to_string(), r["p2p_port"].clone())).collect::<Vec<_>>()
        };
        let default_rows: Vec<(String, Value)> = defaults.as_object().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let sorted = |mut r: Vec<(String, Value)>| {
            r.sort_by(|a, b| a.0.cmp(&b.0));
            r
        };

        // Nothing set: the networks' own ports, in the settings and in every row.
        assert_eq!(settings_json(&external())["p2p_ports"], defaults);
        assert_eq!(sorted(rows(&external())), sorted(default_rows.clone()));
        assert_eq!(rows(&external())[0], ("mainnet".to_string(), json!(8333)), "rows keep the table's order");

        // The Umbrel app moves mainnet (and only what it names); garbage is ignored.
        std::env::set_var("BLVM_AGENT_P2P_PORT_MAINNET", "8334");
        std::env::set_var("BLVM_AGENT_P2P_PORT_SIGNET", "nope");
        let mut moved = defaults.clone();
        moved["mainnet"] = json!(8334);
        let c = external();
        assert_eq!(settings_json(&c)["p2p_ports"], moved);
        let mut want = default_rows.clone();
        want[0].1 = json!(8334);
        assert_eq!(sorted(rows(&c)), sorted(want));
        // A console that starts its node itself does not move it: the variable is the agent's.
        let c = in_process();
        assert_eq!(settings_json(&c)["p2p_ports"], defaults);
        assert_eq!(sorted(rows(&c)), sorted(default_rows));
        std::env::remove_var("BLVM_AGENT_P2P_PORT_MAINNET");
        std::env::remove_var("BLVM_AGENT_P2P_PORT_SIGNET");
    }

    #[tokio::test]
    async fn setup_stores_assume_valid_under_the_chosen_network() {
        let f = fixture("avsetup", managed_env(), true);
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","assume_valid":"800000"})).await;
        assert_eq!(code, 200, "{v}");
        let saved = settings::load_console(&f.dir.join("state"));
        assert_eq!(saved.assume_valid_by_network.get("regtest").map(String::as_str), Some("800000"));
        assert_eq!(saved.assume_valid, None, "the global setting is not touched");
        {
            let g = f.state.read().await;
            assert_eq!(g.console.agent_config().assume_valid.as_deref(), Some("800000"));
        }

        // A bad value is refused and saves nothing.
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","assume_valid":"maybe"})).await;
        assert_eq!(code, 400, "{v}");
        assert_eq!(settings::load_console(&f.dir.join("state")), saved);
        // A block hash is refused here as well (only the older global key takes one).
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","assume_valid":"ab".repeat(32)})).await;
        assert_eq!(code, 400, "{v}");
        assert!(v["error"].as_str().unwrap().contains("assume_valid"), "{v}");
        assert_eq!(settings::load_console(&f.dir.join("state")), saved);

        // Another network gets its own entry; "default" is stored (it beats a global setting); null clears.
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"signet","assume_valid":"default"})).await;
        assert_eq!(code, 200, "{v}");
        let now = settings::load_console(&f.dir.join("state"));
        assert_eq!(now.assume_valid_by_network.get("regtest").map(String::as_str), Some("800000"));
        assert_eq!(now.assume_valid_by_network.get("signet").map(String::as_str), Some("default"));
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"signet","assume_valid":null})).await;
        assert_eq!(code, 200, "{v}");
        let now = settings::load_console(&f.dir.join("state"));
        assert!(!now.assume_valid_by_network.contains_key("signet"));
        assert!(now.assume_valid_by_network.contains_key("regtest"));
        // Left out: unchanged.
        let (code, _) = setup(&f.state, &json!({"mode":"managed","network":"regtest"})).await;
        assert_eq!(code, 200);
        assert_eq!(settings::load_console(&f.dir.join("state")).assume_valid_by_network, now.assume_valid_by_network);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// Put fake chain data and a `meta.json` under the regtest folder of `f`, as a node that has run pruned would.
    fn chain_data_on_disk(f: &Fix) -> PathBuf {
        chain_data_on_disk_ran(f, Some(true))
    }

    /// The same, with `ran_pruned` as given (`None`: a full store that no pruning node has ever touched).
    fn chain_data_on_disk_ran(f: &Fix, ran_pruned: Option<bool>) -> PathBuf {
        let net = networks::by_id("regtest").unwrap();
        let root = f.dir.join("root");
        let dir = networks::dir(&root, net);
        std::fs::create_dir_all(dir.join("data/heed3")).unwrap();
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::write(dir.join("data/heed3/data.mdb"), vec![7u8; 4096]).unwrap();
        std::fs::write(dir.join("blvm.toml"), "keep me").unwrap();
        std::fs::write(dir.join("logs/node.log"), "keep me too").unwrap();
        networks::write_meta(&root, net, &networks::Meta { created: Some(5), last_used: Some(6), last_height: Some(900), last_header: Some(901), ran_pruned }).unwrap();
        dir
    }

    #[tokio::test]
    async fn pruning_is_set_per_network_validated_and_resolved_into_the_agent_config() {
        let f = fixture("prunemap", managed_env(), true);
        let (code, v) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200, "{v}");

        // The read-only fields of the settings.
        assert_eq!(v["prune_by_network"], json!({}));
        assert_eq!(v["prune_min_blocks"], json!(288));
        assert_eq!(v["prune_effective"], json!({"network":"regtest","keep_blocks":null,"has_data":false,"has_pruned_data":false}));

        // Refused: not an object, an unknown network, below the minimum, above the maximum, not a whole number.
        // Nothing is saved, and the message names the culprit.
        let before = settings::load_console(&f.dir.join("state"));
        for (bad, names) in [
            (json!({"prune_by_network":"5000"}), "prune_by_network"),
            (json!({"prune_by_network":{"narnia":5000}}), "narnia"),
            (json!({"prune_by_network":{"mainnet":100}}), "mainnet"),
            (json!({"prune_by_network":{"mainnet":287}}), "mainnet"),
            (json!({"prune_by_network":{"mainnet":0}}), "mainnet"),
            (json!({"prune_by_network":{"mainnet":100_000_001u64}}), "mainnet"),
            (json!({"prune_by_network":{"mainnet":300.5}}), "mainnet"),
            (json!({"prune_by_network":{"mainnet":"5000"}}), "mainnet"),
            (json!({"prune_by_network":{"mainnet":-5}}), "mainnet"),
            (json!({"prune_by_network":{"signet":5000,"mainnet":287}}), "mainnet"),
        ] {
            let (code, v) = patch_settings(&f.state, &bad).await;
            assert_eq!(code, 400, "{bad} -> {v}");
            assert!(v["error"].as_str().unwrap().contains(names), "{bad} -> {v}");
        }
        assert_eq!(settings::load_console(&f.dir.join("state")), before, "a rejected patch changed nothing");

        // 288 and 100 million are the edges that work. No node needs to be stopped for it.
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.start().await.unwrap();
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"mainnet":100_000_000u64,"regtest":288}})).await;
        assert_eq!(code, 200, "a running node does not block this setting: {v}");
        assert_eq!(v["prune_by_network"], json!({"mainnet":100_000_000u64,"regtest":288}));
        assert_eq!(v["prune_effective"], json!({"network":"regtest","keep_blocks":288,"has_data":false,"has_pruned_data":false}));
        assert!(v.get("warning").is_none(), "no chain data, nothing to convert: {v}");
        let saved = settings::load_console(&f.dir.join("state"));
        assert_eq!(saved.prune_by_network.get("regtest"), Some(&288));
        {
            let g = f.state.read().await;
            let cfg = |id| g.console.agent_config_for(networks::by_id(id));
            assert_eq!(cfg("regtest").prune_keep_blocks, Some(288));
            assert_eq!(cfg("mainnet").prune_keep_blocks, Some(100_000_000));
            assert_eq!(cfg("signet").prune_keep_blocks, None, "no entry: every block");
            assert_eq!(g.console.agent_config().prune_keep_blocks, Some(288), "the current network's own value");
            assert_eq!(g.console.agent_config_for(None).prune_keep_blocks, None);
        }
        assert_eq!(agent.config().prune_keep_blocks, Some(288), "the agent has it for its next start");

        // The status carries the per-network setting too (the Settings page's Storage list).
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
            let rows = &g.snapshot.managed.as_ref().unwrap().networks;
            assert_eq!(rows.iter().find(|r| r.id == "regtest").unwrap().prune_keep_blocks, Some(288));
            assert_eq!(rows.iter().find(|r| r.id == "signet").unwrap().prune_keep_blocks, None);
            assert!(g.snapshot.pruning.enabled && g.snapshot.pruning.keep_blocks == Some(288));
        }

        // A patch only touches the networks it names; null removes an entry (no chain data: no questions).
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"mainnet":null,"signet":1000}})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(v["prune_by_network"], json!({"regtest":288,"signet":1000}));
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn going_from_pruned_back_to_a_full_node_with_chain_data_needs_a_confirmed_wipe() {
        let f = fixture("prunewipe", managed_env(), true);
        let (code, v) = patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200, "{v}");
        let net_dir = chain_data_on_disk(&f);
        // The live folder decides, not the cached sizes.
        let (_, v) = patch_settings(&f.state, &json!({})).await;
        assert_eq!(v["prune_effective"]["has_data"], json!(true), "{v}");

        // Refused without the confirmation, with the machine-readable reason, and nothing changes.
        let before = settings::load_console(&f.dir.join("state"));
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null}})).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["needs"], json!("wipe"), "{v}");
        assert!(v["error"].as_str().unwrap().contains("Regtest"), "{v}");
        assert_eq!(settings::load_console(&f.dir.join("state")), before);
        assert!(net_dir.join("data/heed3/data.mdb").is_file(), "nothing was deleted");
        // Confirming without a removal does nothing.
        let (code, v) = patch_settings(&f.state, &json!({"confirm_wipe":true,"show_logs":true})).await;
        assert_eq!(code, 200, "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file());

        // Confirmed, but the node runs: stop it first. Still nothing deleted, nothing saved.
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.start().await.unwrap();
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await;
        assert_eq!(code, 409, "{v}");
        assert!(v["error"].as_str().unwrap().contains("Stop the node first"), "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file(), "a running node's data is never deleted");
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_for("regtest"), Some(300));
        agent.stop_and_wait().await;

        // Stopped and confirmed: the chain data goes, and only the chain data. Heights are forgotten, the setting is saved.
        // (The start above wrote `blvm.toml` and the log; whatever they hold now must survive.)
        let toml_before = std::fs::read_to_string(net_dir.join("blvm.toml")).unwrap();
        let log_before = std::fs::read_to_string(net_dir.join("logs/node.log")).unwrap();
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists(), "the data folder is gone");
        assert_eq!(std::fs::read_to_string(net_dir.join("blvm.toml")).unwrap(), toml_before);
        assert_eq!(std::fs::read_to_string(net_dir.join("logs/node.log")).unwrap(), log_before);
        let meta = networks::read_meta(&f.dir.join("root"), networks::by_id("regtest").unwrap()).unwrap();
        // `created` and `last_used` stay (the node ran in between and refreshed the latter); the heights are forgotten.
        assert_eq!((meta.created, meta.last_height, meta.last_header), (Some(5), None, None));
        assert!(meta.last_used.is_some());
        assert_eq!(v["prune_by_network"], json!({}));
        assert_eq!(v["prune_effective"], json!({"network":"regtest","keep_blocks":null,"has_data":false,"has_pruned_data":false}));
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_for("regtest"), None);
        {
            let g = f.state.read().await;
            assert_eq!(g.console.agent_config().prune_keep_blocks, None);
        }
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn adding_or_lowering_pruning_over_chain_data_warns_and_raising_it_only_notes() {
        let f = fixture("prunewarn", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        chain_data_on_disk(&f);

        // Switching pruning on over an existing store: allowed, with the warning about the slow conversion.
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":10_000}})).await;
        assert_eq!(code, 200, "{v}");
        let w = v["warning"].as_str().unwrap();
        assert!(w.starts_with("The node converts the existing store on its next start"), "{w}");
        assert!(w.contains("does not answer meanwhile") && w.contains("Minutes on Testnet4, possibly hours on mainnet."), "{w}");
        assert!(v.get("note").is_none());
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_for("regtest"), Some(10_000), "saved all the same");

        // Lowering it: the same warning.
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":2_000}})).await;
        assert_eq!(code, 200, "{v}");
        assert!(v["warning"].as_str().unwrap().contains("converts the existing store"), "{v}");

        // Raising it: no warning, a note that deleted blocks stay deleted.
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":5_000}})).await;
        assert_eq!(code, 200, "{v}");
        assert!(v.get("warning").is_none(), "{v}");
        assert!(v["note"].as_str().unwrap().contains("do not come back"), "{v}");

        // The same value again, or any other setting: neither.
        let (_, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":5_000}})).await;
        assert!(v.get("warning").is_none() && v.get("note").is_none(), "{v}");
        let (_, v) = patch_settings(&f.state, &json!({"show_logs":true})).await;
        assert!(v.get("warning").is_none() && v.get("note").is_none(), "{v}");

        // Without chain data there is nothing to convert: no warning for a network that has none.
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"signet":1_000}})).await;
        assert_eq!(code, 200, "{v}");
        assert!(v.get("warning").is_none(), "{v}");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn setup_stores_the_pruning_choice_under_the_chosen_network_with_the_same_guards() {
        let f = fixture("prunesetup", managed_env(), true);
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":5000})).await;
        assert_eq!(code, 200, "{v}");
        assert!(v.get("warning").is_none(), "{v}");
        let saved = settings::load_console(&f.dir.join("state"));
        assert_eq!(saved.prune_by_network.get("regtest"), Some(&5000));
        {
            let g = f.state.read().await;
            assert_eq!(g.console.agent_config().prune_keep_blocks, Some(5000));
        }

        // Bad values are refused and save nothing (including a missing network entry for a network that has none).
        for bad in [json!(100), json!(287), json!("5000"), json!(1.5), json!(100_000_001u64)] {
            let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"signet","prune_keep_blocks":bad})).await;
            assert_eq!(code, 400, "{bad} -> {v}");
            assert!(v["error"].as_str().unwrap().contains("prune_keep_blocks"), "{v}");
        }
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_by_network, saved.prune_by_network);

        // Left out: unchanged. Another network gets its own entry. null removes the entry (no data: allowed).
        let (code, _) = setup(&f.state, &json!({"mode":"managed","network":"regtest"})).await;
        assert_eq!(code, 200);
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_by_network, saved.prune_by_network);
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"signet","prune_keep_blocks":288})).await;
        assert_eq!(code, 200, "{v}");
        let now = settings::load_console(&f.dir.join("state"));
        assert_eq!(now.prune_by_network.get("signet"), Some(&288));
        assert_eq!(now.prune_by_network.get("regtest"), Some(&5000));
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"signet","prune_keep_blocks":null})).await;
        assert_eq!(code, 200, "{v}");
        assert!(!settings::load_console(&f.dir.join("state")).prune_by_network.contains_key("signet"));

        // A re-run over chain data: switching pruning on warns; back to a full node is refused until confirmed.
        let net_dir = chain_data_on_disk(&f);
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":1000})).await;
        assert_eq!(code, 200, "{v}");
        assert!(v["warning"].as_str().unwrap().contains("converts the existing store"), "{v}");
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":null})).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["needs"], json!("wipe"));
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_for("regtest"), Some(1000));
        assert!(net_dir.join("data/heed3/data.mdb").is_file());
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":null,"confirm_wipe":true})).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_for("regtest"), None);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn a_node_started_through_the_console_gets_the_pruning_block_in_its_config() {
        let f = fixture("prune-start", managed_env(), true);
        let (code, v) = patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200, "{v}");
        let out = node_action(&f.state, "start").await;
        assert!(out.ok, "{out:?}");
        let agent = f.state.read().await.console.agent.clone().unwrap();
        for _ in 0..200 {
            if agent.logs(None, 50).iter().any(|l| l.text.starts_with("up")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let file = f.dir.join("root/networks/regtest/blvm.toml");
        let toml = std::fs::read_to_string(&file).unwrap();
        assert!(toml.contains("[storage.pruning]\nauto_prune = true\n") && toml.contains("min_recent_blocks = 300\n"), "{toml}");
        assert!(toml.contains("[rpc_auth]\nrequired = true"), "the generated login is there too: {toml}");
        let log: String = agent.logs(None, 200).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n");
        assert!(log.contains("--config ./blvm.toml"), "{log}");
        agent.stop_and_wait().await;
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

    #[test]
    fn modules_pages_follow_enabled_modules() {
        assert!(modules_pages_for::<String>(&[]).is_empty());
        assert_eq!(modules_pages_for(&["blvm-stratum-v2"]), vec!["mining"]);
        assert_eq!(modules_pages_for(&["blvm-datum", "blvm-stratum-v2", "blvm-gridpool"]), vec!["mining"], "one page, once");
        assert!(modules_pages_for(&["blvm-zmq", "blvm-fibre", "something-else"]).is_empty());
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
        assert_eq!(v["modules_pages"], json!([]));
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
        for ok in ["127.0.0.1:4242", "localhost:4243", "my-node.local:1", "[::1]:4242", "host_name:65535"] {
            assert!(valid_rpc_addr(ok), "{ok}");
        }
        for bad in ["", "4242", "host", "host:", "host:0", "host:70000", ":4242", "http://h:1", "a b:1", "h:1/x", "[::1:4242"] {
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
        crash_once_on_regtest_pruned(tag, None).await
    }

    /// The same, with block pruning set for regtest before the node starts (so the start records `ran_pruned`).
    async fn crash_once_on_regtest_pruned(tag: &str, prune: Option<u64>) -> Fix {
        let f = fixture(tag, managed_env(), true);
        std::fs::write(f.dir.join("fake-blvm.sh"), CRASHER).unwrap();
        let (code, _) = match prune {
            Some(k) => patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":k}})).await,
            None => patch_settings(&f.state, &json!({"network":"regtest"})).await,
        };
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
            // Something answers on regtest's RPC address (another node), so the last poll was live.
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
        // An external agent (another container) is not started from here: its tests keep the network's own port.
        let c = load_in(&dir, env);
        (dir, c)
    }

    /// A console on `dir`'s saved state, as the next run of the app loads it: on free ports (the networks' own
    /// stay free for the person running the tests), unless its agent is another container's.
    fn load_in(dir: &Path, env: EnvOverrides) -> Console {
        let external = env.agent_external;
        let mut c = Console::load(Some(dir.join("state")), env, None);
        if !external {
            crate::agent::testkit::isolate(&mut c);
        }
        c
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
        assert_eq!(c.initial_rpc(), c.rpc_for(networks::by_id("regtest").unwrap()));
        assert!(c.initial_rpc().starts_with("127.0.0.1:"));
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
        let state2 = shared(load_in(&dir, env));
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
        let state = shared(load_in(&dir, env));
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
        assert_eq!(c.initial_rpc(), format!("node.internal:{}", networks::by_id("regtest").unwrap().rpc_port));
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
        let port = regtest.rpc_port;
        assert_eq!(c.rpc_for(regtest), format!("127.0.0.1:{port}"), "in-process: loopback");
        let c = Console::load(None, EnvOverrides { agent_external: true, ..base.clone() }, None);
        assert_eq!(c.rpc_for(regtest), format!("btc-commons-node-view_agent_1:{port}"));
        let c = Console::load(None, EnvOverrides { agent_external: true, node_host: Some("agent.local".into()), ..base.clone() }, None);
        assert_eq!(c.rpc_for(regtest), format!("agent.local:{port}"));
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
    async fn the_logs_page_is_off_until_the_user_turns_it_on_and_that_is_remembered() {
        assert!(!ConsoleSettings::default().show_logs, "off by default");
        let old: ConsoleSettings = serde_json::from_str(r#"{"setup_done":true}"#).unwrap();
        assert!(!old.show_logs, "an older file without the field: off");
        let f = fixture("show-logs", managed_env(), true);
        assert_eq!(settings_json(&f.state.read().await.console)["show_logs"], false);
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
        }
        let v = serde_json::to_value(&f.state.read().await.snapshot).unwrap();
        assert_eq!(v["show_logs"], false, "status carries it, off");
        let (c, v) = patch_settings(&f.state, &json!({"show_logs": "yes"})).await;
        assert_eq!(c, 400, "{v}");
        let (c, v) = patch_settings(&f.state, &json!({"show_logs": true})).await;
        assert_eq!((c, v["show_logs"].clone()), (200, json!(true)));
        assert!(settings::load_console(&f.dir.join("state")).show_logs, "saved to console.json");
        let v = serde_json::to_value(&f.state.read().await.snapshot).unwrap();
        assert_eq!(v["show_logs"], true, "status follows the switch at once");
        let (_, v) = patch_settings(&f.state, &json!({"show_logs": false})).await;
        assert_eq!(v["show_logs"], false);
        assert!(!settings::load_console(&f.dir.join("state")).show_logs);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[tokio::test]
    async fn managed_chain_size_is_the_measured_folder_not_the_nodes_estimate() {
        let f = fixture("disk-measured", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        // Real bytes in the network's folder: 3 MB of chain data.
        let data = f.dir.join("root/networks/regtest/data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("blocks.bin"), vec![7u8; 3 * 1024 * 1024]).unwrap();
        let (cache, root) = f.state.read().await.console.disk_job().expect("a measurement is due");
        run_disk_job(cache, root);
        let measured = f.state.read().await.console.disk_snapshot().lock().unwrap().net("regtest").bytes;
        assert!(measured >= 3 * 1024 * 1024, "{measured}");
        // The node says 27 GB (about 1 MB a block at 27 000 blocks), a made-up figure.
        {
            let mut g = f.state.write().await;
            g.snapshot.disk_used_bytes = 27_000_000_000;
            g.snapshot.disk_used_num = "27.0".into();
            g.snapshot.disk_used_unit = "GB".into();
            g.snapshot.disk_used_label = "27.0 GB".into();
            stamp_power(&mut g);
        }
        let s = f.state.read().await.snapshot.clone();
        assert_eq!(s.disk_used_bytes, measured, "the folder, not the node's number");
        assert_eq!(s.disk_used_source, "folder");
        assert_ne!(s.disk_used_label, "27.0 GB");
        let (total, free) = crate::state::volume_space_at(&f.dir.join("root"));
        assert!(total > 0);
        assert_eq!(s.disk_total_bytes, total, "the volume that holds the data root");
        assert!(s.disk_free_bytes.abs_diff(free) < 1 << 30);
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["disk_used_source"], "folder");
        let _ = std::fs::remove_dir_all(&f.dir);

        // Before the first measurement: no figure, and never the node's.
        let g = fixture("disk-unmeasured", managed_env(), true);
        let (code, _) = patch_settings(&g.state, &json!({"network":"regtest"})).await;
        assert_eq!(code, 200);
        {
            let mut w = g.state.write().await;
            w.snapshot.disk_used_bytes = 27_000_000_000;
            stamp_power(&mut w);
        }
        assert_eq!(g.state.read().await.snapshot.disk_used_bytes, 0);
        let _ = std::fs::remove_dir_all(&g.dir);
    }

    #[tokio::test]
    async fn standalone_keeps_the_nodes_figure_and_says_so() {
        let f = fixture("disk-standalone", EnvOverrides::default(), false);
        {
            let mut g = f.state.write().await;
            g.snapshot.disk_used_bytes = 5_000_000_000;
            stamp_power(&mut g);
        }
        let s = f.state.read().await.snapshot.clone();
        assert_eq!((s.disk_used_bytes, s.disk_used_source), (5_000_000_000, "node"));
        let _ = std::fs::remove_dir_all(&f.dir);
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
        let state = shared(load_in(&dir, env));
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
        let state2 = shared(load_in(&dir, reload_env(&dir)));
        assert!(state2.read().await.console.settings.start_on_boot_effective(true));
        boot(&state2).await;
        let agent2 = state2.read().await.console.agent.clone().unwrap();
        assert_eq!(agent2.state(), ProcState::Stopped, "a restart does not undo Stop");

        // The user starts it, the app restarts, and the node comes back.
        let out = node_action(&state2, "start").await;
        assert!(out.ok, "{out:?}");
        assert_eq!(console_json(&dir).await.node_wanted, Some(true));
        agent2.stop_and_wait().await; // (the process goes, the wish stays: a quit is not a Stop)
        let state3 = shared(load_in(&dir, reload_env(&dir)));
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
        let state4 = shared(load_in(&dir, reload_env(&dir)));
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

    /// Review R2: a desktop install from before nodes had a login. A node is running with open RPC and
    /// `console.json` holds no login; the new console generates one at load.
    #[tokio::test]
    async fn an_old_install_gets_its_open_node_restarted_with_a_password_and_the_login_saved() {
        let (rpc_port, p2p) = crate::agent::testkit::free_port_pair();
        let rpc = format!("127.0.0.1:{rpc_port}");
        let dir = std::env::temp_dir().join(format!("blvm-managed-r2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _reap = crate::agent::testkit::ReapNodes::under(&dir);
        // The node is a program that listens on the RPC port: open until `./blvm.toml` exists next to it, then
        // asking for a login (what the real node does with the config file; see `stub_node_server`).
        let bin = crate::agent::testkit::write_stub_node(&dir);
        let stub_env: BTreeMap<String, String> = [
            ("BLVM_TEST_STUB_ADDR".to_string(), rpc.clone()),
            ("BLVM_TEST_STUB_STATUS".to_string(), "by-config".to_string()),
        ]
        .into();
        let root = dir.join("root");
        let regtest = networks::by_id("regtest").unwrap();
        let toml = networks::dir(&root, regtest).join("blvm.toml");
        // console.json as the old version left it: set up, a network, no login.
        let mut old = ConsoleSettings { setup_done: true, mode: Some("managed".into()), network: Some("regtest".into()), ..Default::default() };
        old.rpc_overrides.insert("regtest".into(), rpc.clone());
        old.child_env = stub_env.clone();
        settings::save_console(&dir.join("state"), &old).unwrap();
        assert_eq!(settings::load_console(&dir.join("state")).rpc_pass, None);
        // The node the old console started (it has no login) and that outlived it.
        let old_cfg = AgentConfig {
            bin: Some(bin.clone()),
            root: root.clone(),
            network: Some(regtest),
            rpc_addr: Some(rpc.clone()),
            assume_valid: None,
            prune_keep_blocks: None,
            child_env: stub_env,
            rpc_auth: None,
            p2p_port: Some(p2p),
            p2p_listen_port: None,
        };
        let old_agent = Agent::new(old_cfg.clone(), Timings::default());
        old_agent.start().await.unwrap();
        let old_pid = old_agent.view().pid.unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while Instant::now() < end && std::net::TcpStream::connect(&rpc).is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(std::net::TcpStream::connect(&rpc).is_ok(), "the node listens");
        old_agent.detach();

        // The new console.
        let env = EnvOverrides {
            data_root: Some(root.display().to_string()),
            node_bin: Some(bin.display().to_string()),
            ..Default::default()
        };
        let console = Console::load(Some(dir.join("state")), env, None);
        assert!(console.login_is_new(), "generated at load, not saved yet");
        let auth = console.rpc_auth().unwrap();
        let state = shared(console);
        let agent = state.read().await.console.agent.clone().unwrap();
        agent.set_config(AgentConfig { p2p_port: Some(p2p), ..agent.config() });
        agent.rpc_flag().store(true, std::sync::atomic::Ordering::Relaxed); // the poller reads the open node fine
        boot(&state).await;

        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            // (A start resets the flag, as it should; the real poller sets it again within a second.)
            agent.rpc_flag().store(true, std::sync::atomic::Ordering::Relaxed);
            let v = agent.view();
            if v.state == ProcState::Running && v.pid.is_some_and(|p| p != old_pid) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let v = agent.view();
        assert!(v.state == ProcState::Running && v.pid.is_some_and(|p| p != old_pid), "restarted: {v:?}");
        assert!(!crate::agent::pid_alive(old_pid), "the open node is gone");
        assert_eq!(v.notice.as_deref(), Some("Restarted the node to turn on its RPC password."));
        assert_eq!(v.last_error, None, "not shown as a crash");
        // The login that the node got is the one that was saved.
        let saved = console_json(&dir).await;
        assert_eq!((saved.rpc_user.as_deref(), saved.rpc_pass.as_deref()), (Some(auth.user.as_str()), Some(auth.pass.as_str())));
        let file = std::fs::read_to_string(&toml).unwrap();
        assert!(file.contains(&format!("password = \"{}\"", auth.pass)), "the node's config has it");
        let log = std::fs::read_to_string(networks::log_file(&root, regtest)).unwrap();
        assert!(log.contains("[console] Restarted the node to turn on its RPC password"), "{log}");
        // The page sees the note, and never the password.
        {
            let mut g = state.write().await;
            stamp_power(&mut g);
        }
        let status = serde_json::to_string(&state.read().await.snapshot).unwrap();
        assert!(status.contains("Restarted the node to turn on its RPC password."), "{status}");
        assert!(!status.contains(&auth.pass));
        // The next run of the console finds the login in the file and has nothing more to do.
        let again = Console::load(Some(dir.join("state")), EnvOverrides { data_root: Some(root.display().to_string()), ..Default::default() }, None);
        assert!(!again.login_is_new());
        assert_eq!(again.rpc_auth(), Some(auth));
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&dir);
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
        let state = shared(load_in(&dir, env));
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
        let again = load_in(&dir, reload_env(&dir));
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

    // ---- fixes for the pruning review (S1, S2, N2, N3, N7, N10) ----

    fn regtest() -> &'static Network {
        networks::by_id("regtest").unwrap()
    }

    /// A pending automatic restart after a crash must not survive a wipe: the wipe cancels it first, deletes, and
    /// the node never comes back on the deleted data. The start that recorded `ran_pruned` is what makes the
    /// removal of the setting a wipe at all.
    #[tokio::test]
    async fn s1_a_pending_crash_restart_is_cancelled_before_the_chain_data_is_deleted() {
        let f = crash_once_on_regtest_pruned("f-s1-timer", Some(300)).await;
        let root = f.dir.join("root");
        let net_dir = networks::dir(&root, regtest());
        assert_eq!(networks::read_meta(&root, regtest()).unwrap().ran_pruned, Some(true), "the start recorded it");
        std::fs::create_dir_all(net_dir.join("data/heed3")).unwrap();
        std::fs::write(net_dir.join("data/heed3/data.mdb"), vec![7u8; 4096]).unwrap();
        let agent = f.state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent.state(), ProcState::Crashed);
        let starts = || std::fs::read_to_string(net_dir.join("logs/node.log")).unwrap_or_default().matches("[console] starting").count();
        assert_eq!(starts(), 1);

        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        // Past the 5 s backoff: the cancelled restart did not run, so nothing was started on (or created in) the folder.
        tokio::time::sleep(Duration::from_millis(6500)).await;
        assert!(!agent.state().is_active(), "{:?}", agent.view());
        assert_eq!(starts(), 1, "no second start");
        assert!(!net_dir.join("data").exists(), "the data folder was not recreated by a node");
        assert_eq!(networks::read_meta(&root, regtest()).unwrap().ran_pruned, None, "the wipe clears it");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// While chain data is being deleted nothing may start a node, not even a restart that is already queued.
    #[tokio::test]
    async fn s1_no_start_of_any_kind_while_the_hold_is_on_and_a_queued_restart_stays_down() {
        let f = crash_once_on_regtest("f-s1-hold").await;
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.hold_starts(true);
        assert_eq!(agent.start().await.unwrap_err(), crate::agent::WIPING_MSG);
        assert_eq!(agent.restart().await.unwrap_err(), crate::agent::WIPING_MSG);
        let out = {
            f.state.write().await.console.wiping.insert("regtest");
            node_action(&f.state, "start").await
        };
        assert_eq!(out.error.as_deref(), Some(crate::agent::WIPING_MSG));
        // The timer fires (5 s) and finds the hold: still crashed, nothing running.
        tokio::time::sleep(Duration::from_millis(6500)).await;
        assert_eq!(agent.state(), ProcState::Crashed, "{:?}", agent.view());
        // A status poll and the settings page are not blocked, and say what is going on.
        {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
            let row = g.snapshot.managed.as_ref().unwrap().networks.iter().find(|r| r.id == "regtest").unwrap().clone();
            assert!(row.wiping);
        }
        let (code, v) = patch_settings(&f.state, &json!({"show_logs":true})).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["error"], json!(crate::agent::WIPING_MSG));
        let (code, _) = setup(&f.state, &json!({"mode":"managed","network":"regtest"})).await;
        assert_eq!(code, 409);
        // Let go: the node can be started again.
        f.state.write().await.console.wiping.clear();
        agent.hold_starts(false);
        assert!(node_action(&f.state, "start").await.ok);
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// The delete runs without the console's lock: between the two halves of a wipe the state can be read and
    /// written, and the row says `wiping`.
    #[tokio::test]
    async fn n7_the_console_is_not_locked_while_the_chain_data_is_deleted() {
        let f = fixture("f-n7", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        let net_dir = chain_data_on_disk(&f);
        let root = f.dir.join("root");
        {
            let mut g = f.state.write().await;
            begin_wipe(&mut g.console, &[regtest()]).unwrap();
            assert!(g.console.wiping.contains("regtest"));
        } // lock released, as the delete runs
        let status = tokio::time::timeout(Duration::from_secs(2), async {
            let mut g = f.state.write().await;
            stamp_power(&mut g);
            g.snapshot.managed.clone().unwrap()
        })
        .await
        .expect("a status poll does not wait for the delete");
        assert!(status.networks.iter().find(|r| r.id == "regtest").unwrap().wiping);
        wipe_all(&root, &[regtest()]).await.unwrap();
        assert!(!net_dir.join("data").exists());
        {
            let mut g = f.state.write().await;
            end_wipe(&mut g, &root, &[regtest()]);
            assert!(g.console.wiping.is_empty());
            stamp_power(&mut g);
            assert!(!g.snapshot.managed.as_ref().unwrap().networks.iter().find(|r| r.id == "regtest").unwrap().wiping);
        }
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// With the agent in another container the console cannot see the process: it needs a fresh heartbeat and a
    /// `state.json` that has caught up with its last request.
    #[tokio::test]
    async fn s1_in_external_mode_a_wipe_needs_a_live_agent_that_has_confirmed_the_stop() {
        let dir = std::env::temp_dir().join(format!("blvm-f-s1-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        let env = EnvOverrides { mode: Some("managed".into()), agent_external: true, data_root: Some(root.display().to_string()), ..Default::default() };
        let state = shared(load_in(&dir, env));
        let (code, v) = patch_settings(&state, &json!({"network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200, "{v}");
        let net_dir = networks::dir(&root, regtest());
        std::fs::create_dir_all(net_dir.join("data/heed3")).unwrap();
        std::fs::write(net_dir.join("data/heed3/data.mdb"), vec![7u8; 4096]).unwrap();
        networks::write_meta(&root, regtest(), &networks::Meta { ran_pruned: Some(true), ..Default::default() }).unwrap();
        let wipe = json!({"prune_by_network":{"regtest":null},"confirm_wipe":true});
        let state_file = |heartbeat: u64, generation: u64, st: &str| {
            let f = control::StateFile { heartbeat, generation, state: st.into(), ..Default::default() };
            control::write_state(&root, &f).unwrap();
        };
        let settle = || tokio::time::sleep(Duration::from_millis(300)); // (state.json is cached for 200 ms)
        let now = || crate::persist::unix_now();

        // No agent has ever written: the console cannot tell whether a node runs.
        let (code, v) = patch_settings(&state, &wipe).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["error"], json!("The node agent is not answering; try again when it is back."));
        assert!(net_dir.join("data/heed3/data.mdb").is_file());

        // A heartbeat that went stale says "stopped" too, but nobody vouches for it: the node may be running.
        state_file(now() - 120, 0, "stopped");
        settle().await;
        let (code, v) = patch_settings(&state, &wipe).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["error"], json!("The node agent is not answering; try again when it is back."));
        assert!(net_dir.join("data/heed3/data.mdb").is_file());
        assert_eq!(settings::load_console(&dir.join("state")).prune_for("regtest"), Some(300), "nothing saved");
        // The refusal asked the node for nothing: for a silent agent "crashed" is only a guess, and a request to
        // stay off would stop a running node when the agent comes back.
        assert!(control::read_desired(&root).is_none(), "desired.json was not written");

        // Alive, but the agent has not dealt with the console's last request (start, then stop: generation 2).
        let agent = state.read().await.console.agent.clone().unwrap();
        agent.start().await.unwrap();
        agent.request_stop();
        state_file(now(), 0, "stopped");
        settle().await;
        let (code, v) = patch_settings(&state, &wipe).await;
        assert_eq!(code, 409, "{v}");
        assert!(v["error"].as_str().unwrap().contains("Stop the node first"), "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file());

        // The agent runs the node: refused as before.
        state_file(now(), 2, "running");
        settle().await;
        let (code, _) = patch_settings(&state, &wipe).await;
        assert_eq!(code, 409);

        // Alive, caught up, stopped: the chain data goes.
        state_file(now(), 2, "stopped");
        settle().await;
        let (code, v) = patch_settings(&state, &wipe).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        assert_eq!(settings::load_console(&dir.join("state")).prune_for("regtest"), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A pruning setting that was saved and never used leaves a full store untouched: taking it back is a plain
    /// save. Once a node has been started pruned on the folder, going back to a full node needs the wipe.
    #[tokio::test]
    async fn s2_taking_back_a_pruning_setting_that_never_ran_is_a_plain_save() {
        let f = fixture("f-s2", managed_env(), true);
        let (code, v) = patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":5_000}})).await;
        assert_eq!(code, 200, "{v}");
        let net_dir = chain_data_on_disk_ran(&f, None); // a full store: no pruning node ever ran on it
        let (_, v) = patch_settings(&f.state, &json!({})).await;
        assert_eq!(v["prune_effective"]["has_data"], json!(true), "{v}");
        assert_eq!(v["prune_effective"]["has_pruned_data"], json!(false), "{v}");

        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null}})).await;
        assert_eq!(code, 200, "no question asked: {v}");
        assert!(v.get("needs").is_none() && v.get("warning").is_none(), "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file(), "the full store is untouched");
        assert_eq!(settings::load_console(&f.dir.join("state")).prune_for("regtest"), None);
        assert_eq!(v["prune_effective"]["has_pruned_data"], json!(false));

        // The same through the wizard.
        let (code, _) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":5_000}})).await;
        assert_eq!(code, 200);
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":null})).await;
        assert_eq!(code, 200, "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file());

        // Now a node is started with pruning on: the folder records it, and the way back needs the wipe.
        let (code, _) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        assert!(node_action(&f.state, "start").await.ok);
        let agent = f.state.read().await.console.agent.clone().unwrap();
        agent.stop_and_wait().await;
        let root = f.dir.join("root");
        assert_eq!(networks::read_meta(&root, regtest()).unwrap().ran_pruned, Some(true));
        let (_, v) = patch_settings(&f.state, &json!({})).await;
        assert_eq!(v["prune_effective"]["has_pruned_data"], json!(true), "{v}");
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null}})).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!(v["needs"], json!("wipe"));
        let (code, v) = setup(&f.state, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":null})).await;
        assert_eq!(code, 409, "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file());

        // A node started without pruning does not clear it; only the wipe does.
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        assert_eq!(networks::read_meta(&root, regtest()).unwrap().ran_pruned, None);
        assert_eq!(v["prune_effective"]["has_pruned_data"], json!(false));
        assert!(node_action(&f.state, "start").await.ok);
        agent.stop_and_wait().await;
        assert_eq!(networks::read_meta(&root, regtest()).unwrap().ran_pruned, None, "an unpruned start leaves it unset");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// After the wipe the console forgets what it knew of that chain: the table row, the heights of the status.
    #[tokio::test]
    async fn n3_after_a_wipe_no_old_heights_are_shown() {
        let f = fixture("f-n3", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        let root = f.dir.join("root");
        chain_data_on_disk(&f); // meta.json says last_height 900 / last_header 901
        {
            let mut g = f.state.write().await;
            let job = g.console.disk_job().expect("a measurement is due");
            run_disk_job(job.0, job.1);
            g.snapshot.local_height = 863;
            g.snapshot.network_height = 863;
            g.snapshot.feed = vec![crate::feed::FeedItem::block(863)];
            stamp_power(&mut g);
            let row = g.snapshot.managed.as_ref().unwrap().networks.iter().find(|r| r.id == "regtest").unwrap().clone();
            assert_eq!((row.last_height, row.has_data), (Some(900), true));
        }
        let (code, v) = patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await;
        assert_eq!(code, 200, "{v}");
        let mut g = f.state.write().await;
        stamp_power(&mut g);
        let row = g.snapshot.managed.as_ref().unwrap().networks.iter().find(|r| r.id == "regtest").unwrap().clone();
        assert_eq!((row.last_height, row.has_data), (None, false), "the table row is fresh at once");
        assert_eq!((g.snapshot.local_height, g.snapshot.network_height), (0, 0));
        assert!(g.snapshot.feed.is_empty());
        let meta = networks::read_meta(&root, regtest()).unwrap();
        assert_eq!((meta.last_height, meta.last_header), (None, None));
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// The node's own answer about pruning counts only while it answers.
    #[tokio::test]
    async fn n2_nothing_the_node_said_about_pruning_survives_its_stop() {
        let f = fixture("f-n2", managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        {
            let mut g = f.state.write().await;
            // A parked answer from when the node ran: pruned, keeping 300.
            g.snapshot.connected = true;
            g.snapshot.pruning.node_reports_pruned = Some(true);
            g.snapshot.pruning.node_enabled = Some(true);
            g.snapshot.pruning.node_keep_blocks = Some(300);
            g.snapshot.pruning.pending_restart = true;
            // The node is not running (no process, nothing answers): the next status has no memory of it.
            g.snapshot.connected = false;
            stamp_power(&mut g);
            let p = &g.snapshot.pruning;
            assert_eq!((p.node_reports_pruned, p.node_enabled, p.node_keep_blocks, p.pending_restart), (None, None, None, false));
            assert_eq!((p.enabled, p.keep_blocks), (true, Some(300)));
        }
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// One patch that moves the data folder and removes pruning is judged against the NEW folder.
    #[tokio::test]
    async fn n10_a_patch_that_moves_the_data_folder_is_judged_on_the_new_one() {
        let dir = std::env::temp_dir().join(format!("blvm-f-n10-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join("fake-blvm.sh");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (a, b) = (dir.join("rootA"), dir.join("rootB"));
        let env = EnvOverrides { mode: Some("managed".into()), node_bin: Some(bin.display().to_string()), ..Default::default() };
        let state = shared(load_in(&dir, env));
        let (code, v) = patch_settings(&state, &json!({"data_root": a.display().to_string(), "network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200, "{v}");
        let put = |root: &Path, ran: Option<bool>| {
            let d = networks::dir(root, regtest());
            std::fs::create_dir_all(d.join("data/heed3")).unwrap();
            std::fs::write(d.join("data/heed3/data.mdb"), vec![7u8; 4096]).unwrap();
            networks::write_meta(root, regtest(), &networks::Meta { ran_pruned: ran, ..Default::default() }).unwrap();
            d
        };
        // Old folder: pruned data. New folder: empty. The setting is saved, and the old folder is left alone.
        let a_dir = put(&a, Some(true));
        let (code, v) = patch_settings(&state, &json!({"data_root": b.display().to_string(), "prune_by_network":{"regtest":null}})).await;
        assert_eq!(code, 200, "nothing to wipe in the new folder: {v}");
        assert!(a_dir.join("data/heed3/data.mdb").is_file(), "the old folder was not touched");
        assert_eq!(settings::load_console(&dir.join("state")).prune_for("regtest"), None);
        assert_eq!(state.read().await.console.eff.data_root, b);

        // The other way round: the folder moved to has pruned data, the one left has none. Refused until confirmed.
        let (code, _) = patch_settings(&state, &json!({"prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        let b_dir = put(&b, Some(true));
        std::fs::remove_dir_all(a_dir.join("data")).unwrap();
        let (code, _) = patch_settings(&state, &json!({"data_root": a.display().to_string()})).await;
        assert_eq!(code, 200);
        let (code, _) = patch_settings(&state, &json!({"prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        let (code, v) = patch_settings(&state, &json!({"data_root": b.display().to_string(), "prune_by_network":{"regtest":null}})).await;
        assert_eq!(code, 409, "the new folder holds pruned data: {v}");
        assert_eq!(v["needs"], json!("wipe"));
        assert!(b_dir.join("data/heed3/data.mdb").is_file());
        assert_eq!(state.read().await.console.eff.data_root, a, "nothing changed");
        let (code, v) = patch_settings(&state, &json!({"data_root": b.display().to_string(), "prune_by_network":{"regtest":null},"confirm_wipe":true})).await;
        assert_eq!(code, 200, "{v}");
        assert!(!b_dir.join("data").exists(), "the NEW folder's data was wiped");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- memory page size: the official release does not start on 16 KB kernels (Raspberry Pi 5) ----

    fn offline_releases() -> Arc<releases::Cache> {
        releases::Cache::new(releases::Source { api_url: "http://127.0.0.1:1/api".into(), https_only: false, ..Default::default() })
    }

    #[tokio::test]
    async fn in_process_a_16k_kernel_is_reported_by_the_releases_route_and_refuses_the_install_with_409() {
        let dir = std::env::temp_dir().join(format!("blvm-pagesize-inproc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let env = EnvOverrides { mode: Some("managed".into()), data_root: Some(dir.join("root").display().to_string()), ..Default::default() };
        let mut c = load_in(&dir, env);
        c.platform_override = Some("linux-aarch64".into());
        c.page_size_override = Some(16384);
        c.releases = offline_releases();
        let state = shared(c);

        let v = releases_json(&state, false).await;
        assert_eq!((v["runs_here"].clone(), v["page_size"].clone()), (json!(false), json!(16384)), "{v}");
        let why = v["runs_here_reason"].as_str().unwrap();
        assert_eq!(why, sysinfo::page_size_message(Some(16384)));
        assert!(why.starts_with("This device uses 16 KB memory pages.") && why.contains("kernel=kernel8.img"), "{why}");

        // The install is refused before any release is looked up (the release server here is unreachable).
        let (code, v) = installer::begin(&state, installer::Kind::Install, None).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!((v["ok"].clone(), v["code"].as_str()), (json!(false), Some("page_size")));
        assert_eq!(v["error"].as_str().unwrap(), why);
        let (code, v) = installer::begin(&state, installer::Kind::Update, Some("v0.1.61")).await;
        assert_eq!(code, 409, "{v}");
        assert_eq!(state.read().await.console.job.lock().unwrap().phase, "idle", "no job was started");

        // The status carries the page size, labelled.
        {
            let mut g = state.write().await;
            stamp_power(&mut g);
        }
        let st = serde_json::to_value(&state.read().await.snapshot).unwrap();
        let p = &st["managed"]["platform"];
        assert_eq!((p["page_size"].clone(), p["node_page_size"].clone(), p["runs_here"].clone()), (json!(16384), json!(16384), json!(false)), "{p}");
        assert_eq!(p["agent"], Value::Null, "the node runs in this process");

        // 4 KB pages (any other platform too): nothing in the way. A pretended 16 KB on x86_64 or macOS is no problem either.
        for (platform, page) in [("linux-aarch64", 4096u64), ("linux-x86_64", 16384), ("unsupported", 16384)] {
            let mut g = state.write().await;
            g.console.platform_override = Some(platform.into());
            g.console.page_size_override = Some(page);
            drop(g);
            let v = releases_json(&state, false).await;
            assert_eq!((v["runs_here"].clone(), v["runs_here_reason"].clone()), (json!(true), Value::Null), "{platform} {page}");
            assert_eq!(v["page_size"], json!(page));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn on_umbrel_the_agents_page_size_from_state_json_decides() {
        let dir = std::env::temp_dir().join(format!("blvm-pagesize-ext-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        let env = EnvOverrides { mode: Some("managed".into()), agent_external: true, data_root: Some(root.display().to_string()), ..Default::default() };
        let mut c = load_in(&dir, env);
        // This container sees 4 KB (or nothing); the node container says 16 KB: the node's computer decides.
        c.platform_override = Some("linux-aarch64".into());
        c.page_size_override = Some(4096);
        c.releases = offline_releases();
        let state = shared(c);
        let write = |platform: Option<sysinfo::Facts>| {
            control::write_state(&root, &control::StateFile { heartbeat: crate::persist::unix_now(), binary_found: false, platform, ..Default::default() }).unwrap();
        };

        // An agent from before this check wrote nothing: this process's own facts are used.
        write(None);
        let v = releases_json(&state, false).await;
        assert_eq!((v["runs_here"].clone(), v["page_size"].clone()), (json!(true), json!(4096)), "{v}");

        write(Some(sysinfo::Facts::for_page("linux-aarch64", Some(16384))));
        tokio::time::sleep(Duration::from_millis(300)).await; // (state.json is cached for 200 ms)
        let v = releases_json(&state, false).await;
        assert_eq!((v["runs_here"].clone(), v["page_size"].clone()), (json!(false), json!(16384)), "{v}");
        assert_eq!(v["runs_here_reason"].as_str().unwrap(), sysinfo::page_size_message(Some(16384)));
        let (code, v) = installer::begin(&state, installer::Kind::Install, None).await;
        assert_eq!((code, v["code"].as_str()), (409, Some("page_size")), "{v}");
        assert!(v["error"].as_str().unwrap().contains("16 KB memory pages"));

        {
            let mut g = state.write().await;
            stamp_power(&mut g);
        }
        let st = serde_json::to_value(&state.read().await.snapshot).unwrap();
        let p = &st["managed"]["platform"];
        assert_eq!(p["page_size"], json!(4096), "the console's own page size is labelled separately");
        assert_eq!((p["node_page_size"].clone(), p["runs_here"].clone()), (json!(16384), json!(false)), "{p}");
        assert_eq!((p["agent"]["page_size"].clone(), p["agent"]["runs_here"].clone()), (json!(16384), json!(false)), "{p}");
        let _ = std::fs::remove_dir_all(&dir);
    }
    // ---- fix pass 3 (re-review: B1, S1-S3, N1-N4, N7) ----

    /// A pruned store with a node that is not running, ready for a confirmed wipe.
    async fn pruned_regtest_store(tag: &str) -> (Fix, PathBuf) {
        let f = fixture(tag, managed_env(), true);
        let (code, _) = patch_settings(&f.state, &json!({"network":"regtest","prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        let net_dir = chain_data_on_disk(&f);
        (f, net_dir)
    }

    async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    /// B1: hyper drops the handler's future when the client goes away. A wipe request dropped while the data is
    /// being deleted must still end the wipe: starts allowed again, the setting saved, `ran_pruned` cleared.
    /// (Without the spawned task the console refused everything with "being deleted" until it was restarted.)
    #[tokio::test]
    async fn b1_a_wipe_request_that_is_dropped_mid_delete_still_finishes() {
        let (f, net_dir) = pruned_regtest_store("f3-b1").await;
        let root = f.dir.join("root");
        SLOW_WIPE.lock().unwrap().insert(root.clone(), Duration::from_millis(1200));
        let st = f.state.clone();
        let r = tokio::time::timeout(Duration::from_millis(300), async move {
            patch_settings(&st, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await
        })
        .await;
        assert!(r.is_err(), "the request future was dropped mid-delete (the client went away)");
        assert!(!f.state.read().await.console.wiping.is_empty(), "the delete is still running");
        // The request is gone; the work is not.
        let state = f.state.clone();
        let mut done = false;
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end && !done {
            tokio::time::sleep(Duration::from_millis(50)).await;
            done = state.read().await.console.wiping.is_empty();
        }
        SLOW_WIPE.lock().unwrap().remove(&root);
        assert!(done, "WEDGE: wiping is still set after the request went away");
        assert!(!net_dir.join("data").exists(), "the delete finished");
        // The setting is saved (it needs the lock the task takes last: look until it is there).
        wait_until("the full-node choice is saved", || settings::load_console(&f.dir.join("state")).prune_for("regtest").is_none()).await;
        assert_eq!(networks::read_meta(&root, regtest()).unwrap().ran_pruned, None, "ran_pruned cleared");
        assert_eq!(f.state.read().await.console.settings.prune_for("regtest"), None);
        // Starts are allowed again, and so is everything else.
        let agent = f.state.read().await.console.agent.clone().unwrap();
        assert!(agent.start().await.is_ok(), "hold_starts released");
        agent.stop_and_wait().await;
        let (code, v) = patch_settings(&f.state, &json!({"show_logs":true})).await;
        assert_eq!(code, 200, "{v}");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// N2: a Stop during the delete (allowed while wiping) writes `node_wanted`; the save after the delete must
    /// not put the old value back.
    #[tokio::test]
    async fn n2_a_stop_during_the_delete_survives_the_save_after_it() {
        let (f, net_dir) = pruned_regtest_store("f3-n2").await;
        let root = f.dir.join("root");
        remember_wanted(&f.state, true).await;
        SLOW_WIPE.lock().unwrap().insert(root.clone(), Duration::from_millis(1000));
        let st = f.state.clone();
        let task = tokio::spawn(async move {
            patch_settings(&st, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await
        });
        wait_until("the wipe has begun", || f.state.try_read().map(|g| !g.console.wiping.is_empty()).unwrap_or(false)).await;
        let out = node_action(&f.state, "stop").await;
        assert!(out.ok, "{out:?}");
        let (code, v) = task.await.unwrap();
        SLOW_WIPE.lock().unwrap().remove(&root);
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        assert_eq!(f.state.read().await.console.settings.prune_for("regtest"), None, "this request's change is in");
        assert_eq!(f.state.read().await.console.settings.node_wanted, Some(false), "the Stop is not undone");
        assert_eq!(settings::load_console(&f.dir.join("state")).node_wanted, Some(false), "and it is on disk");
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    #[test]
    fn merging_after_a_wipe_applies_only_the_keys_the_request_changed() {
        let before = ConsoleSettings { network: Some("regtest".into()), node_wanted: Some(true), show_logs: false, ..Default::default() };
        let mut patched = before.clone();
        patched.show_logs = true;
        patched.prune_by_network.insert("regtest".into(), 300);
        let now = ConsoleSettings { node_wanted: Some(false), ..before.clone() };
        let merged = merge_after_wipe(&now, &before, patched);
        assert_eq!(merged.node_wanted, Some(false), "what changed meanwhile stays");
        assert!(merged.show_logs && merged.prune_by_network.get("regtest") == Some(&300), "what the request changed is applied");
    }

    /// A: no start of any kind while the hold is on: the log of the node shows exactly one start. (The 5 s
    /// restart timer goes through `claim`; a claim that ignores the hold starts the node again.)
    #[tokio::test]
    async fn a_claim_under_the_hold_takes_no_slot_so_the_log_shows_one_start() {
        let f = crash_once_on_regtest("f3-a-claim").await;
        let root = f.dir.join("root");
        let net_dir = networks::dir(&root, regtest());
        let starts = || std::fs::read_to_string(net_dir.join("logs/node.log")).unwrap_or_default().matches("[console] starting").count();
        let agent = f.state.read().await.console.agent.clone().unwrap();
        assert_eq!(agent.state(), ProcState::Crashed);
        let before = starts();
        agent.hold_starts(true);
        tokio::time::sleep(Duration::from_millis(6500)).await;
        assert_eq!(starts(), before, "the restart timer fired into the hold and started nothing");
        assert_eq!(agent.state(), ProcState::Crashed);
        agent.hold_starts(false);
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// An external-mode console over a data folder without an agent; the test plays the agent by writing
    /// `state.json`.
    struct ExtFix {
        dir: PathBuf,
        root: PathBuf,
        state: Shared,
    }

    impl ExtFix {
        async fn new(tag: &str) -> ExtFix {
            let dir = std::env::temp_dir().join(format!("blvm-f3-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            let root = dir.join("root");
            let env = EnvOverrides { mode: Some("managed".into()), agent_external: true, data_root: Some(root.display().to_string()), ..Default::default() };
            let state = shared(load_in(&dir, env));
            let (code, v) = patch_settings(&state, &json!({"network":"regtest"})).await;
            assert_eq!(code, 200, "{v}");
            ExtFix { dir, root, state }
        }

        fn agent_wrote(&self, heartbeat: u64, generation: u64, st: &str) {
            let f = control::StateFile { heartbeat, generation, state: st.into(), ..Default::default() };
            control::write_state(&self.root, &f).unwrap();
        }

        fn desired(&self) -> control::Desired {
            control::read_desired(&self.root).unwrap()
        }

        async fn agent(&self) -> Agent {
            self.state.read().await.console.agent.clone().unwrap()
        }
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(300)).await; // (state.json is cached for 200 ms)
    }

    /// N (managed level): a settings patch in external mode puts the new values into `desired.json`, with the
    /// node's `running`, `restart_id` and `start_id` as they were. (Without `sync_desired` in `patch_settings`
    /// the agent's own restart would use the old values.)
    #[tokio::test]
    async fn n_a_settings_patch_in_external_mode_rewrites_desired_json_with_the_new_values() {
        let x = ExtFix::new("n").await;
        let agent = x.agent().await;
        agent.start().await.unwrap();
        x.agent_wrote(crate::persist::unix_now(), 1, "running");
        let d = x.desired();
        assert_eq!((d.generation, d.running, d.assume_valid.clone(), d.prune_keep_blocks), (1, true, None, None));
        let (code, v) = patch_settings(&x.state, &json!({"assume_valid_by_network":{"regtest":"800000"},"prune_by_network":{"regtest":1000}})).await;
        assert_eq!(code, 200, "{v}");
        let d2 = x.desired();
        assert_eq!(d2.assume_valid.as_deref(), Some("800000"));
        assert_eq!(d2.prune_keep_blocks, Some(1000));
        assert_eq!((d2.generation, d2.running, d2.restart_id, d2.start_id), (2, true, d.restart_id, d.start_id), "settings only, node untouched");
        let _ = std::fs::remove_dir_all(&x.dir);
    }

    /// J: a Save while the node is stopped must not ask for it to run.
    #[tokio::test]
    async fn j_a_save_while_the_node_is_stopped_leaves_running_false() {
        let x = ExtFix::new("j").await;
        let agent = x.agent().await;
        agent.start().await.unwrap();
        agent.request_stop();
        x.agent_wrote(crate::persist::unix_now(), 2, "stopped");
        let d = x.desired();
        assert_eq!((d.generation, d.running), (2, false));
        let (code, v) = patch_settings(&x.state, &json!({"assume_valid_by_network":{"regtest":"800000"}})).await;
        assert_eq!(code, 200, "{v}");
        let d2 = x.desired();
        assert_eq!(d2.assume_valid.as_deref(), Some("800000"), "the value travels");
        assert!(!d2.running, "but the node is not asked to run");
        assert_eq!(d2.start_id, d.start_id);
        let _ = std::fs::remove_dir_all(&x.dir);
    }

    /// S2: with the agent quiet (stale heartbeat) nobody can say whether a node runs, and a request written now
    /// would be carried out when the agent returns: changes that need a stopped node wait. A plain Stop does not.
    #[tokio::test]
    async fn s2_a_quiet_agent_holds_back_every_change_that_needs_a_stopped_node() {
        const MSG: &str = "The node agent is not answering; try again when it is back.";
        let x = ExtFix::new("s2").await;
        x.agent_wrote(crate::persist::unix_now() - 120, 0, "stopped");
        settle().await;
        for patch in [json!({"network":"signet"}), json!({"rpc_overrides":{"regtest":"127.0.0.1:19999"}})] {
            let (code, v) = patch_settings(&x.state, &patch).await;
            assert_eq!((code, v["error"].clone()), (409, json!(MSG)), "{patch}");
        }
        let (code, v) = setup(&x.state, &json!({"mode":"managed","network":"signet"})).await;
        assert_eq!((code, v["error"].clone()), (409, json!(MSG)), "setup with another network");
        assert!(control::read_desired(&x.root).is_none(), "nothing was written for the agent");
        assert_eq!(settings::load_console(&x.dir.join("state")).network.as_deref(), Some("regtest"), "nothing saved");
        // A change that needs no stopped node is a plain save.
        let (code, v) = patch_settings(&x.state, &json!({"show_logs":true})).await;
        assert_eq!(code, 200, "{v}");
        // The same setup for the network it already has is not a move.
        let (code, v) = setup(&x.state, &json!({"mode":"managed","network":"regtest"})).await;
        assert_eq!(code, 200, "{v}");
        // A plain Stop is still delivered.
        let agent = x.agent().await;
        agent.start().await.unwrap();
        assert!(x.desired().running);
        agent.request_stop();
        assert!(!x.desired().running, "Stop reaches desired.json whatever the agent is doing");
        // And the agent back: the change goes through.
        x.agent_wrote(crate::persist::unix_now(), 2, "stopped");
        settle().await;
        let (code, v) = patch_settings(&x.state, &json!({"network":"signet"})).await;
        assert_eq!(code, 200, "{v}");
        let _ = std::fs::remove_dir_all(&x.dir);
    }

    /// N4: `state.json` says stopped and caught up, but `desired.json` still asks for the node (it exited by
    /// itself): an agent that restarts would start it on the data being deleted. Refused until Stop.
    #[tokio::test]
    async fn n4_a_wipe_needs_desired_json_to_stop_asking_for_the_node() {
        let x = ExtFix::new("n4").await;
        let (code, v) = patch_settings(&x.state, &json!({"prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200, "{v}");
        let net_dir = networks::dir(&x.root, regtest());
        std::fs::create_dir_all(net_dir.join("data/heed3")).unwrap();
        std::fs::write(net_dir.join("data/heed3/data.mdb"), vec![7u8; 4096]).unwrap();
        networks::write_meta(&x.root, regtest(), &networks::Meta { ran_pruned: Some(true), ..Default::default() }).unwrap();
        let wipe = json!({"prune_by_network":{"regtest":null},"confirm_wipe":true});
        let agent = x.agent().await;
        agent.start().await.unwrap();
        x.agent_wrote(crate::persist::unix_now(), 1, "stopped"); // exited by itself; the file still says run
        settle().await;
        let (code, v) = patch_settings(&x.state, &wipe).await;
        assert_eq!(code, 409, "{v}");
        assert!(v["error"].as_str().unwrap().contains("Stop the node first"), "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file());
        // Stop, the agent deals with it: the wipe goes.
        agent.request_stop();
        x.agent_wrote(crate::persist::unix_now(), 2, "stopped");
        settle().await;
        let (code, v) = patch_settings(&x.state, &wipe).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        let _ = std::fs::remove_dir_all(&x.dir);
    }

    /// N7: a Save right before a wipe bumps the generation; the agent has not read it yet but has dealt with
    /// the Stop before it, and a settings-only generation cannot start anything: the wipe does not have to wait.
    #[tokio::test]
    async fn n7_a_save_after_the_stop_does_not_hold_up_a_wipe() {
        let x = ExtFix::new("n7").await;
        let (code, _) = patch_settings(&x.state, &json!({"prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200);
        let net_dir = networks::dir(&x.root, regtest());
        std::fs::create_dir_all(net_dir.join("data/heed3")).unwrap();
        std::fs::write(net_dir.join("data/heed3/data.mdb"), vec![7u8; 4096]).unwrap();
        networks::write_meta(&x.root, regtest(), &networks::Meta { ran_pruned: Some(true), ..Default::default() }).unwrap();
        let agent = x.agent().await;
        agent.start().await.unwrap();
        agent.request_stop();
        x.agent_wrote(crate::persist::unix_now(), 2, "stopped");
        let (code, v) = patch_settings(&x.state, &json!({"assume_valid_by_network":{"regtest":"800000"}})).await;
        assert_eq!(code, 200, "{v}");
        assert_eq!(x.desired().generation, 3, "the save is a generation the agent has not read");
        settle().await;
        let (code, v) = patch_settings(&x.state, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        let _ = std::fs::remove_dir_all(&x.dir);
    }

    /// SF1a: the setup half of B1. A `/api/setup` request with `confirm_wipe` that is dropped in the middle of the
    /// delete still finishes it (the page does not send `confirm_wipe` here today; the API takes it).
    #[tokio::test]
    async fn sf1a_a_setup_with_confirm_wipe_that_is_dropped_mid_delete_still_finishes() {
        let (f, net_dir) = pruned_regtest_store("f4-sf1a").await;
        let root = f.dir.join("root");
        SLOW_WIPE.lock().unwrap().insert(root.clone(), Duration::from_millis(1200));
        let st = f.state.clone();
        let r = tokio::time::timeout(Duration::from_millis(300), async move {
            setup(&st, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":null,"confirm_wipe":true})).await
        })
        .await;
        assert!(r.is_err(), "the request future was dropped mid-delete (the client went away)");
        assert!(!f.state.read().await.console.wiping.is_empty(), "the delete is still running");
        let state = f.state.clone();
        let mut done = false;
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end && !done {
            tokio::time::sleep(Duration::from_millis(50)).await;
            done = state.read().await.console.wiping.is_empty();
        }
        SLOW_WIPE.lock().unwrap().remove(&root);
        assert!(done, "WEDGE: wiping is still set after the setup request went away");
        assert!(!net_dir.join("data").exists(), "the delete finished");
        wait_until("the full-node choice is saved", || settings::load_console(&f.dir.join("state")).prune_for("regtest").is_none()).await;
        assert_eq!(networks::read_meta(&root, regtest()).unwrap().ran_pruned, None, "ran_pruned cleared");
        let agent = f.state.read().await.console.agent.clone().unwrap();
        assert!(agent.start().await.is_ok(), "hold_starts released");
        agent.stop_and_wait().await;
        let _ = std::fs::remove_dir_all(&f.dir);
    }

    /// SF1b: the console asked for something (`desired.json` exists) and no agent ever wrote `state.json`: that is
    /// "quiet" too, and a network change waits.
    #[tokio::test]
    async fn sf1b_an_agent_that_never_answered_holds_back_a_network_change() {
        const MSG: &str = "The node agent is not answering; try again when it is back.";
        let x = ExtFix::new("sf1b").await;
        let agent = x.agent().await;
        agent.start().await.unwrap();
        agent.request_stop(); // (a node that is starting would answer "Stop the node first" before the agent check)
        settle().await;
        assert!(control::read_state(&x.root).is_none(), "no agent ever wrote state.json");
        assert!(x.desired().generation > 0);
        let (code, v) = patch_settings(&x.state, &json!({"network":"signet"})).await;
        assert_eq!((code, v["error"].clone()), (409, json!(MSG)), "{v}");
        let (code, v) = setup(&x.state, &json!({"mode":"managed","network":"signet"})).await;
        assert_eq!((code, v["error"].clone()), (409, json!(MSG)), "{v}");
        assert_eq!(settings::load_console(&x.dir.join("state")).network.as_deref(), Some("regtest"), "nothing saved");
        // The very first setup (nothing asked of any agent yet) is not held back: see the first run.
        let y = ExtFix::new("sf1b-first").await;
        assert!(control::read_desired(&y.root).is_none());
        let (code, v) = patch_settings(&y.state, &json!({"network":"signet"})).await;
        assert_eq!(code, 200, "{v}");
        let _ = std::fs::remove_dir_all(&x.dir);
        let _ = std::fs::remove_dir_all(&y.dir);
    }

    /// SF1c: a console that starts over a folder where a Stop was written but the agent has not dealt with it
    /// (`desired.generation` > `state.generation`) cannot know which generations were requests: a wipe waits. (A
    /// `request_gen` of 0 after a restart of the console would let it through. With the agent in a crash backoff
    /// the wipe waits anyway: `abandon_restart` in `begin_wipe` writes a new request first.)
    #[tokio::test]
    async fn sf1c_a_fresh_console_over_an_undealt_stop_does_not_wipe() {
        let x = ExtFix::new("sf1c").await;
        let (code, v) = patch_settings(&x.state, &json!({"prune_by_network":{"regtest":300}})).await;
        assert_eq!(code, 200, "{v}");
        let net_dir = networks::dir(&x.root, regtest());
        std::fs::create_dir_all(net_dir.join("data/heed3")).unwrap();
        std::fs::write(net_dir.join("data/heed3/data.mdb"), vec![7u8; 4096]).unwrap();
        networks::write_meta(&x.root, regtest(), &networks::Meta { ran_pruned: Some(true), ..Default::default() }).unwrap();
        let agent = x.agent().await;
        agent.start().await.unwrap();
        x.agent_wrote(crate::persist::unix_now(), 1, "stopped"); // the node exited by itself; desired.json still said run
        agent.request_stop(); // generation 2, written; the agent has not read it yet
        assert_eq!((x.desired().generation, x.desired().running), (2, false));
        // The console restarts: a new `Ext` over the same folder.
        let env = EnvOverrides { mode: Some("managed".into()), agent_external: true, data_root: Some(x.root.display().to_string()), ..Default::default() };
        let fresh = shared(load_in(&x.dir, env));
        settle().await;
        let wipe = json!({"prune_by_network":{"regtest":null},"confirm_wipe":true});
        let (code, v) = patch_settings(&fresh, &wipe).await;
        assert_eq!(code, 409, "{v}");
        assert!(v["error"].as_str().unwrap().contains("Stop the node first"), "{v}");
        assert!(net_dir.join("data/heed3/data.mdb").is_file(), "nothing was deleted");
        // The agent deals with the Stop: the wipe goes.
        x.agent_wrote(crate::persist::unix_now(), 2, "stopped");
        settle().await;
        let (code, v) = patch_settings(&fresh, &wipe).await;
        assert_eq!(code, 200, "{v}");
        assert!(!net_dir.join("data").exists());
        let _ = std::fs::remove_dir_all(&x.dir);
    }

    /// N10: the network switch waits for a quiet agent, like every other change that needs a stopped node. (It
    /// used to write Stop and Start, save the new network and show a failed switch at once.)
    #[tokio::test]
    async fn n10_a_network_switch_waits_for_a_quiet_agent() {
        const MSG: &str = "The node agent is not answering; try again when it is back.";
        let x = ExtFix::new("n10").await;
        x.agent_wrote(crate::persist::unix_now() - 120, 0, "stopped");
        settle().await;
        let (code, v) = crate::switch::begin(&x.state, "signet").await;
        assert_eq!((code, v["error"].clone()), (409, json!(MSG)), "{v}");
        assert!(control::read_desired(&x.root).is_none(), "nothing was written for the agent");
        assert!(x.state.read().await.console.switch.is_none(), "no switch was started");
        assert_eq!(x.state.read().await.console.settings.network.as_deref(), Some("regtest"));
        let _ = std::fs::remove_dir_all(&x.dir);
    }

    /// N6: if the section between `begin_wipe` and `end_wipe` unwinds, `wiping` is cleared and starts are allowed
    /// again, with the request answered 500 and the setting not saved. (The test hook panics right after the delete.)
    #[tokio::test]
    async fn n6_a_wipe_section_that_unwinds_releases_the_hold() {
        for via_setup in [false, true] {
            let (f, net_dir) = pruned_regtest_store(if via_setup { "f4-n6-setup" } else { "f4-n6" }).await;
            let root = f.dir.join("root");
            PANIC_AFTER_WIPE.lock().unwrap().insert(root.clone());
            let (code, v) = if via_setup {
                setup(&f.state, &json!({"mode":"managed","network":"regtest","prune_keep_blocks":null,"confirm_wipe":true})).await
            } else {
                patch_settings(&f.state, &json!({"prune_by_network":{"regtest":null},"confirm_wipe":true})).await
            };
            PANIC_AFTER_WIPE.lock().unwrap().remove(&root);
            assert_eq!(code, 500, "{v}");
            assert!(v["error"].as_str().unwrap().starts_with("The request could not be completed"), "{v}");
            // The release happens in a task of its own: look until it is there.
            let state = f.state.clone();
            let mut done = false;
            let end = Instant::now() + Duration::from_secs(10);
            while Instant::now() < end && !done {
                tokio::time::sleep(Duration::from_millis(25)).await;
                done = state.read().await.console.wiping.is_empty();
            }
            assert!(done, "WEDGE: wiping is still set after the wipe section unwound (setup: {via_setup})");
            assert!(!net_dir.join("data").exists(), "the delete itself had run");
            assert_eq!(f.state.read().await.console.settings.prune_for("regtest"), Some(300), "the setting was not saved");
            let agent = f.state.read().await.console.agent.clone().unwrap();
            assert!(agent.start().await.is_ok(), "hold_starts released");
            agent.stop_and_wait().await;
            let (code, v) = patch_settings(&f.state, &json!({"show_logs":true})).await;
            assert_eq!(code, 200, "{v}");
            let _ = std::fs::remove_dir_all(&f.dir);
        }
    }
}
