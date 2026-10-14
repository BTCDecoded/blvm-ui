//! The node agent: starts, watches, restarts and stops one `blvm` process (design B, in-process).
//!
//! * Working directory is `<root>/networks/<net>/`, because the node writes
//!   `data/modules/sockets/node.sock` relative to where it runs. Chain data goes to `./data`.
//! * stdout/stderr go straight to `<root>/networks/<net>/logs/node.log` (append), not through a pipe,
//!   so the node survives a console crash. A tail task copies new lines into a 2 000-line ring.
//! * The node gets its own process group, so Ctrl-C in the console's terminal does not reach it.
//! * `control/node.pid` lets a restarted console adopt a node that is still running.
//! * States: `stopped | starting | running | stopping | crashed`. `starting` becomes `running` when
//!   the RPC answers (the poller feeds [`Agent::rpc_flag`] as soon as any method answers) **and** the
//!   listener on the RPC port is this node (or a child of it). A different process on our port is a
//!   conflict: never `running`. Before a spawn the RPC and P2P ports are checked and a start is refused
//!   when someone else holds one. A silent start is given 180 s, and as long as the node is alive and its
//!   log keeps growing the wait is extended (15 minutes at most). Stop is SIGTERM, then SIGKILL after
//!   200 s (the node drains for up to 30 s, its own watchdog fires at 180 s).
//! * A crash restarts the node after 5 s, 30 s, 120 s (three tries within 10 minutes), then stays
//!   `crashed` with the last 50 log lines.
//! * Two ways to hold an [`Agent`]: the normal one supervises the process itself (desktop console, and the
//!   `agent` subcommand in the node container, see `agent_daemon.rs`); [`Agent::external`] is a client for an
//!   agent in another container. It writes `control/desired.json`, reads `control/state.json` and tails the
//!   shared log file, and offers the same methods, so the switch, the installer and the status code do not
//!   care which one they hold (`agent/external.rs`).

use crate::networks::{self, Network};
use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};
use tokio::process::Command;

mod external;

const RING_CAP: usize = 2000;
const CRASH_LOG_LINES: usize = 50;
/// The first read of a log file starts this many lines before its end (the Logs page shows them first).
const PRELOAD_LINES: usize = 500;
/// ...but looks back at most this many bytes for them.
const PRELOAD_MAX_BYTES: u64 = 2 * 1024 * 1024;
/// One tail pass reads at most this much; a bigger backlog skips to the end.
const TAIL_CHUNK: u64 = 4 * 1024 * 1024;
const TAIL_MAX_BACKLOG: u64 = 8 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 4000;

/// Refusal of a start while chain data is being deleted.
pub const WIPING_MSG: &str = "The chain data is being deleted. Wait for that to finish.";
/// Refusal of a wipe while a node process may exist.
pub const STOP_FIRST_MSG: &str = "Stop the node first, then delete the chain data.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProcState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Crashed,
}

impl ProcState {
    pub fn parse(s: &str) -> Option<ProcState> {
        Some(match s {
            "stopped" => ProcState::Stopped,
            "starting" => ProcState::Starting,
            "running" => ProcState::Running,
            "stopping" => ProcState::Stopping,
            "crashed" => ProcState::Crashed,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ProcState::Stopped => "stopped",
            ProcState::Starting => "starting",
            ProcState::Running => "running",
            ProcState::Stopping => "stopping",
            ProcState::Crashed => "crashed",
        }
    }

    /// A process exists (or is being asked to go away).
    pub fn is_active(self) -> bool {
        matches!(self, ProcState::Starting | ProcState::Running | ProcState::Stopping)
    }
}

#[derive(Debug, Clone)]
pub struct Timings {
    /// `starting` -> the first wait for the RPC. While the node is alive and its log is still growing
    /// the wait is extended by this much again, up to [`Timings::start_max`].
    pub start_timeout: Duration,
    /// Cap on the whole wait for a start (15 minutes).
    pub start_max: Duration,
    /// How often a running node's RPC port is re-checked to still belong to it.
    pub owner_check: Duration,
    /// SIGTERM -> SIGKILL.
    pub stop_wait: Duration,
    /// After SIGKILL, how long `stop_and_wait` keeps waiting for the exit.
    pub kill_wait: Duration,
    /// Delay before automatic restart number 1, 2, 3. Also the number of tries.
    pub backoff: Vec<Duration>,
    /// Crashes older than this do not count towards the limit.
    pub crash_window: Duration,
    pub ready_poll: Duration,
    pub tail_poll: Duration,
    pub log_max_bytes: u64,
    pub version_timeout: Duration,
    /// How often `meta.json` is refreshed while a node runs.
    pub meta_tick: Duration,
    /// How long a network switch waits for the old node to be gone before it gives up and reports
    /// `failed`. `None`: [`Timings::stop_wait`] + [`Timings::kill_wait`] + 2 s, i.e. as long as a plain
    /// stop can take. (Mostly a test hook: after SIGKILL the wait only runs out for a process that
    /// cannot be killed.)
    pub stop_give_up: Option<Duration>,
}

impl Timings {
    pub fn stop_limit(&self) -> Duration {
        self.stop_give_up.unwrap_or(self.stop_wait + self.kill_wait + Duration::from_secs(2))
    }
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(180),
            start_max: Duration::from_secs(15 * 60),
            owner_check: Duration::from_secs(3),
            stop_wait: Duration::from_secs(200),
            kill_wait: Duration::from_secs(10),
            backoff: vec![Duration::from_secs(5), Duration::from_secs(30), Duration::from_secs(120)],
            crash_window: Duration::from_secs(600),
            ready_poll: Duration::from_millis(250),
            tail_poll: Duration::from_millis(400),
            log_max_bytes: 50 * 1024 * 1024,
            version_timeout: Duration::from_secs(5),
            meta_tick: Duration::from_secs(60),
            stop_give_up: None,
        }
    }
}

/// The login of the node's RPC. It is written to the node's own config file before every start (the node
/// reads no environment variable for it), and the console's RPC client sends the same pair.
#[derive(Clone, PartialEq, Eq)]
pub struct RpcAuth {
    pub user: String,
    pub pass: String,
}

impl std::fmt::Debug for RpcAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RpcAuth({}, ****)", self.user)
    }
}

/// The node's config file, in the network folder (`<root>/networks/<id>/blvm.toml`), passed as `--config`.
pub const NODE_CONFIG_FILE: &str = "blvm.toml";

/// A TOML basic string: quotes, backslashes and control characters are escaped.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The text of `blvm.toml`, written before every start. `transport_preference` is the one key the node insists
/// on in a config file (a file without it is rejected as a whole, and the node then runs with its defaults: no
/// login at all), and `tcponly` is what it uses when there is no file. Top-level keys come before the first
/// table. `[rpc_auth]` is present only with a login; `[storage.pruning]` only when `prune_keep_blocks` is set.
pub fn node_config_toml(auth: Option<&RpcAuth>, prune_keep_blocks: Option<u64>) -> String {
    let mut out = String::from(
        "# Written by Commons Node View before every start of the node. Changes here are overwritten.\n\
         transport_preference = \"tcponly\"\n",
    );
    if let Some(auth) = auth {
        out.push_str(&format!(
            "\n[rpc_auth]\nrequired = true\nusername = {}\npassword = {}\n",
            toml_string(&auth.user),
            toml_string(&auth.pass)
        ));
    }
    if let Some(n) = prune_keep_blocks {
        out.push_str(&pruning_toml(n));
    }
    out
}

/// The node's `[storage.pruning]` block for "keep the newest `n` blocks". Every key is written: the node's
/// parsed defaults differ from the ones in its code. An omitted `mode` becomes Aggressive, which replays the
/// chain from the first block every 144 blocks, and `auto_prune` and `incremental_prune_during_ibd` default to
/// false, so the node would store every block while it syncs. `mode` is an internally tagged table
/// (`type = "normal"`), written as its own sub-table, as upstream's mainnet example does.
pub fn pruning_toml(n: u64) -> String {
    format!(
        "\n[storage.pruning]\n\
         auto_prune = true\n\
         auto_prune_interval = 144\n\
         min_blocks_to_keep = {n}\n\
         incremental_prune_during_ibd = true\n\
         prune_window_size = {n}\n\
         min_blocks_for_incremental_prune = 288\n\
         prune_on_startup = false\n\
         \n\
         [storage.pruning.mode]\n\
         type = \"normal\"\n\
         keep_from_height = 0\n\
         min_recent_blocks = {n}\n"
    )
}

/// Write `<net_dir>/blvm.toml` (mode 0600, replaced atomically).
pub fn write_node_config(net_dir: &Path, auth: Option<&RpcAuth>, prune_keep_blocks: Option<u64>) -> std::io::Result<()> {
    crate::fsutil::write_atomic_bytes(
        &net_dir.join(NODE_CONFIG_FILE),
        node_config_toml(auth, prune_keep_blocks).as_bytes(),
        Some(0o600),
    )
}

/// Where to dial a node that was told to bind `addr`: the address itself, except that "any address"
/// (`0.0.0.0`, `[::]`) is reached through loopback.
pub fn probe_addr(addr: &str) -> String {
    match addr.rsplit_once(':') {
        Some((h, port)) if matches!(h, "0.0.0.0" | "[::]" | "::" | "") => format!("127.0.0.1:{port}"),
        _ => addr.to_string(),
    }
}

/// The RPC address is reachable from other machines or containers (anything but loopback).
pub fn rpc_exposed(addr: &str) -> bool {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h).trim_matches(['[', ']']);
    !(host == "localhost" || host == "::1" || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback()))
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub bin: Option<PathBuf>,
    pub root: PathBuf,
    pub network: Option<&'static Network>,
    /// `host:port` for `--rpc-addr`; the network's default when `None`.
    pub rpc_addr: Option<String>,
    pub assume_valid: Option<String>,
    /// Block pruning: keep the newest N blocks (`None`: every block). Written into the node's `blvm.toml`.
    pub prune_keep_blocks: Option<u64>,
    pub child_env: BTreeMap<String, String>,
    /// The RPC login the node is started with. `None`: the config file has no `[rpc_auth]`, the node's RPC has
    /// no login (only acceptable while it listens on this machine alone: a start with an exposed address is
    /// refused).
    pub rpc_auth: Option<RpcAuth>,
    /// Port for `--listen-addr` when not the network's own, and then on loopback only. Tests only: a node in a
    /// test must not take the P2P port that every other test checks for strangers, nor listen to the network.
    pub p2p_port: Option<u16>,
    /// Port for `--listen-addr` when not the network's own, on `0.0.0.0` like the default (the Umbrel app
    /// publishes it: `BLVM_AGENT_P2P_PORT_<NET>`). `p2p_port` wins when both are set.
    pub p2p_listen_port: Option<u16>,
}

impl AgentConfig {
    pub fn p2p(&self, net: &Network) -> u16 {
        self.p2p_port.or(self.p2p_listen_port).unwrap_or(net.p2p_port)
    }

    pub fn net_dir(&self) -> Option<PathBuf> {
        self.network.map(|n| networks::dir(&self.root, n))
    }

    pub fn log_path(&self) -> Option<PathBuf> {
        self.network.map(|n| networks::log_file(&self.root, n))
    }

    pub fn rpc(&self) -> Option<String> {
        let n = self.network?;
        Some(self.rpc_addr.clone().unwrap_or_else(|| networks::default_rpc(n)))
    }
}

fn pid_file(root: &Path) -> PathBuf {
    root.join("control").join("node.pid")
}

/// `--assumevalid` / `--noassumevalid` for this network (see [`crate::settings::ConsoleSettings::assume_valid`]).
pub fn assume_valid_args(setting: Option<&str>, net: &Network) -> Vec<String> {
    match setting.map(str::trim) {
        None | Some("") | Some("default") => {
            if net.full_validation {
                vec!["--noassumevalid".into()]
            } else {
                Vec::new()
            }
        }
        Some("off") | Some("0") => vec!["--noassumevalid".into()],
        Some(v) if valid_assume_valid(v) => vec!["--assumevalid".into(), v.to_string()],
        Some(_) => Vec::new(),
    }
}

/// `default`, `off`, a block height, or a 64-character block hash.
pub fn valid_assume_valid(v: &str) -> bool {
    let v = v.trim();
    v == "default"
        || v == "off"
        || (!v.is_empty() && v.len() <= 9 && v.bytes().all(|b| b.is_ascii_digit()))
        || (v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// P2P bind address, always stated: the node's own default exists only for some networks.
pub fn listen_addr(net: &Network) -> String {
    format!("0.0.0.0:{}", net.p2p_port)
}

fn listen_addr_for(cfg: &AgentConfig, net: &Network) -> String {
    // A port chosen by a test (`p2p_port`) belongs to a node that must not be reachable from the network.
    let host = if cfg.p2p_port.is_some() { "127.0.0.1" } else { "0.0.0.0" };
    format!("{host}:{}", cfg.p2p(net))
}

pub fn build_args(cfg: &AgentConfig, net: &Network) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "--network".into(),
        net.cli.into(),
        "--data-dir".into(),
        "./data".into(),
        "--rpc-addr".into(),
        cfg.rpc_addr.clone().unwrap_or_else(|| networks::default_rpc(net)),
        "--listen-addr".into(),
        listen_addr_for(cfg, net),
    ];
    args.extend(assume_valid_args(cfg.assume_valid.as_deref(), net));
    // Always: without it the node may pick up `~/.config/blvm/blvm.toml` or `/etc/blvm/blvm.toml` instead of the
    // file the console wrote (it only looks in its working folder when nothing else is named).
    args.push("--config".into());
    args.push(format!("./{NODE_CONFIG_FILE}"));
    args
}

/// Environment the node gets on top of the console's own: `(name, Some(value))` sets, `(name, None)` unsets.
/// `BLVM_SCHNORR_PER_SIG=1` is on unless the settings say otherwise: the default collector mode skips
/// Taproot signature checks, per-signature mode is upstream's own complete path.
pub fn child_env(cfg: &AgentConfig) -> Vec<(String, Option<String>)> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    map.insert("BLVM_SCHNORR_PER_SIG".into(), "1".into());
    for (k, v) in &cfg.child_env {
        map.insert(k.clone(), v.clone());
    }
    map.into_iter()
        .map(|(k, v)| if v.is_empty() { (k, None) } else { (k, Some(v)) })
        .collect()
}

/// `blvm 0.1.0` -> `0.1.0`. Reads only the name/version line: the `Git:` line comes from running
/// `git` at runtime in whatever directory the command ran in and says nothing about the build.
pub fn parse_version_output(out: &str) -> Option<String> {
    out.lines().map(str::trim).find_map(|l| {
        let rest = l.strip_prefix("blvm ")?;
        let v = rest.split_whitespace().next()?;
        let first = v.chars().next()?;
        (first.is_ascii_digit() || first == 'v').then(|| v.trim_start_matches('v').to_string())
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    pub path: PathBuf,
    pub mtime: Option<SystemTime>,
    /// `blvm 0.1.0`.
    pub self_report: Option<String>,
    /// `0.1.0`.
    pub version: Option<String>,
    pub error: Option<String>,
}

/// Remove terminal escape sequences (colours, cursor moves, window titles) and other control characters
/// from a log line: the node writes ANSI colours into `node.log`, and the Logs page is not a terminal.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.peek().copied() {
                // CSI: ESC [ parameters, intermediates, one final byte 0x40..=0x7E.
                Some('[') => {
                    it.next();
                    for n in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC / DCS / PM / APC: up to BEL or ST (ESC \).
                Some(']' | 'P' | '^' | '_') => {
                    it.next();
                    while let Some(n) = it.next() {
                        if n == '\u{7}' {
                            break;
                        }
                        if n == '\u{1b}' {
                            if it.peek() == Some(&'\\') {
                                it.next();
                            }
                            break;
                        }
                    }
                }
                // Character-set selection takes one more byte: ESC ( B.
                Some('(' | ')' | '*' | '+') => {
                    it.next();
                    it.next();
                }
                // Any other two-byte escape.
                Some(_) => {
                    it.next();
                }
                None => {}
            },
            '\t' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LogLine {
    pub seq: u64,
    pub text: String,
}

/// What the status page sees of the process (`managed.process`, plus extras).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcessView {
    pub state: ProcState,
    pub pid: Option<u32>,
    /// Unix seconds the current state began (uptime base while running).
    pub since: Option<u64>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub restarts: u32,
    pub last_error: Option<String>,
    /// The start failed because another program holds the node's RPC or P2P port (`last_error` says which).
    pub conflict: bool,
    /// Something the console did to the node on its own that the user should know about, shown while the
    /// node runs (for example: the node was restarted to turn on its RPC password). Cleared by the next
    /// Start or Stop.
    pub notice: Option<String>,
    /// Last 50 log lines when the process crashed.
    #[serde(skip)]
    pub crash_log: Vec<String>,
    #[serde(skip)]
    pub adopted: bool,
    #[serde(skip)]
    pub network: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Exit {
    code: Option<i32>,
    signal: Option<i32>,
}

fn exit_of(s: std::process::ExitStatus) -> Exit {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        Exit { code: s.code(), signal: s.signal() }
    }
    #[cfg(not(unix))]
    {
        Exit { code: s.code(), signal: None }
    }
}

fn signal_name(sig: i32) -> String {
    match sig {
        1 => "SIGHUP".into(),
        2 => "SIGINT".into(),
        6 => "SIGABRT".into(),
        9 => "SIGKILL".into(),
        11 => "SIGSEGV".into(),
        15 => "SIGTERM".into(),
        n => format!("signal {n}"),
    }
}

fn describe_exit(e: Exit) -> String {
    match (e.code, e.signal) {
        (Some(c), _) => format!("Exited with code {c}"),
        (None, Some(s)) => format!("Killed by signal {s} ({})", signal_name(s)),
        (None, None) => "Exited (status unknown)".into(),
    }
}

#[derive(Debug)]
struct St {
    state: ProcState,
    pid: Option<u32>,
    since: Option<u64>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    restarts: u32,
    last_error: Option<String>,
    crash_log: Vec<String>,
    adopted: bool,
    /// Bumped whenever a new process is claimed or supervision is dropped; stale tasks see a
    /// different value and stand down.
    epoch: u64,
    /// The user wants the node running (false after Stop). Automatic restarts need it.
    want_run: bool,
    pending_restart: bool,
    /// Set when the console itself decided the start failed (RPC never answered).
    fail_reason: Option<String>,
    stop_requested: bool,
    kill_note: Option<String>,
    crash_times: Vec<Instant>,
    network: Option<&'static str>,
    /// Process group the node leads (its pid), killed whole when the node crashes.
    group: Option<u32>,
    conflict: bool,
    /// See [`ProcessView::notice`].
    notice: Option<String>,
    /// The notice the restart that is queued behind a stop will show once the new process is claimed.
    notice_next: Option<String>,
}

impl St {
    fn new() -> Self {
        Self {
            state: ProcState::Stopped,
            pid: None,
            since: None,
            exit_code: None,
            exit_signal: None,
            restarts: 0,
            last_error: None,
            crash_log: Vec::new(),
            adopted: false,
            epoch: 0,
            want_run: false,
            pending_restart: false,
            fail_reason: None,
            stop_requested: false,
            kill_note: None,
            crash_times: Vec::new(),
            network: None,
            group: None,
            conflict: false,
            notice: None,
            notice_next: None,
        }
    }
}

struct Ring {
    lines: VecDeque<LogLine>,
    next_seq: u64,
    path: Option<PathBuf>,
    offset: u64,
    partial: Vec<u8>,
    preloaded: bool,
    skip_to_newline: bool,
}

impl Ring {
    fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            next_seq: 1,
            path: None,
            offset: 0,
            partial: Vec::new(),
            preloaded: false,
            skip_to_newline: false,
        }
    }

    fn push(&mut self, raw: &[u8]) {
        let mut text = strip_ansi(String::from_utf8_lossy(raw).trim_end_matches('\r'));
        if text.chars().count() > MAX_LINE_CHARS {
            text = text.chars().take(MAX_LINE_CHARS).collect::<String>() + "…";
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.lines.push_back(LogLine { seq, text });
        while self.lines.len() > RING_CAP {
            self.lines.pop_front();
        }
    }
}

struct Inner {
    cfg: Mutex<AgentConfig>,
    timings: Timings,
    st: Mutex<St>,
    ring: Mutex<Ring>,
    rpc_ok: Arc<AtomicBool>,
    ready: Arc<dyn Fn() -> bool + Send + Sync>,
    tailer_started: AtomicBool,
    installed: Mutex<Option<Installed>>,
    refreshing: AtomicBool,
    /// Height of the node's chain from the poller's last successful poll of the running network.
    height: Mutex<Option<HeightNote>>,
    /// Supervision was dropped (console quitting): no new process may be started by this agent.
    closed: AtomicBool,
    /// The console is deleting chain data: no start, restart or automatic restart may go ahead until it lets go
    /// (see [`Agent::hold_starts`]).
    hold_starts: AtomicBool,
    /// An agent in another container does the supervising (see [`Agent::external`]).
    external: bool,
    /// SIGTERM to SIGKILL, when the controlling console asked for something other than
    /// [`Timings::stop_wait`] (`stop_grace_secs` in `desired.json`).
    stop_wait_override: Mutex<Option<Duration>>,
    /// The console has a login for the node that it created in this run (an install from before logins): a
    /// node it adopts that still answers without one is restarted with the login instead of being stopped.
    /// See [`Agent::restart_adopted_node_with_login`].
    upgrade_login: AtomicBool,
    /// Ask a node that was given a password whether it really wants one (see [`Inner::check_login`]). Off only
    /// in tests whose stand-in node has no RPC server to ask.
    login_check: AtomicBool,
}

#[derive(Debug, Clone, Copy)]
struct HeightNote {
    network: &'static str,
    height: u64,
    header: u64,
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
    ext: Option<Arc<external::Ext>>,
}

/// What [`Inner::check_login`] found.
enum LoginCheck {
    /// Nothing to object to (no login was given, or the node asks for it): carry on.
    Fine,
    /// The node could not be asked just now.
    Later,
    /// The node is being stopped (or this watch is stale): stand down.
    Gone,
}

/// Shown on the Node card, and written to the log, after an adopted node was restarted with a login.
pub const LOGIN_RESTART_NOTICE: &str = "Restarted the node to turn on its RPC password.";

enum Claim {
    Go(u64),
    AlreadyUp,
    Busy,
    Cancelled,
}

impl Agent {
    pub fn new(cfg: AgentConfig, timings: Timings) -> Self {
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = Arc::clone(&flag);
        Self::build(cfg, timings, flag, Arc::new(move || f2.load(Ordering::Relaxed)), false)
    }

    /// A client for the agent that runs in the node container (`BLVM_UI_AGENT=external`): it spawns
    /// nothing. Start, stop and restart write `control/desired.json`; the process view comes from
    /// `control/state.json`; logs are the shared log file.
    pub fn external(cfg: AgentConfig, timings: Timings) -> Self {
        let flag = Arc::new(AtomicBool::new(false));
        let f2 = Arc::clone(&flag);
        let mut a = Self::build(cfg, timings, flag, Arc::new(move || f2.load(Ordering::Relaxed)), true);
        a.ext = Some(Arc::new(external::Ext::new()));
        a
    }

    pub fn is_external(&self) -> bool {
        self.ext.is_some()
    }

    /// Whether the node program exists, as far as the agent that runs it can tell (`None`: this agent is
    /// in-process, and the caller can look at the file itself, or no agent has reported yet).
    pub fn binary_found(&self) -> Option<bool> {
        self.ext.as_ref().and_then(|e| e.binary_found(&self.inner))
    }

    /// The page size of the computer the node runs on, as the agent in another container reported it in
    /// `state.json` (`None`: in-process agent, an older agent, or none has reported yet).
    pub fn platform_facts(&self) -> Option<crate::sysinfo::Facts> {
        self.ext.as_ref().and_then(|e| e.platform_facts(&self.inner))
    }

    /// The agent process is there and writing its heartbeat (always true for an in-process agent).
    pub fn alive(&self) -> bool {
        match &self.ext {
            Some(e) => e.alive(&self.inner),
            None => true,
        }
    }

    /// The refusal to answer a change that needs a stopped node with, when the agent in another container has gone
    /// quiet (see `Ext::quiet`); `None` for an in-process agent and for one that answers.
    pub fn quiet_refusal(&self) -> Option<&'static str> {
        match &self.ext {
            Some(e) if e.quiet(&self.inner) => Some(external::NOT_ANSWERING),
            _ => None,
        }
    }

    /// The console has just given the node a login it did not have before (it was saved by the caller, and the
    /// agent's config carries it). If the node adopted next still answers without a password, it is an
    /// install from before logins: restart it once with the config file instead of stopping it as broken.
    pub fn restart_adopted_node_with_login(&self, on: bool) {
        self.inner.upgrade_login.store(on, Ordering::SeqCst);
    }

    /// Tests: the stand-in node has no RPC server, so there is nothing to ask about its login.
    #[cfg(test)]
    pub fn skip_login_check(&self) {
        self.inner.login_check.store(false, Ordering::SeqCst);
    }

    /// Use `d` instead of [`Timings::stop_wait`] from now on (the agent daemon passes `stop_grace_secs`).
    pub fn set_stop_wait(&self, d: Duration) {
        *self.inner.stop_wait_override.lock().unwrap() = Some(d);
    }

    /// Readiness decided by `ready` instead of the poller's flag (tests).
    pub fn with_ready(
        cfg: AgentConfig,
        timings: Timings,
        ready: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Self {
        Self::build(cfg, timings, Arc::new(AtomicBool::new(false)), ready, false)
    }

    fn build(
        cfg: AgentConfig,
        timings: Timings,
        rpc_ok: Arc<AtomicBool>,
        ready: Arc<dyn Fn() -> bool + Send + Sync>,
        external: bool,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                cfg: Mutex::new(cfg),
                timings,
                st: Mutex::new(St::new()),
                ring: Mutex::new(Ring::new()),
                rpc_ok,
                ready,
                tailer_started: AtomicBool::new(false),
                installed: Mutex::new(None),
                refreshing: AtomicBool::new(false),
                height: Mutex::new(None),
                closed: AtomicBool::new(false),
                hold_starts: AtomicBool::new(false),
                external,
                stop_wait_override: Mutex::new(None),
                upgrade_login: AtomicBool::new(false),
                login_check: AtomicBool::new(true),
            }),
            ext: None,
        }
    }

    /// The poller sets this to whether the node's RPC answered on its last poll.
    pub fn rpc_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.inner.rpc_ok)
    }

    /// The poller reports the height it just read from the node's RPC at `rpc`. Taken only while that
    /// address is the one this agent's node listens on and the node is up; the value goes into
    /// `meta.json` (`last_height`).
    pub fn note_height(&self, rpc: &str, height: u64, header: u64) {
        let want = self.inner.cfg.lock().unwrap().rpc();
        if want.as_deref() != Some(rpc) {
            return;
        }
        let (state, net) = match &self.ext {
            Some(_) => {
                let v = self.view();
                (v.state, v.network)
            }
            None => {
                let st = self.inner.st.lock().unwrap();
                (st.state, st.network)
            }
        };
        if !matches!(state, ProcState::Starting | ProcState::Running) {
            return;
        }
        if let Some(network) = net {
            *self.inner.height.lock().unwrap() = Some(HeightNote { network, height, header });
            if let Some(e) = &self.ext {
                // The agent in the other container cannot see the height; the console keeps `meta.json` fresh.
                e.touch_meta(&self.inner, network);
            }
        }
    }

    pub fn config(&self) -> AgentConfig {
        self.inner.cfg.lock().unwrap().clone()
    }

    pub fn set_config(&self, cfg: AgentConfig) {
        *self.inner.cfg.lock().unwrap() = cfg;
    }

    /// With the agent in another container: save the current config into `desired.json` without restarting
    /// anything (see `Ext::sync_desired`). Nothing to do for an agent in this process, which keeps the config
    /// in memory.
    pub fn sync_desired(&self) {
        if let Some(e) = &self.ext {
            e.sync_desired(&self.inner);
        }
    }

    pub fn view(&self) -> ProcessView {
        if let Some(e) = &self.ext {
            return e.view(&self.inner);
        }
        let st = self.inner.st.lock().unwrap();
        ProcessView {
            state: st.state,
            pid: st.pid,
            since: st.since,
            exit_code: st.exit_code,
            exit_signal: st.exit_signal,
            restarts: st.restarts,
            last_error: st.last_error.clone(),
            conflict: st.conflict,
            notice: st.notice.clone(),
            crash_log: st.crash_log.clone(),
            adopted: st.adopted,
            network: st.network,
        }
    }

    pub fn state(&self) -> ProcState {
        if self.ext.is_some() {
            return self.view().state;
        }
        self.inner.st.lock().unwrap().state
    }

    pub fn installed(&self) -> Option<Installed> {
        if let Some(e) = &self.ext {
            return e.installed(&self.inner);
        }
        self.inner.installed.lock().unwrap().clone()
    }

    pub fn logs(&self, after: Option<u64>, limit: usize) -> Vec<LogLine> {
        if self.ext.is_some() {
            self.inner.ensure_tailer();
            // Pick up what the node wrote since the last tick, so the first request is not empty.
            self.inner.tail_once(false);
        }
        self.inner.logs(after, limit)
    }

    /// Start the node. Returns at once with `Ok` when it is already up.
    pub async fn start(&self) -> Result<(), String> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err("The console is shutting down.".into());
        }
        if self.inner.hold_starts.load(Ordering::SeqCst) {
            return Err(WIPING_MSG.into());
        }
        if let Some(e) = &self.ext {
            return e.start(&self.inner);
        }
        match self.inner.claim(None) {
            Claim::Go(epoch) => {
                {
                    let mut st = self.inner.st.lock().unwrap();
                    st.restarts = 0;
                    st.crash_times.clear();
                }
                self.inner.spawn_node(epoch).await
            }
            Claim::AlreadyUp => Ok(()),
            Claim::Busy => Err("The node is still stopping. Try again in a moment.".into()),
            Claim::Cancelled => Ok(()),
        }
    }

    /// Ask the node to stop (SIGTERM) and return at once. The state shows `stopping` until it exits;
    /// SIGKILL follows after [`Timings::stop_wait`].
    pub fn request_stop(&self) {
        if let Some(e) = &self.ext {
            e.request_stop(&self.inner);
            return;
        }
        self.inner.request_stop();
    }

    /// [`Self::request_stop`], then wait until the process is gone.
    pub async fn stop_and_wait(&self) {
        self.request_stop();
        self.wait_inactive(self.stop_limit()).await;
    }

    /// Wait up to `limit` until no process is active. `true` when it is gone.
    pub async fn wait_inactive(&self, limit: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        while tokio::time::Instant::now() < deadline {
            if !self.state().is_active() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        !self.state().is_active()
    }

    /// How long a network switch waits for the old node to go away (see [`Timings::stop_give_up`]).
    pub fn stop_limit(&self) -> Duration {
        match *self.inner.stop_wait_override.lock().unwrap() {
            Some(w) if self.inner.timings.stop_give_up.is_none() => {
                w + self.inner.timings.kill_wait + Duration::from_secs(2)
            }
            _ => self.inner.timings.stop_limit(),
        }
    }

    /// The longest a fresh start may take before the agent gives up on it (a node that keeps writing its
    /// log is waited for beyond the first [`Timings::start_timeout`]).
    pub fn start_limit(&self) -> Duration {
        self.inner.timings.start_max.max(self.inner.timings.start_timeout)
    }

    /// Forget a restart that a `restart()` queued behind a stop (a network switch replaces it).
    pub fn cancel_restart(&self) {
        if self.ext.is_some() {
            return;
        }
        self.inner.st.lock().unwrap().pending_restart = false;
    }

    /// A crashed node is waiting for its automatic restart: do not restart it. The state stays
    /// `crashed` with its log; the error text no longer promises a restart.
    pub fn abandon_restart(&self) {
        if let Some(e) = &self.ext {
            e.abandon_restart(&self.inner);
            return;
        }
        let mut st = self.inner.st.lock().unwrap();
        if st.state != ProcState::Crashed {
            return;
        }
        st.want_run = false;
        if let Some(e) = st.last_error.as_mut() {
            if let Some(i) = e.find(". Restarting in") {
                e.truncate(i);
                e.push('.');
            }
        }
    }

    /// Stop, then start again. Returns at once.
    pub async fn restart(&self) -> Result<(), String> {
        if self.inner.hold_starts.load(Ordering::SeqCst) {
            return Err(WIPING_MSG.into());
        }
        if let Some(e) = &self.ext {
            return e.restart(&self.inner);
        }
        let must_stop = {
            let mut st = self.inner.st.lock().unwrap();
            match st.state {
                ProcState::Starting | ProcState::Running | ProcState::Stopping => {
                    st.pending_restart = true;
                    true
                }
                ProcState::Stopped | ProcState::Crashed => false,
            }
        };
        if must_stop {
            self.inner.request_stop();
            Ok(())
        } else {
            self.start().await
        }
    }

    /// While the console deletes chain data nothing may start a node on it: `true` refuses every start, restart
    /// and automatic restart of this agent until `false` is passed again.
    pub fn hold_starts(&self, on: bool) {
        self.inner.hold_starts.store(on, Ordering::SeqCst);
    }

    /// Can the chain data be deleted now? `Ok` only when no node process exists. With the agent in another
    /// container the console cannot see the process, so it also needs a fresh heartbeat and a `state.json` that
    /// belongs to the last request the console wrote (see `Ext::confirm_stopped`).
    pub fn confirm_stopped(&self) -> Result<(), String> {
        if let Some(e) = &self.ext {
            return e.confirm_stopped(&self.inner);
        }
        if self.state().is_active() {
            return Err(STOP_FIRST_MSG.into());
        }
        Ok(())
    }

    /// Pick up a node that survived a console crash. `true` when one was adopted.
    pub fn adopt(&self) -> bool {
        if self.ext.is_some() {
            // The agent in the node container adopts; all the console can do is see that it did.
            return self.view().state.is_active();
        }
        self.inner.adopt()
    }

    /// Drop supervision without touching the process (console exits and leaves the node running).
    pub fn detach(&self) {
        // From now on nothing (a network switch in flight, a queued restart) may start another process.
        self.inner.closed.store(true, Ordering::SeqCst);
        if self.ext.is_some() {
            return;
        }
        let mut st = self.inner.st.lock().unwrap();
        st.epoch += 1;
        st.want_run = false;
    }

    /// Console shutdown: stop the node and wait, or leave it running.
    pub async fn shutdown(&self, stop_node: bool) {
        // First forbid new starts, so a network switch that is mid-way cannot spawn a node while the
        // console is leaving.
        self.inner.closed.store(true, Ordering::SeqCst);
        // An agent in another container outlives this console by design: never stop its node on the way out.
        if stop_node && self.ext.is_none() && self.state().is_active() {
            self.stop_and_wait().await;
        }
        self.detach();
    }

    /// Re-read `<bin> version` when the binary changed (or was never read). Cheap when nothing changed.
    pub async fn refresh_installed(&self, force: bool) {
        if self.ext.is_some() {
            return; // the agent that runs the program reads its version (state.json)
        }
        self.inner.refresh_installed(force).await;
    }

    /// `true` when the binary's path or mtime differs from the cached `version` read.
    pub fn installed_is_stale(&self) -> bool {
        if self.ext.is_some() {
            return false;
        }
        self.inner.installed_is_stale()
    }
}

// ---------------------------------------------------------------------------------------------

impl Inner {
    fn now() -> u64 {
        crate::persist::unix_now()
    }

    /// How long a SIGTERM may take before SIGKILL.
    fn stop_wait(&self) -> Duration {
        self.stop_wait_override.lock().unwrap().unwrap_or(self.timings.stop_wait)
    }

    /// Decide whether a start may go ahead and, if so, take the slot (state `starting`, no pid yet).
    /// `auto` carries the epoch of a pending automatic restart, which only runs while it is still current.
    fn claim(&self, auto: Option<u64>) -> Claim {
        if self.closed.load(Ordering::SeqCst) {
            return Claim::Cancelled;
        }
        let mut st = self.st.lock().unwrap();
        // Looked at under the state lock: `Agent::confirm_stopped` reads the state under it too, so a start
        // either took the slot before that look (and the wipe is refused) or sees the hold already set.
        if self.hold_starts.load(Ordering::SeqCst) {
            return Claim::Cancelled;
        }
        if let Some(e) = auto {
            if st.epoch != e || st.state != ProcState::Crashed || !st.want_run {
                return Claim::Cancelled;
            }
        } else {
            match st.state {
                ProcState::Running | ProcState::Starting => return Claim::AlreadyUp,
                ProcState::Stopping => return Claim::Busy,
                ProcState::Stopped | ProcState::Crashed => {}
            }
        }
        st.epoch += 1;
        st.state = ProcState::Starting;
        st.pid = None;
        st.since = Some(Self::now());
        st.want_run = true;
        st.pending_restart = false;
        st.stop_requested = false;
        st.fail_reason = None;
        st.kill_note = None;
        st.exit_code = None;
        st.exit_signal = None;
        st.last_error = None;
        st.conflict = false;
        st.group = None;
        st.crash_log.clear();
        st.adopted = false;
        st.notice = None;
        self.upgrade_login.store(false, Ordering::SeqCst);
        Claim::Go(st.epoch)
    }

    fn fail_start(&self, epoch: u64, msg: String) -> Result<(), String> {
        let mut st = self.st.lock().unwrap();
        if st.epoch == epoch {
            st.state = ProcState::Crashed;
            st.pid = None;
            st.want_run = false;
            st.last_error = Some(msg.clone());
        }
        Err(msg)
    }

    /// A start that was refused because someone else holds one of our ports.
    fn fail_conflict(&self, epoch: u64, msg: String) -> Result<(), String> {
        {
            let mut st = self.st.lock().unwrap();
            if st.epoch == epoch {
                st.conflict = true;
            }
        }
        self.fail_start(epoch, msg)
    }

    async fn spawn_node(self: &Arc<Self>, epoch: u64) -> Result<(), String> {
        let cfg = self.cfg.lock().unwrap().clone();
        let Some(net) = cfg.network else {
            return self.fail_start(epoch, "Choose a network first.".into());
        };
        let Some(bin) = cfg.bin.clone() else {
            return self.fail_start(
                epoch,
                "No node binary configured. Set BLVM_UI_NODE_BIN or choose one in setup.".into(),
            );
        };
        if !bin.is_file() {
            return self.fail_start(epoch, format!("Node binary not found: {}", bin.display()));
        }
        // Another program on our RPC or P2P port: the node would fail to bind and keep running without a
        // server, and the console would take the other program's answers for its own.
        if let Some(msg) = port_conflict(&cfg, net).await {
            return self.fail_conflict(epoch, msg);
        }
        let dir = networks::dir(&cfg.root, net);
        let log_path = networks::log_file(&cfg.root, net);
        if let Err(e) = std::fs::create_dir_all(log_path.parent().unwrap_or(&dir))
            .and_then(|_| std::fs::create_dir_all(cfg.root.join("control")))
        {
            return self.fail_start(epoch, format!("Cannot create {}: {e}", dir.display()));
        }
        let _ = rotate_log(&log_path, self.timings.log_max_bytes);
        if cfg.rpc_auth.is_none() && cfg.rpc().as_deref().is_some_and(rpc_exposed) {
            return self.fail_start(
                epoch,
                "The node's RPC would be reachable from other containers without a login, so it was not started. \
                 Set BLVM_RPC_USER and BLVM_RPC_PASS for the node container."
                    .into(),
            );
        }
        // The config file is written before every start, with or without a login: the node reads `./blvm.toml`
        // from its working folder even when it is not told which file to use, so a file left from an earlier
        // start (with a login, or with pruning) must never be applied by accident.
        if let Err(e) = write_node_config(&dir, cfg.rpc_auth.as_ref(), cfg.prune_keep_blocks) {
            return self.fail_start(epoch, format!("Cannot write {}: {e}", dir.join(NODE_CONFIG_FILE).display()));
        }
        // Remember in `meta.json` that this folder has been used by a pruning node: the node starts deleting old
        // blocks from then on, and going back to a full node needs the chain data deleted (see `ran_pruned`).
        // Written before the node exists, and cleared only by the wipe. The same code runs in the agent container.
        if cfg.prune_keep_blocks.is_some() {
            let mut m = networks::read_meta(&cfg.root, net).unwrap_or_default();
            if m.ran_pruned != Some(true) {
                m.ran_pruned = Some(true);
                if let Err(e) = networks::write_meta(&cfg.root, net, &m) {
                    return self.fail_start(
                        epoch,
                        format!("Cannot write {}: {e}", networks::meta_file(&cfg.root, net).display()),
                    );
                }
            }
        }
        let args = build_args(&cfg, net);
        let mut log = match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(f) => f,
            Err(e) => return self.fail_start(epoch, format!("Cannot open {}: {e}", log_path.display())),
        };
        let _ = writeln!(
            log,
            "[console] starting {} {} (cwd {})",
            bin.display(),
            args.join(" "),
            dir.display()
        );
        let (out, err) = match (log.try_clone(), log.try_clone()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return self.fail_start(epoch, "Cannot open the log file twice.".into()),
        };

        let mut cmd = Command::new(&bin);
        cmd.args(&args)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .kill_on_drop(false);
        // The console's own settings (RPC login included) are none of the node's business.
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("BLVM_UI_") {
                cmd.env_remove(&k);
            }
        }
        // `RUST_LOG` is the console's own log filter (the Docker image sets `blvm_ui_next=info`). The node reads
        // the same variable, and a filter that names only the console's crate silences every line the node
        // writes: the Logs page stayed empty. The node gets one only through the Child environment setting.
        cmd.env_remove("RUST_LOG");
        // The node reads `BLVM_ASSUME_VALID_HEIGHT` before the command line and the config file, so one
        // inherited from the console's own environment would silently override the user's assume-valid
        // choice. The node gets one only through the Child environment setting.
        cmd.env_remove("BLVM_ASSUME_VALID_HEIGHT");
        for (k, v) in child_env(&cfg) {
            match v {
                Some(v) => {
                    cmd.env(k, v);
                }
                None => {
                    cmd.env_remove(k);
                }
            }
        }
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        // A reading from before this start must not count as "the new node answers".
        self.rpc_ok.store(false, Ordering::Relaxed);
        *self.height.lock().unwrap() = None;
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return self.fail_start(epoch, format!("Could not start {}: {e}", bin.display())),
        };
        let Some(pid) = child.id() else {
            return self.fail_start(epoch, "The node exited before it could be watched.".into());
        };
        {
            let mut st = self.st.lock().unwrap();
            if st.epoch != epoch {
                // Cancelled while spawning (stop or detach during start): do not leave it running unwatched.
                send_signal(pid, SIGTERM);
                return Ok(());
            }
            st.pid = Some(pid);
            st.group = Some(pid); // `process_group(0)`: the node leads its own group
            st.since = Some(Self::now());
            st.network = Some(net.id);
        }
        write_pid_file(&cfg.root, pid, net.id, &bin);
        #[cfg(all(test, unix))]
        testkit::note_spawn(pid, &cfg.root);
        self.write_meta(net.id);
        tracing::info!(pid, network = net.id, bin = %bin.display(), "node agent: started");
        self.ensure_tailer();
        let this = Arc::clone(self);
        tokio::spawn(async move { this.supervise(epoch, pid, Some(child)).await });
        let this = Arc::clone(self);
        tokio::spawn(async move { this.watch_ready(epoch).await });
        let this = Arc::clone(self);
        tokio::spawn(async move { this.refresh_installed(false).await });
        Ok(())
    }

    /// Refresh `networks/<net>/meta.json`: first use, last use, and the last height the poller saw on
    /// this network. Failures are not worth interrupting the node for.
    fn write_meta(&self, net: &'static str) {
        let Some(n) = networks::by_id(net) else { return };
        let root = self.cfg.lock().unwrap().root.clone();
        if !networks::dir(&root, n).is_dir() {
            return;
        }
        let now = Self::now();
        let mut m = networks::read_meta(&root, n).unwrap_or_default();
        m.created.get_or_insert(now);
        m.last_used = Some(now);
        if let Some(h) = *self.height.lock().unwrap() {
            // A node that briefly reports 0 while it re-validates must not wipe a real height.
            if h.network == net && (h.height > 0 || m.last_height.is_none()) {
                m.last_height = Some(h.height);
                if h.header > 0 {
                    m.last_header = Some(h.header);
                }
            }
        }
        if let Err(e) = networks::write_meta(&root, n, &m) {
            tracing::debug!("node agent: cannot write meta.json for {net}: {e}");
        }
    }

    /// Waits for the process to end, then records how.
    async fn supervise(self: Arc<Self>, epoch: u64, pid: u32, child: Option<tokio::process::Child>) {
        let exit = match child {
            Some(mut c) => match c.wait().await {
                Ok(s) => exit_of(s),
                Err(_) => Exit::default(),
            },
            None => {
                while pid_alive(pid) {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                Exit::default()
            }
        };
        self.on_exit(epoch, exit);
    }

    fn on_exit(self: &Arc<Self>, epoch: u64, exit: Exit) {
        // Pick up the last words before reading them back.
        self.tail_once(true);
        let tail = self.last_lines(CRASH_LOG_LINES);
        let cfg = self.cfg.lock().unwrap().clone();
        let mut restart_after: Option<Duration> = None;
        let mut restart_now = false;
        let mut kill_group_of: Option<u32> = None;
        let net_id;
        let carried_notice: Option<String>;
        {
            let mut st = self.st.lock().unwrap();
            if st.epoch != epoch {
                return;
            }
            carried_notice = st.notice_next.take();
            net_id = st.network;
            let group = st.group.take();
            st.pid = None;
            st.since = Some(Self::now());
            st.exit_code = exit.code;
            st.exit_signal = exit.signal;
            st.adopted = false;
            remove_pid_file(&cfg.root);
            if let Some(reason) = st.fail_reason.take() {
                kill_group_of = group;
                st.state = ProcState::Crashed;
                st.want_run = false;
                st.last_error = Some(reason);
                st.crash_log = tail;
                st.pending_restart = false;
            } else if st.stop_requested {
                st.state = ProcState::Stopped;
                st.stop_requested = false;
                st.last_error = st.kill_note.take();
                st.crash_log.clear();
                if st.pending_restart {
                    st.pending_restart = false;
                    restart_now = true;
                }
            } else if exit.code == Some(0) {
                // The node ended itself cleanly (for example an RPC `stop`).
                st.state = ProcState::Stopped;
                st.last_error = None;
                st.want_run = false;
            } else {
                // A crash: the node's helpers (modules) are not to outlive it, or the automatic restart
                // would start a second set next to the old one.
                kill_group_of = group;
                st.state = ProcState::Crashed;
                st.crash_log = tail;
                let window = self.timings.crash_window;
                st.crash_times.retain(|t| t.elapsed() < window);
                st.crash_times.push(Instant::now());
                let n = st.crash_times.len();
                let what = describe_exit(exit);
                if st.want_run && n <= self.timings.backoff.len() {
                    let delay = self.timings.backoff[n - 1];
                    st.last_error = Some(format!(
                        "{what}. Restarting in {}s (try {n} of {}).",
                        delay.as_secs().max(1),
                        self.timings.backoff.len()
                    ));
                    restart_after = Some(delay);
                } else {
                    st.want_run = false;
                    st.last_error = Some(format!(
                        "{what}. It crashed {n} time{} in {} minutes, so the console stopped restarting it.",
                        if n == 1 { "" } else { "s" },
                        (window.as_secs() / 60).max(1)
                    ));
                }
            }
        }
        if let Some(g) = kill_group_of {
            kill_group(g);
        }
        if let Some(n) = net_id {
            self.write_meta(n);
        }
        if restart_now {
            let this = Arc::clone(self);
            tokio::spawn(async move {
                if let Claim::Go(e) = this.claim(None) {
                    if let Some(n) = carried_notice {
                        this.log_console(&n);
                        this.st.lock().unwrap().notice = Some(n);
                    }
                    let _ = this.spawn_node(e).await;
                }
            });
        }
        if let Some(delay) = restart_after {
            let this = Arc::clone(self);
            tokio::spawn(async move {
                tokio::time::sleep(delay).await;
                if let Claim::Go(e) = this.claim(Some(epoch)) {
                    {
                        let mut st = this.st.lock().unwrap();
                        st.restarts += 1;
                    }
                    let _ = this.spawn_node(e).await;
                }
            });
        }
    }

    /// Size of the node's log file (changes whenever the node writes, or the file is rotated).
    fn log_len(&self) -> Option<u64> {
        let path = self.cfg.lock().unwrap().log_path()?;
        std::fs::metadata(path).ok().map(|m| m.len())
    }

    /// Does somebody other than our node (or its children) hold the RPC port? Returns the message to show.
    async fn rpc_conflict(&self) -> Option<String> {
        let port = self.cfg.lock().unwrap().rpc().as_deref().and_then(port_of)?;
        let ours = self.st.lock().unwrap().pid;
        let (pid, name) = tokio::task::spawn_blocking(move || crate::procinfo::foreign_listener(port, ours))
            .await
            .ok()??;
        Some(in_use_message("RPC", port, pid, &name))
    }

    /// Our node is alive but somebody else answers on its port: it is not "running". Stop it (it has no
    /// RPC server of its own) and report the conflict; no automatic restart.
    fn conflict_found(self: &Arc<Self>, epoch: u64, msg: String) {
        {
            let mut st = self.st.lock().unwrap();
            if st.epoch != epoch || !matches!(st.state, ProcState::Starting | ProcState::Running) {
                return;
            }
            st.fail_reason = Some(msg);
            st.conflict = true;
        }
        self.request_stop();
    }

    /// Make sure a node that was given a password really asks for it. Only an explicit 401/403 to a caller
    /// with no login counts as "asks"; a dropped connection or a timeout is no answer (the node limits the
    /// connections of one address), so it is asked again after a while instead of being taken for locked.
    async fn check_login(self: &Arc<Self>, epoch: u64, last_probe: &mut Option<Instant>) -> LoginCheck {
        let (locked, addr) = {
            let c = self.cfg.lock().unwrap();
            (c.rpc_auth.is_some(), c.rpc())
        };
        let Some(addr) = addr.filter(|_| locked && self.login_check.load(Ordering::SeqCst)) else {
            return LoginCheck::Fine;
        };
        let gap = self.timings.ready_poll * 12;
        if last_probe.is_some_and(|t| t.elapsed() < gap) {
            return LoginCheck::Later;
        }
        *last_probe = Some(Instant::now());
        match crate::rpc::probe_login(&probe_addr(&addr)).await {
            crate::rpc::LoginProbe::Locked => {
                self.upgrade_login.store(false, Ordering::SeqCst);
                LoginCheck::Fine
            }
            crate::rpc::LoginProbe::Unknown => LoginCheck::Later,
            crate::rpc::LoginProbe::Open => {
                // The node still answers a caller who has no password: it did not apply the config file
                // (the node falls back to "no login" when it cannot read it), or it was started before this
                // console gave nodes a login. Never leave a node like that running.
                let upgrade = {
                    let mut st = self.st.lock().unwrap();
                    if st.epoch != epoch || st.state != ProcState::Starting {
                        return LoginCheck::Gone;
                    }
                    if st.adopted && self.upgrade_login.swap(false, Ordering::SeqCst) {
                        st.pending_restart = true;
                        st.notice_next = Some(LOGIN_RESTART_NOTICE.into());
                        true
                    } else {
                        st.fail_reason = Some(
                            "The node answered without asking for the RPC password, so the console stopped it. \
                             See the node log for why it did not read its settings file."
                                .into(),
                        );
                        false
                    }
                };
                if upgrade {
                    tracing::info!("node agent: the adopted node has no RPC password; restarting it with one");
                }
                self.request_stop();
                LoginCheck::Gone
            }
        }
    }

    /// One line in the node's log file, marked as the console's.
    fn log_console(&self, text: &str) {
        let Some(path) = self.cfg.lock().unwrap().log_path() else { return };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "[console] {text}");
        }
    }

    /// `starting` -> `running` when the RPC answers and the answering listener is ours; then keeps
    /// checking that it stays ours. A silent start is waited for while the node is alive and writing its
    /// log (see [`Timings::start_timeout`] and [`Timings::start_max`]).
    async fn watch_ready(self: Arc<Self>, epoch: u64) {
        let started = Instant::now();
        let hard = started + self.timings.start_max.max(self.timings.start_timeout);
        let mut deadline = (started + self.timings.start_timeout).min(hard);
        let mut log_mark = self.log_len();
        let mut running = false;
        let mut probed: Option<Instant> = None;
        loop {
            tokio::time::sleep(if running { self.timings.owner_check } else { self.timings.ready_poll }).await;
            {
                let st = self.st.lock().unwrap();
                if st.epoch != epoch || !matches!(st.state, ProcState::Starting | ProcState::Running) {
                    return;
                }
            }
            if running {
                if let Some(msg) = self.rpc_conflict().await {
                    self.conflict_found(epoch, msg);
                    return;
                }
                continue;
            }
            if (self.ready)() {
                if let Some(msg) = self.rpc_conflict().await {
                    self.conflict_found(epoch, msg);
                    return;
                }
                match self.check_login(epoch, &mut probed).await {
                    LoginCheck::Gone => return,
                    // The node could not be asked (see [`crate::rpc::LoginProbe::Unknown`]): not `running` yet,
                    // ask again in a while. The start deadline below still applies.
                    LoginCheck::Later => {}
                    LoginCheck::Fine => {
                        let net = {
                            let mut st = self.st.lock().unwrap();
                            if st.epoch != epoch {
                                return;
                            }
                            if st.state == ProcState::Starting {
                                st.state = ProcState::Running;
                                st.since = Some(Self::now());
                            }
                            st.network
                        };
                        if let Some(n) = net {
                            self.write_meta(n);
                        }
                        running = true;
                        continue;
                    }
                }
            }
            let now = Instant::now();
            if now >= deadline {
                // Alive and still talking: a node that replays its chain state or builds an index is slow,
                // not stuck. Wait another round, up to the cap.
                let len = self.log_len();
                if len != log_mark && now < hard {
                    log_mark = len;
                    deadline = (now + self.timings.start_timeout).min(hard);
                    continue;
                }
                {
                    let mut st = self.st.lock().unwrap();
                    if st.epoch != epoch || st.state != ProcState::Starting {
                        return;
                    }
                    st.fail_reason = Some(format!(
                        "The node started but its RPC did not answer within {} seconds{}, so the console stopped it.",
                        started.elapsed().as_secs().max(1),
                        if len != log_mark || now >= hard { "" } else { " and its log went quiet" }
                    ));
                }
                self.request_stop();
                return;
            }
        }
    }

    fn request_stop(self: &Arc<Self>) {
        let net: Option<&'static str>;
        let (pid, epoch) = {
            let mut st = self.st.lock().unwrap();
            match st.state {
                ProcState::Stopped | ProcState::Stopping => return,
                ProcState::Crashed => {
                    // Waiting to restart, or given up: either way the user wants it off.
                    st.want_run = false;
                    st.pending_restart = false;
                    st.epoch += 1;
                    st.state = ProcState::Stopped;
                    st.since = Some(Self::now());
                    st.last_error = None;
                    return;
                }
                ProcState::Starting | ProcState::Running => {}
            }
            let Some(pid) = st.pid else {
                // Claimed but not spawned yet: cancel the start.
                st.epoch += 1;
                st.state = ProcState::Stopped;
                st.want_run = false;
                return;
            };
            st.state = ProcState::Stopping;
            st.stop_requested = true;
            st.want_run = false;
            st.notice = None;
            st.since = Some(Self::now());
            net = st.network;
            (pid, st.epoch)
        };
        if let Some(n) = net {
            self.write_meta(n);
        }
        tracing::info!(pid, "node agent: SIGTERM (graceful)");
        send_signal(pid, SIGTERM);
        let this = Arc::clone(self);
        let wait = self.stop_wait();
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            {
                let mut st = this.st.lock().unwrap();
                if st.epoch != epoch || st.state != ProcState::Stopping {
                    return;
                }
                st.kill_note = Some(format!("The node did not exit within {} seconds and was killed.", wait.as_secs()));
            }
            tracing::warn!(pid, "node agent: SIGKILL after SIGTERM did not finish");
            kill_hard(pid);
        });
    }

    // ---- adoption -------------------------------------------------------------------------

    fn adopt(self: &Arc<Self>) -> bool {
        let cfg = self.cfg.lock().unwrap().clone();
        let (Some(net), Some(bin)) = (cfg.network, cfg.bin.clone()) else {
            return false;
        };
        let Some(rec) = read_pid_file(&cfg.root) else {
            return false;
        };
        if rec.network != net.id {
            tracing::warn!(
                pid = rec.pid,
                recorded = %rec.network,
                configured = net.id,
                "node agent: a node from another network is recorded; not adopting it"
            );
            // Say so on the Node card instead of leaving a second node running unannounced.
            if pid_alive(rec.pid) && is_our_binary(rec.pid, &bin) {
                let mut st = self.st.lock().unwrap();
                if !st.state.is_active() {
                    let label = networks::by_id(&rec.network).map_or(rec.network.as_str(), |n| n.label);
                    st.last_error = Some(format!(
                        "A node on {label} (pid {}) from an earlier session is still running. Switch back to {label} to manage it, or stop it yourself.",
                        rec.pid
                    ));
                }
            }
            return false;
        }
        let net_dir = networks::dir(&cfg.root, net);
        if !pid_alive(rec.pid) || !is_our_binary(rec.pid, &bin) || !runs_in(rec.pid, &net_dir) {
            remove_pid_file(&cfg.root);
            return false;
        }
        let epoch = {
            let mut st = self.st.lock().unwrap();
            if st.state.is_active() {
                return false;
            }
            st.epoch += 1;
            st.pid = Some(rec.pid);
            st.since = Some(rec.started);
            st.state = ProcState::Starting;
            st.want_run = true;
            st.adopted = true;
            st.group = pgid_if_leader(rec.pid);
            st.conflict = false;
            st.stop_requested = false;
            st.pending_restart = false;
            st.fail_reason = None;
            st.restarts = 0;
            st.exit_code = None;
            st.exit_signal = None;
            st.last_error = None;
            st.network = Some(net.id);
            st.epoch
        };
        self.write_meta(net.id);
        tracing::info!(pid = rec.pid, network = net.id, "node agent: adopted a running node");
        self.ensure_tailer();
        let this = Arc::clone(self);
        tokio::spawn(async move { this.supervise(epoch, rec.pid, None).await });
        let this = Arc::clone(self);
        tokio::spawn(async move { this.watch_ready(epoch).await });
        let this = Arc::clone(self);
        tokio::spawn(async move { this.refresh_installed(false).await });
        true
    }

    // ---- logs -----------------------------------------------------------------------------

    fn ensure_tailer(self: &Arc<Self>) {
        // Tasks need a runtime; a caller outside one (a sync test) just gets the on-demand reads.
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        if self.tailer_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak: Weak<Inner> = Arc::downgrade(self);
        let meta_weak = Weak::clone(&weak);
        let meta_every = self.timings.meta_tick;
        tokio::spawn(async move {
            // While a node runs, keep `meta.json` (last height, last use) fresh.
            loop {
                tokio::time::sleep(meta_every).await;
                let Some(inner) = meta_weak.upgrade() else { break };
                let net = {
                    let st = inner.st.lock().unwrap();
                    if st.state.is_active() { st.network } else { None }
                };
                if let Some(n) = net {
                    inner.write_meta(n);
                }
            }
        });
        let every = self.timings.tail_poll;
        tokio::spawn(async move {
            let mut n: u64 = 0;
            loop {
                tokio::time::sleep(every).await;
                let Some(inner) = weak.upgrade() else { break };
                inner.tail_once(false);
                n += 1;
                // The agent next to the node rotates the shared log; two rotators would copy it twice.
                if n % 20 == 0 && !inner.external {
                    let path = inner.cfg.lock().unwrap().log_path();
                    let max = inner.timings.log_max_bytes;
                    if let Some(p) = path {
                        let _ = tokio::task::spawn_blocking(move || rotate_log(&p, max)).await;
                    }
                }
            }
        });
    }

    /// Copy new log bytes into the ring. `flush_partial` also takes an unterminated last line.
    fn tail_once(&self, flush_partial: bool) {
        let Some(path) = self.cfg.lock().unwrap().log_path() else {
            return;
        };
        let mut r = self.ring.lock().unwrap();
        if r.path.as_ref() != Some(&path) {
            r.path = Some(path.clone());
            r.offset = 0;
            r.partial.clear();
            r.lines.clear();
            r.preloaded = false;
            r.skip_to_newline = false;
        }
        let Ok(mut f) = std::fs::File::open(&path) else {
            return;
        };
        let Ok(meta) = f.metadata() else {
            return;
        };
        let len = meta.len();
        if !r.preloaded {
            r.preloaded = true;
            let (offset, at_line_start) = preload_offset(&mut f, len);
            r.offset = offset;
            r.skip_to_newline = offset > 0 && !at_line_start;
        }
        if len < r.offset {
            // Rotated (copy-truncate) or cleared.
            r.offset = 0;
            r.partial.clear();
            r.skip_to_newline = false;
        }
        if len - r.offset > TAIL_MAX_BACKLOG {
            r.offset = len - 1024 * 1024;
            r.partial.clear();
            r.skip_to_newline = true;
        }
        let want = (len - r.offset).min(TAIL_CHUNK) as usize;
        if want > 0 {
            if f.seek(SeekFrom::Start(r.offset)).is_err() {
                return;
            }
            let mut buf = vec![0u8; want];
            let Ok(n) = f.read(&mut buf) else {
                return;
            };
            buf.truncate(n);
            r.offset += n as u64;
            let mut data = std::mem::take(&mut r.partial);
            data.extend_from_slice(&buf);
            if r.skip_to_newline {
                match data.iter().position(|b| *b == b'\n') {
                    Some(i) => {
                        data.drain(..=i);
                        r.skip_to_newline = false;
                    }
                    None => {
                        r.partial = Vec::new();
                        return;
                    }
                }
            }
            let mut start = 0;
            while let Some(i) = data[start..].iter().position(|b| *b == b'\n') {
                let line = data[start..start + i].to_vec();
                r.push(&line);
                start += i + 1;
            }
            r.partial = data[start..].to_vec();
        }
        if flush_partial && !r.partial.is_empty() {
            let line = std::mem::take(&mut r.partial);
            r.push(&line);
        }
    }

    fn last_lines(&self, n: usize) -> Vec<String> {
        let r = self.ring.lock().unwrap();
        let skip = r.lines.len().saturating_sub(n);
        r.lines.iter().skip(skip).map(|l| l.text.clone()).collect()
    }

    fn logs(&self, after: Option<u64>, limit: usize) -> Vec<LogLine> {
        let r = self.ring.lock().unwrap();
        let last = r.next_seq.saturating_sub(1);
        let limit = limit.clamp(1, 1000);
        match after {
            Some(a) if a <= last => r.lines.iter().filter(|l| l.seq > a).take(limit).cloned().collect(),
            _ => {
                let skip = r.lines.len().saturating_sub(limit);
                r.lines.iter().skip(skip).cloned().collect()
            }
        }
    }

    // ---- installed version (design M.2) ---------------------------------------------------

    fn installed_is_stale(&self) -> bool {
        let Some(bin) = self.cfg.lock().unwrap().bin.clone() else {
            return false;
        };
        let mtime = std::fs::metadata(&bin).and_then(|m| m.modified()).ok();
        match &*self.installed.lock().unwrap() {
            Some(i) => i.path != bin || i.mtime != mtime,
            None => true,
        }
    }

    async fn refresh_installed(self: &Arc<Self>, force: bool) {
        let Some(bin) = self.cfg.lock().unwrap().bin.clone() else {
            return;
        };
        if !force && !self.installed_is_stale() {
            return;
        }
        if self.refreshing.swap(true, Ordering::SeqCst) {
            return;
        }
        let mtime = std::fs::metadata(&bin).and_then(|m| m.modified()).ok();
        let res = read_version(&bin, self.timings.version_timeout).await;
        let info = match res {
            Ok(out) => {
                let version = parse_version_output(&out);
                Installed {
                    path: bin,
                    mtime,
                    self_report: version.as_ref().map(|v| format!("blvm {v}")),
                    error: version.is_none().then(|| "`version` printed no `blvm X.Y.Z` line".to_string()),
                    version,
                }
            }
            Err(e) => Installed { path: bin, mtime, self_report: None, version: None, error: Some(e) },
        };
        *self.installed.lock().unwrap() = Some(info);
        self.refreshing.store(false, Ordering::SeqCst);
    }
}

/// Runs `<bin> version` from a neutral directory. Only stdout and stderr text is returned.
async fn read_version(bin: &Path, limit: Duration) -> Result<String, String> {
    let mut cmd = Command::new(bin);
    cmd.arg("version")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(limit, cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("could not run `{} version`: {e}", bin.display())),
        Err(_) => return Err(format!("`{} version` did not finish in {}s", bin.display(), limit.as_secs())),
    };
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(text)
}

// ---- ports ------------------------------------------------------------------------------------

fn port_of(addr: &str) -> Option<u16> {
    addr.rsplit_once(':')?.1.parse().ok()
}

fn in_use_message(kind: &str, port: u16, pid: u32, name: &str) -> String {
    format!("The {kind} port {port} is in use by pid {pid} ({name}). Stop that program first, then start the node again.")
}

/// Refuse a start when the network's RPC or P2P port already has a listener. `None`: both are free (or
/// the owner cannot be found, in which case the node's own bind error is what the log will show).
async fn port_conflict(cfg: &AgentConfig, net: &'static Network) -> Option<String> {
    let rpc_port = cfg.rpc().as_deref().and_then(port_of);
    let ports = [("RPC", rpc_port), ("P2P", Some(cfg.p2p(net)))];
    for (kind, port) in ports {
        let Some(port) = port else { continue };
        let found = tokio::task::spawn_blocking(move || crate::procinfo::foreign_listener(port, None)).await.ok()?;
        if let Some((pid, name)) = found {
            return Some(in_use_message(kind, port, pid, &name));
        }
    }
    None
}

/// Where to start reading a log file the first time so that its last [`PRELOAD_LINES`] lines are included: the
/// start of the line that many newlines before the end, or `PRELOAD_MAX_BYTES` back when the lines are long.
/// The second value says the offset is the start of a line; when it is not (and not 0) the caller skips the
/// cut line up to the next newline.
fn preload_offset(f: &mut std::fs::File, len: u64) -> (u64, bool) {
    let floor = len.saturating_sub(PRELOAD_MAX_BYTES);
    let mut end = len;
    let mut newlines = 0usize;
    let mut buf = vec![0u8; 64 * 1024];
    // A final newline ends the last line; it does not start another.
    let mut first = true;
    while end > floor {
        let start = end.saturating_sub(buf.len() as u64).max(floor);
        let n = (end - start) as usize;
        if f.seek(SeekFrom::Start(start)).is_err() || f.read_exact(&mut buf[..n]).is_err() {
            return (floor, floor == 0);
        }
        for i in (0..n).rev() {
            if buf[i] != b'\n' {
                continue;
            }
            if first && start + i as u64 + 1 == len {
                first = false;
                continue;
            }
            first = false;
            newlines += 1;
            if newlines == PRELOAD_LINES {
                // The line after this newline is the first of the last PRELOAD_LINES lines.
                return (start + i as u64 + 1, true);
            }
        }
        end = start;
    }
    (floor, floor == 0)
}

// ---- log rotation ---------------------------------------------------------------------------

/// Copy-truncate: `node.log` -> `node.log.1`, then empty `node.log` in place. The node keeps its
/// descriptor (opened `O_APPEND`) and carries on at the new end.
pub fn rotate_log(path: &Path, max_bytes: u64) -> std::io::Result<bool> {
    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(_) => return Ok(false),
    };
    if len < max_bytes {
        return Ok(false);
    }
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    std::fs::copy(path, path.with_file_name(name))?;
    std::fs::OpenOptions::new().write(true).open(path)?.set_len(0)?;
    Ok(true)
}

// ---- pid file -------------------------------------------------------------------------------

#[derive(Debug, Serialize, serde::Deserialize)]
struct PidRecord {
    pid: u32,
    network: String,
    bin: String,
    started: u64,
}

fn write_pid_file(root: &Path, pid: u32, network: &str, bin: &Path) {
    let rec = PidRecord {
        pid,
        network: network.to_string(),
        bin: bin.display().to_string(),
        started: crate::persist::unix_now(),
    };
    let path = pid_file(root);
    if let Ok(body) = serde_json::to_vec(&rec) {
        let tmp = path.with_extension("pid.tmp");
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn read_pid_file(root: &Path) -> Option<PidRecord> {
    serde_json::from_slice(&std::fs::read(pid_file(root)).ok()?).ok()
}

fn remove_pid_file(root: &Path) {
    let _ = std::fs::remove_file(pid_file(root));
}

/// Is `pid` the node binary we manage (and not a recycled pid)? The binary's file name must appear
/// in the process's command line or name.
fn is_our_binary(pid: u32, bin: &Path) -> bool {
    if pid == std::process::id() {
        return false;
    }
    let Some(name) = bin.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return false;
    };
    let in_argv = crate::procinfo::argv(pid).is_some_and(|a| a.iter().any(|x| x.contains(&name)));
    in_argv || crate::procinfo::comm(pid).is_some_and(|c| c.contains(&name))
}

/// Does `pid` run with `dir` as its working directory? (A recycled pid, or another console's node of the
/// same program, runs somewhere else.)
fn runs_in(pid: u32, dir: &Path) -> bool {
    let Some(cwd) = crate::procinfo::cwd(pid) else {
        return false;
    };
    match (cwd.canonicalize(), dir.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => cwd == dir,
    }
}

/// `pid` when it leads its own process group (the node does, we start it that way).
#[cfg(unix)]
fn pgid_if_leader(pid: u32) -> Option<u32> {
    (unsafe { libc::getpgid(pid as i32) } == pid as i32).then_some(pid)
}
#[cfg(not(unix))]
fn pgid_if_leader(_pid: u32) -> Option<u32> {
    None
}

// ---- process helpers ------------------------------------------------------------------------

#[cfg(unix)]
const SIGTERM: i32 = libc::SIGTERM;
#[cfg(not(unix))]
const SIGTERM: i32 = 15;

#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}
#[cfg(not(unix))]
pub fn pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) {
    unsafe {
        libc::kill(pid as i32, sig);
    }
}
#[cfg(not(unix))]
fn send_signal(_pid: u32, _sig: i32) {}

/// SIGKILL the node; when it leads its own process group (it does, we start it that way), take its
/// helpers (modules) down with it.
#[cfg(unix)]
fn kill_hard(pid: u32) {
    unsafe {
        if libc::getpgid(pid as i32) == pid as i32 {
            libc::killpg(pid as i32, libc::SIGKILL);
        } else {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }
}
#[cfg(not(unix))]
fn kill_hard(_pid: u32) {}

/// SIGKILL what is left of a node's process group after its leader died (modules it started).
#[cfg(unix)]
fn kill_group(pgid: u32) {
    if pgid > 1 {
        unsafe {
            libc::killpg(pgid as i32, libc::SIGKILL);
        }
    }
}
#[cfg(not(unix))]
fn kill_group(_pgid: u32) {}

// ---------------------------------------------------------------------------------------------

/// Test support: no node a test starts may outlive the test, whether it passes, fails or hangs.
///
/// * every node the agent spawns in a test is noted (pid, data folder);
/// * [`testkit::ReapNodes`] in a test's fixture kills the noted nodes under its folder, process group and
///   all, when the fixture is dropped (also while a failed assertion unwinds);
/// * an `atexit` hook kills whatever is left when the test program ends;
/// * the stand-in nodes are shell scripts that end by themselves when their parent (the test program) is gone,
///   which covers a test program that was killed (`SIGKILL`) and so ran neither of the above.
#[cfg(all(test, unix))]
pub(crate) mod testkit {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, Once};

    static SPAWNED: Mutex<Vec<(u32, PathBuf)>> = Mutex::new(Vec::new());
    static ATEXIT: Once = Once::new();

    extern "C" fn at_exit() {
        reap(None);
    }

    pub(crate) fn note_spawn(pid: u32, root: &Path) {
        ATEXIT.call_once(|| unsafe {
            libc::atexit(at_exit);
        });
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        SPAWNED.lock().unwrap_or_else(|e| e.into_inner()).push((pid, root));
    }

    /// Kill the noted nodes whose data folder is under `under` (all of them for `None`).
    pub(crate) fn reap(under: Option<&Path>) {
        let list: Vec<(u32, PathBuf)> = SPAWNED.lock().unwrap_or_else(|e| e.into_inner()).clone();
        for (pid, root) in list {
            if pid <= 1 || pid == std::process::id() || under.is_some_and(|u| !root.starts_with(u)) {
                continue;
            }
            if !super::pid_alive(pid) {
                continue;
            }
            // Still the same process? (A pid can be handed out again; a node runs inside its data folder.)
            if let Some(cwd) = crate::procinfo::cwd(pid) {
                let cwd = cwd.canonicalize().unwrap_or(cwd);
                if !cwd.starts_with(&root) {
                    continue;
                }
            }
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
                libc::kill(pid as i32, libc::SIGKILL);
            }
        }
    }

    /// `<dir>/stub-blvm.sh`: a program that is a node. It answers `version`, and otherwise becomes
    /// `agent::tests::stub_node_server`, a listener on `BLVM_TEST_STUB_ADDR` that answers `BLVM_TEST_STUB_STATUS`
    /// (see there). Its own name is on the command line as a second test filter that matches nothing, so that
    /// adoption finds the program by name.
    pub(crate) fn write_stub_node(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let exe = std::env::current_exe().unwrap();
        let script = dir.join("stub-blvm.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\nexec '{}' --ignored --exact agent::tests::stub_node_server stub-blvm.sh --nocapture\n",
                exe.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// A free loopback port, picked by the system (bind to port 0, read it back, let go). Every test that
    /// starts a node or binds a listener takes its ports from here: the networks' own ports may be held by a
    /// node of the person running the tests, and a test must not depend on them being free.
    pub(crate) fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// Two different free loopback ports (the RPC and the peer-to-peer port of one node).
    pub(crate) fn free_port_pair() -> (u16, u16) {
        // Hold both listeners until both are known, so the two cannot be the same port.
        let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        (a.local_addr().unwrap().port(), b.local_addr().unwrap().port())
    }

    /// Point a test console at free ports only: every network's RPC address is an override on a free loopback
    /// port (an override already there stays), and the node's peer-to-peer port is a free one. Without this a
    /// test would start a node on the network's own ports (regtest: 18443 and 18444).
    pub(crate) fn isolate(console: &mut crate::managed::Console) {
        for n in crate::networks::ALL {
            console
                .settings
                .rpc_overrides
                .entry(n.id.to_string())
                .or_insert_with(|| format!("127.0.0.1:{}", free_port()));
        }
        console.p2p_port = Some(free_port());
        console.reresolve();
    }

    /// Where a test console polls `net` (its override, which `isolate` put there).
    pub(crate) async fn rpc_of(state: &crate::state::Shared, net: &str) -> String {
        let g = state.read().await;
        g.console.rpc_for(crate::networks::by_id(net).unwrap())
    }

    /// Put one in the fixture of a test: kills the nodes started under `dir` when it is dropped.
    pub(crate) struct ReapNodes(PathBuf);

    impl ReapNodes {
        pub(crate) fn under(dir: &Path) -> Self {
            let _ = std::fs::create_dir_all(dir);
            ReapNodes(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()))
        }
    }

    impl Drop for ReapNodes {
        fn drop(&mut self) {
            reap(Some(&self.0));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

    const FAKE: &str = r#"#!/bin/sh
# Stand-in for `blvm`: behaviour chosen by FAKE_MODE. Signal handling is set up before the first
# output line, so a test that saw "fake node up" knows what SIGTERM will do.
if [ "$1" = "version" ]; then echo "blvm 9.9.9"; echo "Git: deadbeef"; exit 0; fi
case "$FAKE_MODE" in
  stubborn) trap '' TERM ;;
  crash|exit0) ;;
  *) trap 'echo "got TERM"; exit 0' TERM ;;
esac
echo "fake node up: $*"
echo "cwd=$(pwd) schnorr=$BLVM_SCHNORR_PER_SIG leak=[$BLVM_UI_TEST_LEAK] av=[$BLVM_ASSUME_VALID_HEIGHT]"
case "$FAKE_MODE" in
  crash) sleep 0.3; echo "about to crash"; exit 3 ;;
  exit0) sleep 0.3; exit 0 ;;
  chatty) while kill -0 $PPID 2>/dev/null; do echo "tick"; sleep 0.1; done ;;
  *) while kill -0 $PPID 2>/dev/null; do sleep 0.1; done ;;
esac
"#;

    struct Env {
        root: PathBuf,
        bin: PathBuf,
        _reap: super::testkit::ReapNodes,
    }

    fn setup(tag: &str) -> Env {
        let root = std::env::temp_dir().join(format!("blvm-agent-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let bin = root.join("fake-blvm.sh");
        std::fs::write(&bin, FAKE).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _reap = super::testkit::ReapNodes::under(&root);
        Env { root: root.join("data-root"), bin, _reap }
    }

    fn cfg(env: &Env, mode: &str) -> AgentConfig {
        let mut child_env = BTreeMap::new();
        child_env.insert("FAKE_MODE".to_string(), mode.to_string());
        AgentConfig {
            bin: Some(env.bin.clone()),
            root: env.root.clone(),
            network: networks::by_id("regtest"),
            assume_valid: None,
            prune_keep_blocks: None,
            child_env,
            rpc_auth: None,
            // Never the network's own ports: a regtest node of the person running the tests must not
            // make them fail, and a test must not take a port that somebody else may be about to use.
            rpc_addr: Some(free_addr()),
            p2p_port: Some(free_port()),
            p2p_listen_port: None,
        }
    }

    async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while Instant::now() < end {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    /// The fake node has installed its signal handling and said so.
    async fn wait_up(agent: &Agent) {
        wait_for("node output", || agent.logs(None, 200).iter().any(|l| l.text.starts_with("fake node up"))).await;
    }

    fn cleanup(env: &Env) {
        let _ = std::fs::remove_dir_all(env.bin.parent().unwrap());
    }

    #[tokio::test]
    async fn start_runs_in_the_network_dir_and_stop_is_graceful() {
        let env = setup("start-stop");
        std::env::set_var("BLVM_UI_TEST_LEAK", "secret");
        std::env::set_var("BLVM_ASSUME_VALID_HEIGHT", "123456");
        let agent = Agent::new(cfg(&env, "run"), fast());
        assert_eq!(agent.state(), ProcState::Stopped);
        agent.start().await.unwrap();
        assert_eq!(agent.state(), ProcState::Starting);
        let pid = agent.view().pid.expect("pid");
        assert!(pid_alive(pid));

        // `starting` until the RPC answers.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(agent.state(), ProcState::Starting);
        agent.rpc_flag().store(true, Ordering::Relaxed);
        wait_for("running", || agent.state() == ProcState::Running).await;
        assert!(env.root.join("control/node.pid").is_file(), "pid file written");

        // The node's own words reach the ring: args, cwd, and the default schnorr env.
        wait_for("log lines", || agent.logs(None, 100).iter().any(|l| l.text.contains("schnorr=1"))).await;
        let text: Vec<String> = agent.logs(None, 100).into_iter().map(|l| l.text).collect();
        let all = text.join("\n");
        let (rpc, p2p) = (agent.config().rpc_addr.unwrap(), agent.config().p2p_port.unwrap());
        assert!(
            all.contains(&format!("--network regtest --data-dir ./data --rpc-addr {rpc} --listen-addr 127.0.0.1:{p2p} --noassumevalid")),
            "{all}"
        );
        let net_dir = env.root.join("networks/regtest");
        let net_dir = net_dir.canonicalize().unwrap();
        assert!(all.contains(&format!("cwd={}", net_dir.display())), "{all}");
        assert!(all.contains("leak=[]"), "BLVM_UI_* variables must not reach the node: {all}");
        assert!(
            all.contains("av=[]"),
            "an inherited BLVM_ASSUME_VALID_HEIGHT beats the node's command line, so it must not reach the node: {all}"
        );
        assert!(net_dir.join("logs/node.log").is_file());

        // Graceful stop: SIGTERM, the script says so and exits 0.
        agent.request_stop();
        assert_eq!(agent.state(), ProcState::Stopping);
        agent.stop_and_wait().await;
        let v = agent.view();
        assert_eq!(v.state, ProcState::Stopped);
        assert_eq!(v.exit_code, Some(0));
        assert_eq!(v.pid, None);
        assert_eq!(v.last_error, None, "a clean stop is not an error");
        assert!(!pid_alive(pid));
        assert!(!env.root.join("control/node.pid").exists(), "pid file removed");
        assert!(agent.logs(None, 100).iter().any(|l| l.text.contains("got TERM")));

        // And it can start again.
        agent.start().await.unwrap();
        assert_eq!(agent.state(), ProcState::Starting);
        agent.stop_and_wait().await;
        assert_eq!(agent.state(), ProcState::Stopped);
        // Process-wide, and every other test in this binary starts nodes too (they must not see it either:
        // the agent removes it for them as well). Do not leave it behind.
        std::env::remove_var("BLVM_ASSUME_VALID_HEIGHT");
        cleanup(&env);
    }

    #[tokio::test]
    async fn crash_restarts_with_backoff_then_gives_up_with_the_log() {
        let env = setup("crash");
        let agent = Agent::new(cfg(&env, "crash"), fast());
        agent.start().await.unwrap();
        // The first crash is visible with its exit code and a restart pending.
        wait_for("first crash", || {
            let v = agent.view();
            v.state == ProcState::Crashed && v.exit_code == Some(3)
        })
        .await;
        let v = agent.view();
        assert!(v.last_error.as_deref().unwrap().contains("Exited with code 3"), "{:?}", v.last_error);
        assert!(v.last_error.as_deref().unwrap().contains("Restarting"), "{:?}", v.last_error);
        assert!(v.crash_log.iter().any(|l| l.contains("about to crash")), "{:?}", v.crash_log);

        // Three automatic restarts, then it stays crashed.
        wait_for("gave up", || {
            let v = agent.view();
            v.state == ProcState::Crashed && v.restarts == 3 && v.last_error.as_deref().is_some_and(|e| e.contains("stopped restarting"))
        })
        .await;
        let v = agent.view();
        assert_eq!(v.restarts, 3);
        assert_eq!(v.exit_code, Some(3));
        assert!(v.crash_log.len() <= CRASH_LOG_LINES);
        assert!(v.crash_log.iter().any(|l| l.contains("about to crash")));
        // Nothing else happens afterwards.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(agent.view().restarts, 3);
        assert_eq!(agent.state(), ProcState::Crashed);

        // "Try again" starts fresh and resets the counters.
        agent.start().await.unwrap();
        assert_eq!(agent.view().restarts, 0);
        assert_eq!(agent.state(), ProcState::Starting);
        agent.stop_and_wait().await;
        assert_eq!(agent.state(), ProcState::Stopped);
        cleanup(&env);
    }

    #[tokio::test]
    async fn killing_the_node_from_outside_is_a_crash_and_it_comes_back() {
        let env = setup("kill9");
        let agent = Agent::new(cfg(&env, "run"), fast());
        agent.start().await.unwrap();
        agent.rpc_flag().store(true, Ordering::Relaxed);
        wait_for("running", || agent.state() == ProcState::Running).await;
        let pid = agent.view().pid.unwrap();
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        wait_for("crashed", || agent.state() == ProcState::Crashed).await;
        let v = agent.view();
        assert_eq!(v.exit_code, None);
        assert_eq!(v.exit_signal, Some(9));
        assert!(v.last_error.as_deref().unwrap().contains("SIGKILL"), "{:?}", v.last_error);
        wait_for("restarted", || {
            let v = agent.view();
            v.restarts == 1 && v.pid.is_some_and(|p| p != pid)
        })
        .await;
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_node_that_ignores_sigterm_is_killed_after_the_timeout() {
        let env = setup("stubborn");
        let agent = Agent::new(cfg(&env, "stubborn"), fast());
        agent.start().await.unwrap();
        let pid = agent.view().pid.unwrap();
        wait_up(&agent).await;
        let t = Instant::now();
        agent.request_stop();
        // Still alive and "stopping" well after SIGTERM.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(agent.state(), ProcState::Stopping, "{:?}", agent.view());
        assert!(pid_alive(pid));
        agent.stop_and_wait().await;
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(1400), "SIGKILL only after stop_wait, took {waited:?}");
        assert!(waited < Duration::from_secs(6), "took {waited:?}");
        let v = agent.view();
        assert_eq!(v.state, ProcState::Stopped);
        assert_eq!(v.exit_signal, Some(9));
        assert!(v.last_error.as_deref().unwrap().contains("was killed"), "{:?}", v.last_error);
        assert!(!pid_alive(pid));
        cleanup(&env);
    }

    #[tokio::test]
    async fn restart_stops_then_starts_a_new_process() {
        let env = setup("restart");
        let agent = Agent::new(cfg(&env, "run"), fast());
        agent.start().await.unwrap();
        agent.rpc_flag().store(true, Ordering::Relaxed);
        wait_for("running", || agent.state() == ProcState::Running).await;
        wait_up(&agent).await;
        let pid = agent.view().pid.unwrap();
        agent.restart().await.unwrap();
        assert_eq!(agent.state(), ProcState::Stopping);
        wait_for("new process", || {
            let v = agent.view();
            v.pid.is_some_and(|p| p != pid) && v.state.is_active() && v.state != ProcState::Stopping
        })
        .await;
        assert_eq!(agent.view().restarts, 0, "a user restart is not a crash restart");
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_node_that_never_answers_is_stopped_and_reported() {
        let env = setup("noready");
        let mut t = fast();
        t.start_timeout = Duration::from_millis(500);
        let agent = Agent::new(cfg(&env, "run"), t);
        agent.start().await.unwrap();
        wait_for("start failure", || agent.state() == ProcState::Crashed).await;
        let v = agent.view();
        assert!(v.last_error.as_deref().unwrap().contains("did not answer"), "{:?}", v.last_error);
        assert_eq!(v.pid, None);
        // No restart loop for this kind of failure.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(agent.state(), ProcState::Crashed);
        assert_eq!(agent.view().restarts, 0);
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_clean_exit_on_its_own_is_stopped_not_crashed() {
        let env = setup("exit0");
        let agent = Agent::new(cfg(&env, "exit0"), fast());
        agent.start().await.unwrap();
        wait_for("stopped", || agent.state() == ProcState::Stopped).await;
        assert_eq!(agent.view().exit_code, Some(0));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(agent.state(), ProcState::Stopped, "no restart after a clean exit");
        cleanup(&env);
    }

    #[tokio::test]
    async fn start_needs_a_network_and_a_binary() {
        let env = setup("preflight");
        let mut c = cfg(&env, "run");
        c.network = None;
        let agent = Agent::new(c.clone(), fast());
        let e = agent.start().await.unwrap_err();
        assert!(e.contains("Choose a network"), "{e}");
        assert_eq!(agent.state(), ProcState::Crashed);
        assert!(!env.root.join("networks").exists(), "nothing is created without a network");

        let mut c2 = cfg(&env, "run");
        c2.bin = Some(env.root.join("missing-blvm"));
        let agent = Agent::new(c2, fast());
        let e = agent.start().await.unwrap_err();
        assert!(e.contains("not found"), "{e}");
        let mut c3 = cfg(&env, "run");
        c3.bin = None;
        let agent = Agent::new(c3, fast());
        assert!(agent.start().await.unwrap_err().contains("No node binary"));
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_new_console_adopts_the_node_the_old_one_left_running() {
        let env = setup("adopt");
        let first = Agent::new(cfg(&env, "run"), fast());
        first.start().await.unwrap();
        let pid = first.view().pid.unwrap();
        wait_up(&first).await;
        // The old console "dies": supervision ends, the process keeps going.
        first.detach();
        assert!(pid_alive(pid));

        let second = Agent::new(cfg(&env, "run"), fast());
        second.rpc_flag().store(true, Ordering::Relaxed);
        assert!(second.adopt());
        let v = second.view();
        assert_eq!(v.pid, Some(pid));
        assert!(v.adopted);
        wait_for("adopted node running", || second.state() == ProcState::Running).await;
        // History is still there.
        wait_for("old log lines", || second.logs(None, 50).iter().any(|l| l.text.contains("fake node up"))).await;

        // And it can be stopped like any other.
        second.stop_and_wait().await;
        assert_eq!(second.state(), ProcState::Stopped);
        assert!(!pid_alive(pid));
        assert!(!env.root.join("control/node.pid").exists());
        cleanup(&env);
    }

    #[tokio::test]
    async fn an_adopted_node_that_dies_is_a_crash() {
        let env = setup("adopt-die");
        let first = Agent::new(cfg(&env, "run"), fast());
        first.start().await.unwrap();
        let pid = first.view().pid.unwrap();
        first.detach();
        let second = Agent::new(cfg(&env, "run"), fast());
        assert!(second.adopt());
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
        wait_for("crashed", || second.state() == ProcState::Crashed).await;
        assert_eq!(second.view().exit_code, None, "exit status of an adopted node is unknown");
        second.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn stale_or_foreign_pid_files_are_not_adopted() {
        let env = setup("adopt-no");
        std::fs::create_dir_all(env.root.join("control")).unwrap();
        let agent = Agent::new(cfg(&env, "run"), fast());

        // No file.
        assert!(!agent.adopt());

        // Dead pid.
        write_pid_file(&env.root, 999_999, "regtest", &env.bin);
        assert!(!agent.adopt());
        assert!(!env.root.join("control/node.pid").exists(), "stale file removed");

        // A live process that is not our binary.
        let mut other = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        write_pid_file(&env.root, other.id(), "regtest", &env.bin);
        assert!(!agent.adopt());
        assert!(pid_alive(other.id()), "an unrelated process is left alone");
        assert!(!env.root.join("control/node.pid").exists());

        // Our binary on a different network than the configured one.
        let live = Agent::new(cfg(&env, "run"), fast());
        live.start().await.unwrap();
        let pid = live.view().pid.unwrap();
        live.detach();
        write_pid_file(&env.root, pid, "signet", &env.bin);
        assert!(!agent.adopt());
        assert!(pid_alive(pid));
        kill_hard(pid);
        let _ = other.kill();
        let _ = other.wait();
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_closed_agent_starts_nothing_new() {
        let env = setup("closed");
        let agent = Agent::new(cfg(&env, "run"), fast());
        agent.start().await.unwrap();
        wait_up(&agent).await;
        agent.shutdown(true).await;
        assert_eq!(agent.state(), ProcState::Stopped);
        let e = agent.start().await.unwrap_err();
        assert!(e.contains("shutting down"), "{e}");
        assert_eq!(agent.state(), ProcState::Stopped);
        assert!(agent.restart().await.is_err());
        cleanup(&env);
    }

    #[tokio::test]
    async fn shutdown_stops_or_leaves_the_node() {
        let env = setup("shutdown");
        let agent = Agent::new(cfg(&env, "run"), fast());
        agent.start().await.unwrap();
        let pid = agent.view().pid.unwrap();
        wait_up(&agent).await;
        agent.shutdown(true).await;
        assert!(!pid_alive(pid), "stop_node_on_exit stops the node");

        let agent = Agent::new(cfg(&env, "run"), fast());
        agent.start().await.unwrap();
        let pid = agent.view().pid.unwrap();
        agent.shutdown(false).await;
        assert!(pid_alive(pid), "keep running leaves it");
        assert!(env.root.join("control/node.pid").is_file(), "so the next console can adopt it");
        kill_hard(pid);
        cleanup(&env);
    }

    #[tokio::test]
    async fn version_is_read_from_the_binary_and_reread_when_it_changes() {
        let env = setup("version");
        let agent = Agent::new(cfg(&env, "run"), fast());
        assert!(agent.installed_is_stale(), "never read");
        agent.refresh_installed(false).await;
        let i = agent.installed().unwrap();
        assert_eq!(i.version.as_deref(), Some("9.9.9"));
        assert_eq!(i.self_report.as_deref(), Some("blvm 9.9.9"));
        assert!(!agent.installed_is_stale());

        // Replace the binary: new mtime, new answer.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let body = FAKE.replace("blvm 9.9.9", "blvm 1.2.3");
        std::fs::write(&env.bin, body).unwrap();
        assert!(agent.installed_is_stale());
        agent.refresh_installed(false).await;
        assert_eq!(agent.installed().unwrap().version.as_deref(), Some("1.2.3"));
        cleanup(&env);
    }

    #[test]
    fn version_output_parsing_ignores_the_git_line_and_noise() {
        let out = "<jemalloc>: option background_thread currently supports pthread only\nblvm 0.1.0\nRepository: https://github.com/BTCDecoded/blvm\nGit: abc123\n";
        assert_eq!(parse_version_output(out).as_deref(), Some("0.1.0"));
        assert_eq!(parse_version_output("blvm v0.1.60\n").as_deref(), Some("0.1.60"));
        assert_eq!(parse_version_output("Git: blvm 9.9.9\n"), None, "must be the name/version line");
        assert_eq!(parse_version_output("blvm\n"), None);
        assert_eq!(parse_version_output("blvm weird\n"), None);
        assert_eq!(parse_version_output(""), None);
    }

    #[test]
    fn args_and_env_follow_the_spec() {
        let regtest = networks::by_id("regtest").unwrap();
        let mainnet = networks::by_id("mainnet").unwrap();
        let mut c = AgentConfig {
            bin: None,
            root: PathBuf::from("/r"),
            network: Some(regtest),
            rpc_addr: None,
            assume_valid: None,
            prune_keep_blocks: None,
            child_env: BTreeMap::new(),
            rpc_auth: None,
            p2p_port: None,
            p2p_listen_port: None,
        };
        assert_eq!(
            build_args(&c, regtest),
            [
                "--network", "regtest", "--data-dir", "./data", "--rpc-addr", "127.0.0.1:18443", "--listen-addr", "0.0.0.0:18444",
                "--noassumevalid", "--config", "./blvm.toml"
            ]
        );
        // Mainnet keeps the node's own assume-valid default; the P2P bind is always stated (B3).
        assert_eq!(
            build_args(&c, mainnet),
            ["--network", "mainnet", "--data-dir", "./data", "--rpc-addr", "127.0.0.1:8332", "--listen-addr", "0.0.0.0:8333", "--config", "./blvm.toml"]
        );
        // Overrides.
        c.assume_valid = Some("off".into());
        assert!(build_args(&c, mainnet).contains(&"--noassumevalid".to_string()));
        c.assume_valid = Some("700000".into());
        let a = build_args(&c, regtest);
        assert!(a.windows(2).any(|w| w[0] == "--assumevalid" && w[1] == "700000"));
        assert!(!a.contains(&"--noassumevalid".to_string()));
        c.assume_valid = Some("bogus".into());
        assert_eq!(build_args(&c, mainnet).len(), 10, "an invalid setting falls back to the default (eight options and --config <file>)");
        c.rpc_addr = Some("127.0.0.1:19999".into());
        assert!(build_args(&c, mainnet).contains(&"127.0.0.1:19999".to_string()));

        assert!(valid_assume_valid("0") && valid_assume_valid("off") && valid_assume_valid("default"));
        assert!(valid_assume_valid(&"ab".repeat(32)));
        assert!(!valid_assume_valid("12x") && !valid_assume_valid("") && !valid_assume_valid("1234567890"));

        // "default" per network: full validation is stated on Testnet4 and regtest, the node's own default
        // (no flag) on the others; an explicit height or "off" wins on every network.
        for net in networks::ALL.iter() {
            let want: Vec<String> = if net.full_validation { vec!["--noassumevalid".into()] } else { Vec::new() };
            assert_eq!(assume_valid_args(Some("default"), net), want, "{}", net.id);
            assert_eq!(assume_valid_args(None, net), want, "{}", net.id);
            assert_eq!(assume_valid_args(Some("off"), net), ["--noassumevalid"], "{}", net.id);
            assert_eq!(assume_valid_args(Some("800000"), net), ["--assumevalid", "800000"], "{}", net.id);
        }
        assert!(["mainnet", "testnet", "signet"].iter().all(|id| !networks::by_id(id).unwrap().full_validation));
        assert!(["testnet4", "regtest"].iter().all(|id| networks::by_id(id).unwrap().full_validation));

        // Child env: schnorr per-sig on by default, overridable, empty value unsets.
        assert_eq!(child_env(&c), vec![("BLVM_SCHNORR_PER_SIG".to_string(), Some("1".to_string()))]);
        c.child_env.insert("BLVM_SCHNORR_PER_SIG".into(), "".into());
        c.child_env.insert("RUST_LOG".into(), "info".into());
        let e = child_env(&c);
        assert!(e.contains(&("BLVM_SCHNORR_PER_SIG".to_string(), None)));
        assert!(e.contains(&("RUST_LOG".to_string(), Some("info".to_string()))));
    }

    #[test]
    fn log_rotation_is_copy_truncate() {
        let dir = std::env::temp_dir().join(format!("blvm-agent-rot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("node.log");
        std::fs::write(&log, "small\n").unwrap();
        assert!(!rotate_log(&log, 100).unwrap(), "under the limit");
        assert!(!dir.join("node.log.1").exists());
        // A writer that keeps its descriptor open in append mode, like the node.
        let mut w = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        w.write_all(&vec![b'x'; 200]).unwrap();
        assert!(rotate_log(&log, 100).unwrap());
        assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
        assert_eq!(std::fs::metadata(dir.join("node.log.1")).unwrap().len(), 206);
        w.write_all(b"after\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "after\n", "the writer carries on at the new end");
        assert!(!rotate_log(&dir.join("absent.log"), 1).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_ring_follows_the_file_across_truncation_and_serves_after() {
        let env = setup("ring");
        let agent = Agent::new(cfg(&env, "run"), fast());
        let log = networks::log_file(&env.root, networks::by_id("regtest").unwrap());
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "old 1\nold 2\n").unwrap();
        agent.inner.ensure_tailer();
        wait_for("preload", || agent.logs(None, 10).len() == 2).await;
        let first = agent.logs(None, 10);
        assert_eq!((first[0].seq, first[0].text.as_str()), (1, "old 1"));
        assert_eq!(first[1].seq, 2);

        // New lines get higher sequence numbers; `after` returns only those.
        let mut w = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        w.write_all(b"new 3\nnew 4\npartial").unwrap();
        wait_for("new lines", || agent.logs(Some(2), 10).len() == 2).await;
        let after = agent.logs(Some(2), 10);
        assert_eq!(after.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["new 3", "new 4"]);
        assert!(agent.logs(Some(4), 10).is_empty(), "the unterminated line is not a line yet");
        w.write_all(b" line\n").unwrap();
        wait_for("completed line", || agent.logs(Some(4), 10).len() == 1).await;
        assert_eq!(agent.logs(Some(4), 10)[0].text, "partial line");

        // limit applies from the front for `after` (so a poller sees every line), tail for none.
        assert_eq!(agent.logs(Some(0), 2).iter().map(|l| l.seq).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(agent.logs(None, 2).iter().map(|l| l.seq).collect::<Vec<_>>(), [4, 5]);
        // A cursor from a previous console run (beyond the newest) gets the tail.
        assert_eq!(agent.logs(Some(9_999), 2).len(), 2);

        // Copy-truncate: the file shrinks, the ring keeps its lines and picks up the new ones.
        rotate_log(&log, 1).unwrap();
        w.write_all(b"post rotate\n").unwrap();
        wait_for("post rotate line", || agent.logs(Some(5), 10).iter().any(|l| l.text == "post rotate")).await;
        cleanup(&env);
    }

    // ---- review (U0/U1) tests: each asserts the behaviour the design asks for ----

    const ONCE_CRASH: &str = r#"#!/bin/sh
if [ "$1" = "version" ]; then echo "blvm 9.9.9"; exit 0; fi
trap 'exit 0' TERM
if [ ! -f ./crashed-once ]; then touch ./crashed-once; echo "first run crashes"; exit 3; fi
echo "fake node up: $*"
while kill -0 $PPID 2>/dev/null; do sleep 0.1; done
"#;

    // ---- the node's RPC login (review B1) ----

    fn login() -> RpcAuth {
        RpcAuth { user: "commons".into(), pass: "p4ss\"w\\rd".into() }
    }

    #[test]
    fn the_node_config_has_the_login_and_the_one_key_the_node_insists_on() {
        let t = node_config_toml(Some(&login()), None);
        assert!(t.contains("transport_preference = \"tcponly\"\n"), "{t}");
        assert!(t.contains("[rpc_auth]\nrequired = true\nusername = \"commons\"\npassword = \"p4ss\\\"w\\\\rd\"\n"), "{t}");
        // Top-level keys come before the first table, or they would belong to it.
        assert!(t.find("transport_preference").unwrap() < t.find("[rpc_auth]").unwrap());
        // Control characters cannot break out of the string.
        let nasty = node_config_toml(Some(&RpcAuth { user: "a\nb".into(), pass: "x\u{1}\ty".into() }), None);
        assert!(nasty.contains("username = \"a\\nb\"") && nasty.contains("password = \"x\\u0001\\ty\""), "{nasty}");
        assert_eq!(nasty.lines().filter(|l| l.starts_with("username")).count(), 1);
        // The password never shows in a Debug print of the config.
        let c = AgentConfig { rpc_auth: Some(login()), ..cfg(&setup("dbg"), "run") };
        assert!(!format!("{c:?}").contains("p4ss"));
    }

    #[test]
    fn the_config_option_is_always_passed() {
        // Without it the node may pick up ~/.config/blvm/blvm.toml or /etc/blvm/blvm.toml instead of the file
        // the console wrote, with or without a login and with or without pruning.
        let regtest = networks::by_id("regtest").unwrap();
        let mut c = cfg(&setup("cfgarg"), "run");
        let has_config = |c: &AgentConfig| {
            let a = build_args(c, regtest);
            a.windows(2).any(|w| w[0] == "--config" && w[1] == "./blvm.toml") && a.iter().filter(|x| *x == "--config").count() == 1
        };
        assert!(has_config(&c), "no login, no pruning");
        c.rpc_auth = Some(login());
        assert!(has_config(&c), "login");
        c.prune_keep_blocks = Some(5000);
        assert!(has_config(&c), "login and pruning");
        c.rpc_auth = None;
        assert!(has_config(&c), "pruning only");
    }

    /// The `[storage.pruning]` block the node must be given for "keep the newest N blocks": every key explicit
    /// (the node's parsed defaults are unsafe), `mode` as a sub-table of `type = "normal"`.
    fn pruning_block(n: u64) -> String {
        format!(
            "\n[storage.pruning]\nauto_prune = true\nauto_prune_interval = 144\nmin_blocks_to_keep = {n}\n\
             incremental_prune_during_ibd = true\nprune_window_size = {n}\nmin_blocks_for_incremental_prune = 288\n\
             prune_on_startup = false\n\n[storage.pruning.mode]\ntype = \"normal\"\nkeep_from_height = 0\n\
             min_recent_blocks = {n}\n"
        )
    }

    #[test]
    fn the_node_config_has_pruning_only_when_asked_and_a_login_only_when_there_is_one() {
        let header = "# Written by Commons Node View before every start of the node. Changes here are overwritten.\n\
                      transport_preference = \"tcponly\"\n";
        let rpc = "\n[rpc_auth]\nrequired = true\nusername = \"commons\"\npassword = \"p4ss\\\"w\\\\rd\"\n";

        // Nothing configured: the header and the one key the node insists on.
        assert_eq!(node_config_toml(None, None), header);
        // A login only.
        assert_eq!(node_config_toml(Some(&login()), None), format!("{header}{rpc}"));
        // Pruning only: every key is there and N is substituted in all three places.
        let p = node_config_toml(None, Some(5000));
        assert_eq!(p, format!("{header}{}", pruning_block(5000)));
        assert!(!p.contains("rpc_auth"));
        for key in [
            "auto_prune = true",
            "auto_prune_interval = 144",
            "min_blocks_to_keep = 5000",
            "incremental_prune_during_ibd = true",
            "prune_window_size = 5000",
            "min_blocks_for_incremental_prune = 288",
            "prune_on_startup = false",
            "[storage.pruning.mode]",
            "type = \"normal\"",
            "keep_from_height = 0",
            "min_recent_blocks = 5000",
        ] {
            assert!(p.lines().any(|l| l == key), "missing `{key}` in\n{p}");
        }
        assert_eq!(p.matches("5000").count(), 3, "N appears in min_blocks_to_keep, prune_window_size and min_recent_blocks");
        // Both: login first, then pruning; top-level keys stay above every table.
        let both = node_config_toml(Some(&login()), Some(288));
        assert_eq!(both, format!("{header}{rpc}{}", pruning_block(288)));
        assert!(both.find("transport_preference").unwrap() < both.find("[rpc_auth]").unwrap());
        assert!(both.find("[rpc_auth]").unwrap() < both.find("[storage.pruning]").unwrap());
        assert!(both.find("[storage.pruning]\n").unwrap() < both.find("[storage.pruning.mode]").unwrap());
        // `mode` is its own sub-table, with the `type` tag the node's internally tagged enum reads (parsed with a
        // real TOML parser in `the_generated_config_parses_to_the_tables_the_node_reads`).
        assert!(both.contains("\n[storage.pruning.mode]\ntype = \"normal\"\n"), "{both}");
        // The mode table is the last table, so no pruning key can fall under it by accident.
        assert!(both.rfind("[storage.pruning]").unwrap() < both.rfind("[storage.pruning.mode]").unwrap());
        assert!(!both["[storage.pruning.mode]".len() + both.rfind("[storage.pruning.mode]").unwrap()..].contains("auto_prune"));
    }

    /// The text is parsed with a real TOML parser, so a typo that the exact-text test above would repeat (a key
    /// under the wrong table, a bad escape) shows up as a different structure.
    #[test]
    fn the_generated_config_parses_to_the_tables_the_node_reads() {
        let parse = |t: &str| t.parse::<toml::Table>().unwrap_or_else(|e| panic!("not TOML: {e}\n{t}"));

        // Nothing configured: one top-level key and no tables.
        let t = parse(&node_config_toml(None, None));
        assert_eq!(t.get("transport_preference").and_then(|v| v.as_str()), Some("tcponly"));
        assert!(t.get("rpc_auth").is_none() && t.get("storage").is_none());
        assert_eq!(t.len(), 1);

        // A login only: the password survives its escapes.
        let t = parse(&node_config_toml(Some(&login()), None));
        let auth = t["rpc_auth"].as_table().unwrap();
        assert_eq!(auth["required"].as_bool(), Some(true));
        assert_eq!(auth["username"].as_str(), Some("commons"));
        assert_eq!(auth["password"].as_str(), Some("p4ss\"w\\rd"));
        assert!(t.get("storage").is_none());

        // Pruning only: `storage.pruning` with every key at its type, `mode` a table tagged `normal`.
        for n in [288u64, 5000, 100_000_000] {
            let t = parse(&node_config_toml(None, Some(n)));
            assert!(t.get("rpc_auth").is_none(), "no login, no [rpc_auth]");
            let p = t["storage"]["pruning"].as_table().unwrap();
            assert_eq!(p["auto_prune"].as_bool(), Some(true));
            assert_eq!(p["auto_prune_interval"].as_integer(), Some(144));
            assert_eq!(p["min_blocks_to_keep"].as_integer(), Some(n as i64));
            assert_eq!(p["incremental_prune_during_ibd"].as_bool(), Some(true));
            assert_eq!(p["prune_window_size"].as_integer(), Some(n as i64));
            assert_eq!(p["min_blocks_for_incremental_prune"].as_integer(), Some(288));
            assert_eq!(p["prune_on_startup"].as_bool(), Some(false));
            let mode = p["mode"].as_table().expect("mode is a table");
            assert_eq!(mode["type"].as_str(), Some("normal"));
            assert_eq!(mode["keep_from_height"].as_integer(), Some(0));
            assert_eq!(mode["min_recent_blocks"].as_integer(), Some(n as i64));
            assert_eq!(mode.len(), 3, "nothing else under mode: {mode:?}");
            // Nothing under `storage` but pruning; no key of the block fell out of it.
            assert_eq!(t["storage"].as_table().unwrap().len(), 1);
            assert_eq!(p.len(), 8, "seven keys and the mode table: {p:?}");
        }

        // Both: the login and the same pruning structure, and the top-level key is still top level.
        let t = parse(&node_config_toml(Some(&login()), Some(288)));
        assert_eq!(t["transport_preference"].as_str(), Some("tcponly"));
        assert_eq!(t["rpc_auth"]["username"].as_str(), Some("commons"));
        assert_eq!(t["storage"]["pruning"]["mode"]["type"].as_str(), Some("normal"));
        assert_eq!(t["storage"]["pruning"]["min_blocks_to_keep"].as_integer(), Some(288));
    }

    #[test]
    fn exposed_addresses_are_everything_but_loopback() {
        for open in ["0.0.0.0:4242", "10.0.0.7:4242", "node.internal:4242", "[::]:4242", "btc-commons-node-view_agent_1:4243"] {
            assert!(rpc_exposed(open), "{open}");
        }
        for local in ["127.0.0.1:4242", "127.5.5.5:1", "localhost:4242", "[::1]:4242"] {
            assert!(!rpc_exposed(local), "{local}");
        }
        assert_eq!(probe_addr("0.0.0.0:4243"), "127.0.0.1:4243");
        assert_eq!(probe_addr("[::]:4243"), "127.0.0.1:4243");
        assert_eq!(probe_addr("10.0.0.7:4243"), "10.0.0.7:4243", "a specific bind address is dialled as it is");
        assert_eq!(probe_addr("127.0.0.1:4244"), "127.0.0.1:4244");
    }

    #[tokio::test]
    async fn a_start_with_a_login_writes_a_private_config_and_passes_it_on() {
        use std::os::unix::fs::PermissionsExt;
        let env = setup("login-start");
        let mut c = cfg(&env, "run");
        c.rpc_auth = Some(login());
        let agent = Agent::new(c, fast());
        agent.start().await.unwrap();
        wait_up(&agent).await;
        let file = env.root.join("networks/regtest/blvm.toml");
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, node_config_toml(Some(&login()), None));
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        let log: String = agent.logs(None, 200).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n");
        assert!(log.contains("--config ./blvm.toml"), "{log}");
        assert!(!log.contains("p4ss"), "the password is not logged: {log}");
        agent.stop_and_wait().await;
        // The next start writes the file again, with the login of the moment.
        let mut c2 = agent.config();
        c2.rpc_auth = Some(RpcAuth { user: "commons".into(), pass: "second".into() });
        agent.set_config(c2);
        agent.start().await.unwrap();
        wait_for("second start", || agent.logs(None, 400).iter().filter(|l| l.text.starts_with("fake node up")).count() >= 2).await;
        assert!(std::fs::read_to_string(&file).unwrap().contains("password = \"second\""));
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_node_whose_rpc_would_be_open_to_other_containers_is_not_started() {
        let env = setup("login-exposed");
        let mut c = cfg(&env, "run");
        c.rpc_addr = Some(format!("0.0.0.0:{}", free_port()));
        let agent = Agent::new(c, fast());
        let e = agent.start().await.unwrap_err();
        assert!(e.contains("without a login") && e.contains("BLVM_RPC_PASS"), "{e}");
        assert_eq!(agent.state(), ProcState::Crashed);
        assert_eq!(agent.view().pid, None);
        assert!(!env.root.join("networks/regtest/blvm.toml").exists());
        // With a login the same address is fine.
        let mut c2 = agent.config();
        c2.rpc_auth = Some(login());
        agent.set_config(c2);
        agent.start().await.unwrap();
        wait_up(&agent).await;
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    /// Not a test: the body of the stand-in node of the tests below. A test spawns this very test program with
    /// `--ignored --exact agent::tests::stub_node_server`; it then plays a node whose RPC answers every request
    /// with `BLVM_TEST_STUB_STATUS`, until it is signalled.
    #[test]
    #[ignore = "run by the tests that need a node with a real listener"]
    fn stub_node_server() {
        use std::io::{Read, Write};
        let (Ok(addr), Ok(status)) = (std::env::var("BLVM_TEST_STUB_ADDR"), std::env::var("BLVM_TEST_STUB_STATUS")) else {
            return;
        };
        let l = std::net::TcpListener::bind(&addr).unwrap();
        // Gone with the test program that started it, however that ended.
        let parent = unsafe { libc::getppid() };
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(200));
            if unsafe { libc::getppid() } != parent {
                std::process::exit(0);
            }
        });
        println!("fake node up: stub on {addr}");
        let mut seen = 0u32;
        for conn in l.incoming().flatten() {
            let mut conn = conn;
            seen += 1;
            let mut b = [0u8; 1024];
            let _ = conn.read(&mut b);
            // `drop`: hang up on everybody (the node's per-address limit). `drop2`: on the first two callers only.
            // `by-config`: open without a login in `./blvm.toml` in the working folder, asking for one with it (what
            // the real node does with the config file this console writes: the file is always there now, and only
            // its `[rpc_auth]` table turns the login on).
            let status = match status.as_str() {
                "drop" => continue,
                "drop2" if seen <= 2 => continue,
                "drop2" => "401 Unauthorized",
                "by-config" if std::fs::read_to_string("./blvm.toml").is_ok_and(|t| t.contains("[rpc_auth]")) => "401 Unauthorized",
                "by-config" => "200 OK",
                other => other,
            };
            let body = "{\"jsonrpc\":\"2.0\",\"result\":0,\"id\":0}";
            let _ = conn.write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes(),
            );
        }
    }

    /// A script that is a node: it answers `version`, and otherwise becomes the stub server above.
    fn stub_node(env: &Env, status: &str, addr: &str) -> AgentConfig {
        let script = testkit::write_stub_node(env.bin.parent().unwrap());
        let mut c = cfg(env, "run");
        c.bin = Some(script);
        c.child_env.insert("BLVM_TEST_STUB_ADDR".into(), addr.into());
        c.child_env.insert("BLVM_TEST_STUB_STATUS".into(), status.into());
        c.rpc_addr = Some(addr.into());
        c.rpc_auth = Some(login());
        c
    }

    use testkit::free_port;

    fn free_addr() -> String {
        format!("127.0.0.1:{}", free_port())
    }

    #[tokio::test]
    async fn a_node_that_answers_without_the_password_is_stopped_and_one_that_asks_is_kept() {
        // Open: the config file was not applied.
        let env = setup("login-open");
        let mut t = fast();
        t.start_timeout = Duration::from_secs(20);
        let addr = free_addr();
        let a2 = addr.clone();
        // "Ready" as the poller would decide it: once something answers on the node's RPC port.
        let agent = Agent::with_ready(stub_node(&env, "200 OK", &addr), t.clone(), Arc::new(move || std::net::TcpStream::connect(&a2).is_ok()));
        agent.start().await.unwrap();
        wait_for("stopped for safety", || matches!(agent.state(), ProcState::Stopped | ProcState::Crashed)).await;
        let why = agent.view().last_error.unwrap_or_default();
        assert!(why.contains("without asking for the RPC password"), "{why}");
        assert_ne!(agent.state(), ProcState::Running);
        cleanup(&env);

        // Closed: a 401 to a caller with no login is what a locked node says.
        let env = setup("login-closed");
        let addr = free_addr();
        let a2 = addr.clone();
        let agent = Agent::with_ready(stub_node(&env, "401 Unauthorized", &addr), t, Arc::new(move || std::net::TcpStream::connect(&a2).is_ok()));
        agent.start().await.unwrap();
        wait_for("running", || agent.state() == ProcState::Running).await;
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    /// Mark the stand-in node as up, the way the poller would once the RPC answered.
    fn ready_flag() -> (Arc<AtomicBool>, Arc<dyn Fn() -> bool + Send + Sync>) {
        let f = Arc::new(AtomicBool::new(false));
        let f2 = Arc::clone(&f);
        (f, Arc::new(move || f2.load(Ordering::Relaxed)))
    }

    #[tokio::test]
    async fn a_node_a_test_leaves_running_is_killed_when_its_fixture_goes() {
        // R3: a failing assertion unwinds past the `stop` at the end of a test; the fixture's guard must still
        // take the node (and its group) down.
        let pid;
        {
            let env = setup("reap-guard");
            let agent = Agent::new(cfg(&env, "run"), fast());
            agent.start().await.unwrap();
            wait_up(&agent).await;
            pid = agent.view().pid.unwrap();
            assert!(pid_alive(pid));
            agent.detach();
            // `env` goes out of scope with the node still running.
        }
        wait_for("the node to be gone", || !pid_alive(pid)).await;
    }

    #[tokio::test]
    async fn a_node_that_hangs_up_on_the_check_is_not_taken_for_a_locked_one() {
        // N-a: a dropped connection (the node's limit of connections per address) says nothing about the login.
        let env = setup("login-drop");
        let mut t = fast();
        t.start_timeout = Duration::from_millis(1500);
        t.start_max = Duration::from_millis(1500);
        let (up, ready) = ready_flag();
        let addr = free_addr();
        let agent = Agent::with_ready(stub_node(&env, "drop", &addr), t, ready);
        agent.start().await.unwrap();
        wait_up(&agent).await;
        up.store(true, Ordering::Relaxed);
        let end = Instant::now() + Duration::from_millis(1200);
        while Instant::now() < end {
            assert_ne!(agent.state(), ProcState::Running, "an unanswered check must not make the node `running`");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        wait_for("given up", || agent.state() == ProcState::Crashed).await;
        assert!(agent.view().last_error.unwrap_or_default().contains("did not answer"), "not the 'no password' text");
        cleanup(&env);

        // A node that only hangs up on the first callers is asked again, and is `running` once it says 401.
        let env = setup("login-drop2");
        let (up, ready) = ready_flag();
        let addr = free_addr();
        let agent = Agent::with_ready(stub_node(&env, "drop2", &addr), fast(), ready);
        agent.start().await.unwrap();
        wait_up(&agent).await;
        up.store(true, Ordering::Relaxed);
        wait_for("running after the retries", || agent.state() == ProcState::Running).await;
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_start_without_a_login_rewrites_the_config_file_without_rpc_auth() {
        // The node reads ./blvm.toml from its working folder even without --config, so the console always owns
        // the file: a login from an earlier start must not survive into a start without one, and the file the
        // console passes with --config must exist.
        use std::os::unix::fs::PermissionsExt;
        let env = setup("login-stale");
        let dir = env.root.join("networks/regtest");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("blvm.toml"), node_config_toml(Some(&login()), Some(1000))).unwrap();
        let agent = Agent::new(cfg(&env, "run"), fast()); // no login: loopback only
        agent.start().await.unwrap();
        wait_up(&agent).await;
        let file = dir.join("blvm.toml");
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, node_config_toml(None, None), "the old password and the old pruning block are gone");
        assert!(!text.contains("rpc_auth") && !text.contains("storage.pruning"), "{text}");
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        let log: String = agent.logs(None, 50).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n");
        assert!(log.contains("--config ./blvm.toml"), "{log}");
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn a_start_with_pruning_writes_the_pruning_block_and_the_next_start_follows_the_setting() {
        let env = setup("prune-start");
        let mut c = cfg(&env, "run");
        c.prune_keep_blocks = Some(300);
        let agent = Agent::new(c, fast());
        agent.start().await.unwrap();
        wait_up(&agent).await;
        let file = env.root.join("networks/regtest/blvm.toml");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), node_config_toml(None, Some(300)));
        agent.stop_and_wait().await;
        // Pruning switched off: the next start writes the file without the block.
        let mut c2 = agent.config();
        c2.prune_keep_blocks = None;
        agent.set_config(c2);
        agent.start().await.unwrap();
        wait_up(&agent).await;
        assert_eq!(std::fs::read_to_string(&file).unwrap(), node_config_toml(None, None));
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    /// An install from before nodes had a login: a node is running without one (so it answers any caller), the
    /// console has just created a password for it and adopts the node.
    async fn open_node_adopted_with_a_new_login(env: &Env, flag: bool) -> (Agent, u32, Arc<AtomicBool>) {
        let addr = free_addr();
        let mut old = stub_node(env, "by-config", &addr);
        old.rpc_auth = None;
        let a = Agent::new(old.clone(), fast());
        a.start().await.unwrap();
        wait_up(&a).await;
        let pid = a.view().pid.unwrap();
        // A start without a login still writes the file, but without `[rpc_auth]`.
        let before = std::fs::read_to_string(env.root.join("networks/regtest/blvm.toml")).unwrap();
        assert!(!before.contains("rpc_auth"), "{before}");
        assert_eq!(crate::rpc::probe_login(&addr).await, crate::rpc::LoginProbe::Open, "it really is open");
        a.detach(); // the old console ends; the node outlives it
        let mut cfg = old;
        cfg.rpc_auth = Some(login());
        let (up, ready) = ready_flag();
        up.store(true, Ordering::Relaxed); // the poller reads it fine: an open node ignores the password
        let b = Agent::with_ready(cfg, fast(), ready);
        b.restart_adopted_node_with_login(flag);
        assert!(b.adopt(), "the running node is adopted");
        (b, pid, up)
    }

    #[tokio::test]
    async fn an_adopted_node_without_a_password_is_restarted_with_one_when_the_login_is_new() {
        let env = setup("login-upgrade");
        let (b, old_pid, _up) = open_node_adopted_with_a_new_login(&env, true).await;
        wait_for("restarted and running", || {
            let v = b.view();
            v.state == ProcState::Running && v.pid.is_some_and(|p| p != old_pid)
        })
        .await;
        assert!(!pid_alive(old_pid), "the old node is gone");
        let v = b.view();
        assert_eq!(v.notice.as_deref(), Some(LOGIN_RESTART_NOTICE));
        assert_eq!(v.last_error, None, "this is not a failure");
        assert!(!v.adopted, "it is the console's own process now");
        // It was started with the login: the config file is there, and the node says so in its log.
        let file = env.root.join("networks/regtest/blvm.toml");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), node_config_toml(Some(&login()), None));
        let log: String = b.logs(None, 200).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n");
        assert!(log.contains("[console] Restarted the node to turn on its RPC password"), "{log}");
        assert!(log.contains("--config ./blvm.toml"), "{log}");
        assert!(!log.contains("p4ss"), "{log}");
        // The next stop clears the note.
        b.stop_and_wait().await;
        assert_eq!(b.view().notice, None);
        cleanup(&env);
    }

    #[tokio::test]
    async fn an_adopted_node_without_a_password_is_stopped_when_the_login_is_not_new() {
        // The safety net is for a node that SHOULD have a login: it keeps its behaviour and its message.
        let env = setup("login-no-upgrade");
        let (b, old_pid, _up) = open_node_adopted_with_a_new_login(&env, false).await;
        wait_for("stopped for safety", || matches!(b.state(), ProcState::Stopped | ProcState::Crashed)).await;
        let v = b.view();
        assert_eq!(v.state, ProcState::Crashed);
        assert!(v.last_error.unwrap_or_default().contains("without asking for the RPC password"));
        assert_eq!(v.notice, None);
        wait_for("old node gone", || !pid_alive(old_pid)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(b.state(), ProcState::Crashed, "nothing restarts it");
        cleanup(&env);
    }

    #[tokio::test]
    async fn the_real_node_started_before_logins_existed_comes_back_with_a_password() {
        let Some(bin) = real_node() else {
            eprintln!("SKIPPED: no built blvm (blvm/target/release-fast/blvm or BLVM_TEST_NODE_BIN)");
            return;
        };
        let env = setup("real-upgrade");
        let rpc = format!("127.0.0.1:{}", free_port());
        let mut c = cfg(&env, "run");
        c.bin = Some(bin);
        c.child_env.clear();
        c.rpc_addr = Some(rpc.clone());
        let mut t = fast();
        t.stop_wait = Duration::from_secs(30);
        t.kill_wait = Duration::from_secs(10);
        t.start_timeout = Duration::from_secs(60);
        // The old console: no login, a node that answers anybody.
        let a = Agent::new(c.clone(), t.clone());
        a.start().await.unwrap();
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end && !(std::net::TcpStream::connect(&rpc).is_ok() && raw_rpc(&rpc, None).await.0 == 200) {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(raw_rpc(&rpc, None).await.0, 200, "open before the upgrade");
        let old_pid = a.view().pid.unwrap();
        a.detach();
        // The new console: a generated login, saved, and the node adopted.
        let auth = RpcAuth { user: "commons".into(), pass: "correct-horse-battery".into() };
        c.rpc_auth = Some(auth.clone());
        let (up, ready) = ready_flag();
        up.store(true, Ordering::Relaxed);
        let b = Agent::with_ready(c, t, ready);
        b.restart_adopted_node_with_login(true);
        assert!(b.adopt());
        wait_for("restarted and running", || {
            let v = b.view();
            v.state == ProcState::Running && v.pid.is_some_and(|p| p != old_pid)
        })
        .await;
        assert_eq!(b.view().notice.as_deref(), Some(LOGIN_RESTART_NOTICE));
        assert_eq!(raw_rpc(&rpc, None).await.0, 401, "now it asks");
        assert_eq!(raw_rpc(&rpc, Some((&auth.user, &auth.pass))).await.0, 200);
        b.stop_and_wait().await;
        cleanup(&env);
    }

    /// The `blvm` of this workspace, when it has been built (`BLVM_TEST_NODE_BIN` names another).
    fn real_node() -> Option<PathBuf> {
        let from_env = std::env::var_os("BLVM_TEST_NODE_BIN").map(PathBuf::from);
        let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("../blvm/target/release-fast/blvm");
        from_env.into_iter().chain([here]).find(|p| p.is_file())
    }

    async fn raw_rpc(addr: &str, auth: Option<(&str, &str)>) -> (u16, String) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"getblockcount","params":[]}"#;
        let auth = auth
            .map(|(u, p)| format!("Authorization: {}\r\n", crate::rpc::basic_auth_header(u, p)))
            .unwrap_or_default();
        let req = format!(
            "POST / HTTP/1.1\r\nHost: {addr}\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        let text = String::from_utf8_lossy(&out).to_string();
        let code = text.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        (code, text.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
    }

    /// The real node, started the way the console starts it (regtest, its own folder, the generated config):
    /// without the password it says 401, with it it answers.
    #[tokio::test]
    async fn the_real_node_refuses_unauthenticated_rpc_and_answers_with_the_password() {
        let Some(bin) = real_node() else {
            eprintln!("SKIPPED: no built blvm (blvm/target/release-fast/blvm or BLVM_TEST_NODE_BIN)");
            return;
        };
        let env = setup("real-login");
        let rpc = format!("127.0.0.1:{}", free_port());
        let auth = RpcAuth { user: "commons".into(), pass: "correct-horse-battery".into() };
        let mut c = cfg(&env, "run");
        c.bin = Some(bin);
        c.child_env.clear();
        c.rpc_addr = Some(rpc.clone());
        c.p2p_port = Some(free_port()); // and, with it, `--listen-addr 127.0.0.1:<port>`: loopback only
        c.rpc_auth = Some(auth.clone());
        let mut t = fast();
        t.stop_wait = Duration::from_secs(30);
        t.kill_wait = Duration::from_secs(10);
        t.start_timeout = Duration::from_secs(60);
        // Ready once the node answers the right login (the console's poller does this in real life).
        let ready = Arc::new(AtomicBool::new(false));
        let r2 = Arc::clone(&ready);
        let agent = Agent::with_ready(c, t, Arc::new(move || r2.load(Ordering::Relaxed)));
        agent.start().await.unwrap();
        let end = Instant::now() + Duration::from_secs(60);
        let mut first = (0, String::new());
        while Instant::now() < end {
            if std::net::TcpStream::connect(&rpc).is_ok() {
                first = raw_rpc(&rpc, Some((&auth.user, &auth.pass))).await;
                if first.0 == 200 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert_eq!(first.0, 200, "the right login is accepted: {first:?}\n{}", agent.logs(None, 50).into_iter().map(|l| l.text).collect::<Vec<_>>().join("\n"));
        assert!(first.1.contains("\"result\":0"), "{first:?}");
        // Without a login, and with a wrong one: refused (two failures; the node blocks an address after five).
        let (code, body) = raw_rpc(&rpc, None).await;
        assert_eq!(code, 401, "{body}");
        let (code, _) = raw_rpc(&rpc, Some(("commons", "wrong"))).await;
        assert_eq!(code, 401);
        // The same login still works afterwards.
        let (code, _) = raw_rpc(&rpc, Some((&auth.user, &auth.pass))).await;
        assert_eq!(code, 200);
        // The agent's own safety check agrees, and lets the node become `running`.
        assert_eq!(crate::rpc::probe_login(&rpc).await, crate::rpc::LoginProbe::Locked);
        ready.store(true, Ordering::Relaxed);
        wait_for("running", || agent.state() == ProcState::Running).await;
        // The console's RPC client, given the login, can poll it too.
        let probe = crate::rpc::probe_once(&rpc, Some((&auth.user, &auth.pass))).await.unwrap();
        assert!(probe[0].is_some(), "getblockchaininfo answered");
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn review_restart_during_crash_backoff_starts_exactly_once() {
        let env = setup("rv-backoff");
        std::fs::write(&env.bin, ONCE_CRASH).unwrap();
        let mut t = fast();
        t.backoff = vec![Duration::from_millis(1200), Duration::from_millis(1200), Duration::from_millis(1200)];
        let agent = Agent::new(cfg(&env, "run"), t);
        agent.start().await.unwrap();
        wait_for("first crash", || agent.state() == ProcState::Crashed).await;
        // User presses Restart while the automatic restart is pending.
        agent.restart().await.unwrap();
        let pid = agent.view().pid.expect("restarted at once");
        tokio::time::sleep(Duration::from_millis(1800)).await;
        let v = agent.view();
        assert_eq!(v.pid, Some(pid), "the pending automatic restart must not start a second process");
        assert_eq!(v.restarts, 0);
        agent.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn review_adoption_checks_the_data_root_not_only_the_binary_name() {
        let env = setup("rv-adopt-root");
        let a = Agent::new(cfg(&env, "run"), fast());
        a.start().await.unwrap();
        let pid = a.view().pid.unwrap();
        wait_up(&a).await;
        a.detach();
        // A second data root whose stale pid file names that pid (pid recycled to another blvm).
        let mut other = cfg(&env, "run");
        other.root = env.root.parent().unwrap().join("other-root");
        write_pid_file(&other.root.clone(), pid, "regtest", &env.bin);
        let _ = std::fs::create_dir_all(other.root.join("control"));
        write_pid_file(&other.root, pid, "regtest", &env.bin);
        let b = Agent::new(other, fast());
        let adopted = b.adopt();
        kill_hard(pid);
        cleanup(&env);
        assert!(!adopted, "adopted a node that runs in another data root (cwd/--data-dir not checked)");
    }

    #[tokio::test]
    async fn review_start_refuses_when_the_rpc_port_is_already_taken() {
        let env = setup("rv-port");
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut c = cfg(&env, "run");
        c.rpc_addr = Some(format!("127.0.0.1:{port}"));
        let agent = Agent::new(c, fast());
        // The poller is already talking to whatever owns the port.
        agent.rpc_flag().store(true, Ordering::Relaxed);
        let r = agent.start().await;
        let started = r.is_ok() && agent.view().pid.is_some();
        // Within a moment a foreign answer marks our process "running".
        tokio::time::sleep(Duration::from_millis(400)).await;
        let state = agent.state();
        agent.stop_and_wait().await;
        cleanup(&env);
        drop(l);
        assert!(!started, "started a node although 127.0.0.1:{port} is in use (state became {state:?})");
    }

    #[tokio::test]
    async fn review_a_crash_takes_the_nodes_helpers_down_too() {
        let env = setup("rv-group");
        std::fs::write(
            &env.bin,
            "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\nsleep 300 &\necho \"child=$!\"\nsleep 0.3\nexit 3\n",
        )
        .unwrap();
        let mut t = fast();
        t.backoff = vec![];
        let agent = Agent::new(cfg(&env, "run"), t);
        agent.start().await.unwrap();
        wait_for("crash", || agent.state() == ProcState::Crashed).await;
        let child: u32 = agent
            .logs(None, 50)
            .iter()
            .find_map(|l| l.text.strip_prefix("child=").and_then(|p| p.trim().parse().ok()))
            .expect("child pid in log");
        tokio::time::sleep(Duration::from_millis(500)).await;
        let alive = pid_alive(child);
        unsafe {
            libc::kill(child as i32, libc::SIGKILL);
        }
        cleanup(&env);
        assert!(!alive, "a module/helper of the crashed node is still running (pid {child}); the restart would start a second one");
    }

    // ---- fix pass for the U0/U1 review (B3, S3, S4, S6, S9) ----

    /// Kills the node (and its group) when a failing test unwinds, so no fake node outlives its test.
    struct Reaper(Agent);
    impl Drop for Reaper {
        fn drop(&mut self) {
            if let Some(pid) = self.0.view().pid {
                kill_hard(pid);
            }
            self.0.detach();
        }
    }

    #[test]
    fn ansi_escape_sequences_are_stripped_from_log_lines() {
        // S9: the node writes tracing colours into node.log.
        assert_eq!(strip_ansi("\u{1b}[2m2026-10-12T10:00:00Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m blvm: started"), "2026-10-12T10:00:00Z  INFO blvm: started");
        assert_eq!(strip_ansi("\u{1b}[1;31;40mred\u{1b}[0m"), "red");
        assert_eq!(strip_ansi("a\u{1b}[2Kb\u{1b}[10;20Hc"), "abc");
        assert_eq!(strip_ansi("\u{1b}]0;window title\u{7}after"), "after");
        assert_eq!(strip_ansi("\u{1b}]8;;http://x\u{1b}\\link\u{1b}]8;;\u{1b}\\"), "link");
        assert_eq!(strip_ansi("\u{1b}(Bplain"), "plain");
        assert_eq!(strip_ansi("tab\tkept, bell\u{7} and nul\u{0} dropped"), "tab\tkept, bell and nul dropped");
        assert_eq!(strip_ansi("no escapes [2m here"), "no escapes [2m here", "only real escapes are removed");
        assert_eq!(strip_ansi("trailing \u{1b}"), "trailing ");
        assert_eq!(strip_ansi("unicode ✓ ünï"), "unicode ✓ ünï");
    }

    #[tokio::test]
    async fn the_ring_and_the_crash_log_carry_no_escape_sequences() {
        let env = setup("ansi");
        let net = networks::by_id("regtest").unwrap();
        let log = networks::log_file(&env.root, net);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m hello\nplain\n").unwrap();
        let agent = Agent::new(cfg(&env, "run"), fast());
        agent.inner.ensure_tailer();
        wait_for("preload", || agent.logs(None, 10).len() == 2).await;
        let texts: Vec<String> = agent.logs(None, 10).into_iter().map(|l| l.text).collect();
        assert_eq!(texts, ["2026  INFO hello", "plain"]);
        assert!(texts.iter().all(|t| !t.contains('\u{1b}')));
        cleanup(&env);

        // The crash block comes from the same ring.
        let env = setup("ansi-crash");
        std::fs::write(
            &env.bin,
            "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\nprintf '\\033[31mboom\\033[0m\\n'\nexit 3\n",
        )
        .unwrap();
        let mut t = fast();
        t.backoff = vec![];
        let agent = Agent::new(cfg(&env, "run"), t);
        agent.start().await.unwrap();
        wait_for("crash", || agent.state() == ProcState::Crashed).await;
        let v = agent.view();
        assert!(v.crash_log.iter().any(|l| l == "boom"), "{:?}", v.crash_log);
        assert!(v.crash_log.iter().all(|l| !l.contains('\u{1b}')), "{:?}", v.crash_log);
        cleanup(&env);
    }

    fn numbered_lines(from: usize, to: usize) -> String {
        (from..=to).map(|i| format!("line {i}\n")).collect()
    }

    #[tokio::test]
    async fn the_tailer_starts_with_the_last_500_lines_then_follows_appends_and_truncation() {
        for external in [false, true] {
            let env = setup(if external { "tail-ext" } else { "tail" });
            let net = networks::by_id("regtest").unwrap();
            let log = networks::log_file(&env.root, net);
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            // The file is there before the console starts.
            std::fs::write(&log, numbered_lines(1, 1000)).unwrap();
            let agent = if external { Agent::external(cfg(&env, "run"), fast()) } else { Agent::new(cfg(&env, "run"), fast()) };
            // The very first request already has them (the external client reads on demand).
            agent.inner.ensure_tailer();
            wait_for("first 500", || agent.logs(None, 1000).len() == 500).await;
            let first = agent.logs(None, 1000);
            assert_eq!((first[0].seq, first[0].text.as_str()), (1, "line 501"));
            assert_eq!((first[499].seq, first[499].text.as_str()), (500, "line 1000"));
            assert!(first.windows(2).all(|w| w[1].seq == w[0].seq + 1), "sequence numbers are consecutive");

            // Appended lines follow, after the last sequence number the page saw.
            let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            f.write_all(numbered_lines(1001, 1003).as_bytes()).unwrap();
            wait_for("appended lines", || agent.logs(Some(500), 1000).len() == 3).await;
            let more = agent.logs(Some(500), 1000);
            assert_eq!(more.iter().map(|l| (l.seq, l.text.as_str())).collect::<Vec<_>>(), [(501, "line 1001"), (502, "line 1002"), (503, "line 1003")]);
            // A line is not shown before it is finished.
            f.write_all(b"half a li").unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            assert_eq!(agent.logs(Some(503), 10).len(), 0);
            f.write_all(b"ne\n").unwrap();
            wait_for("finished line", || agent.logs(Some(503), 10).iter().any(|l| l.text == "half a line")).await;

            // Truncation (rotation by copy-truncate, or a cleared file): reading starts again from the top.
            f.set_len(0).unwrap();
            drop(f);
            std::fs::write(&log, "fresh 1\nfresh 2\n").unwrap();
            wait_for("after truncation", || agent.logs(Some(504), 10).iter().any(|l| l.text == "fresh 2")).await;
            let after = agent.logs(Some(504), 10);
            assert_eq!(after.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["fresh 1", "fresh 2"]);
            assert!(after[0].seq > 504, "sequence numbers keep counting up");
            cleanup(&env);
        }
    }

    #[tokio::test]
    async fn the_first_read_of_a_short_or_exact_file_loses_no_line() {
        for (n, tail_newline) in [(3usize, true), (500, true), (501, true), (500, false), (1, false)] {
            let env = setup("tail-edge");
            let net = networks::by_id("regtest").unwrap();
            let log = networks::log_file(&env.root, net);
            std::fs::create_dir_all(log.parent().unwrap()).unwrap();
            let mut text = numbered_lines(1, n);
            if !tail_newline {
                text.pop();
            }
            std::fs::write(&log, text).unwrap();
            let agent = Agent::new(cfg(&env, "run"), fast());
            agent.inner.ensure_tailer();
            let want = if tail_newline { n.min(500) } else { n.min(500) - 1 };
            wait_for("preload", || agent.logs(None, 1000).len() >= want).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let got = agent.logs(None, 1000);
            assert_eq!(got.len(), want, "{n} lines, final newline {tail_newline}");
            if tail_newline {
                assert_eq!(got.last().unwrap().text, format!("line {n}"));
                assert_eq!(got[0].text, format!("line {}", n + 1 - want));
            }
            cleanup(&env);
        }
    }

    #[tokio::test]
    async fn the_nodes_stdout_and_stderr_reach_node_log_at_once_and_without_the_consoles_log_filter() {
        // The console's own filter (the Docker image sets one that names only the console's crate) must not reach
        // the node: it would silence every line the node writes. The Child environment setting still can.
        std::env::set_var("RUST_LOG", "blvm_ui_next=info");
        for (child_filter, expect) in [(None, "filter=unset"), (Some("debug"), "filter=debug")] {
            let env = setup("node-log");
            // A node that writes to both streams and keeps running; it also says which log filter it got.
            std::fs::write(
                &env.bin,
                "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'blvm 9.9.9'; exit 0; fi\ntrap 'exit 0' TERM\n\
                 echo \"filter=${RUST_LOG-unset}\"\necho 'to stdout INFO'\necho 'to stderr WARN' >&2\n\
                 while kill -0 $PPID 2>/dev/null; do sleep 0.1; done\n",
            )
            .unwrap();
            let mut c = cfg(&env, "run");
            if let Some(f) = child_filter {
                c.child_env.insert("RUST_LOG".into(), f.into());
            }
            let agent = Agent::new(c, fast());
            agent.start().await.unwrap();
            let _reap = Reaper(agent.clone());
            let log = networks::log_file(&env.root, networks::by_id("regtest").unwrap());
            // Straight from the file, not through the ring: the node's own descriptor, no buffering in between.
            wait_for("both streams in node.log", || {
                let t = std::fs::read_to_string(&log).unwrap_or_default();
                t.contains("to stdout INFO") && t.contains("to stderr WARN")
            })
            .await;
            let t = std::fs::read_to_string(&log).unwrap();
            assert!(t.lines().any(|l| l == expect), "expected {expect} in {t}");
            wait_for("the page sees them", || agent.logs(None, 50).iter().any(|l| l.text == "to stderr WARN")).await;
            agent.stop_and_wait().await;
            cleanup(&env);
        }
        std::env::remove_var("RUST_LOG");
    }

    #[tokio::test]
    async fn b3_a_foreign_listener_on_the_rpc_port_is_named_and_refused() {
        let env = setup("b3-name");
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut c = cfg(&env, "run");
        c.rpc_addr = Some(format!("127.0.0.1:{port}"));
        let agent = Reaper(Agent::new(c, fast()));
        let e = agent.0.start().await.unwrap_err();
        let me = std::process::id();
        assert!(e.contains(&format!("RPC port {port} is in use by pid {me}")), "{e}");
        assert!(e.contains("Stop that program"), "{e}");
        let v = agent.0.view();
        assert_eq!((v.state, v.pid, v.conflict), (ProcState::Crashed, None, true));
        assert_eq!(v.last_error.as_deref(), Some(e.as_str()));
        assert!(!env.root.join("networks/regtest/logs/node.log").exists(), "nothing was spawned or opened");
        assert!(!env.root.join("control/node.pid").exists());
        // No retry loop for a conflict, and once the port is free the next start works.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(agent.0.state(), ProcState::Crashed);
        drop(l);
        agent.0.start().await.unwrap();
        assert!(!agent.0.view().conflict, "a new start clears the conflict");
        assert!(agent.0.view().pid.is_some());
        agent.0.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn b3_a_foreign_listener_on_the_p2p_port_is_refused_too() {
        let env = setup("b3-p2p");
        // Any free port will do as the node's P2P port (and the listener that is already there takes it).
        let l = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let mut c = cfg(&env, "run");
        c.network = networks::by_id("signet");
        c.p2p_port = Some(port);
        let agent = Reaper(Agent::new(c, fast()));
        let e = agent.0.start().await.unwrap_err();
        assert!(e.contains(&format!("P2P port {port} is in use by pid")), "{e}");
        assert!(agent.0.view().conflict);
        drop(l);
        cleanup(&env);
    }

    #[tokio::test]
    async fn b3_a_node_whose_rpc_port_is_taken_over_is_a_conflict_never_running() {
        // The port is free at spawn time (so the preflight passes), then another program takes it and
        // answers: the poller's "RPC answers" flag goes up, but the answer is not from our node.
        let env = setup("b3-takeover");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut c = cfg(&env, "run");
        c.rpc_addr = Some(format!("127.0.0.1:{port}"));
        let agent = Reaper(Agent::new(c, fast()));
        agent.0.start().await.unwrap();
        let pid = agent.0.view().pid.unwrap();
        wait_up(&agent.0).await;
        let l = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
        agent.0.rpc_flag().store(true, Ordering::Relaxed);
        // Sample fast: the state must never read `running`, not even for a moment.
        let mut saw_running = false;
        let end = Instant::now() + Duration::from_secs(10);
        while !agent.0.view().conflict && Instant::now() < end {
            saw_running |= agent.0.state() == ProcState::Running;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(agent.0.view().conflict, "conflict not noticed");
        assert!(!saw_running, "another program's answer made our node `running`");
        wait_for("our useless node is gone", || agent.0.state() == ProcState::Crashed && !pid_alive(pid)).await;
        let v = agent.0.view();
        assert!(v.last_error.as_deref().unwrap().contains(&format!("RPC port {port} is in use by pid {}", std::process::id())), "{:?}", v.last_error);
        assert_eq!(v.pid, None);
        // It never was `running`, and it does not come back by itself.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(agent.0.state(), ProcState::Crashed);
        assert_eq!(agent.0.view().restarts, 0);
        drop(l);
        cleanup(&env);
    }

    #[tokio::test]
    async fn b3_running_needs_the_rpc_port_to_stay_ours() {
        // A node that is `running` and then finds another program on its port is not running any more.
        let env = setup("b3-recheck");
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut c = cfg(&env, "run");
        c.rpc_addr = Some(format!("127.0.0.1:{port}"));
        let agent = Reaper(Agent::new(c, fast()));
        agent.0.start().await.unwrap();
        agent.0.rpc_flag().store(true, Ordering::Relaxed);
        wait_for("running", || agent.0.state() == ProcState::Running).await;
        // (the poller's flag is not what notices: the periodic owner check is)
        agent.0.rpc_flag().store(false, Ordering::Relaxed);
        let _l = std::net::TcpListener::bind(format!("127.0.0.1:{port}")).unwrap();
        wait_for("conflict is noticed", || agent.0.view().conflict).await;
        wait_for("crashed", || agent.0.state() == ProcState::Crashed).await;
        assert!(agent.0.view().last_error.unwrap().contains("is in use by pid"));
        cleanup(&env);
    }

    #[tokio::test]
    async fn s3_a_node_that_keeps_writing_its_log_is_not_killed_for_a_slow_rpc() {
        let env = setup("s3-chatty");
        let mut t = fast();
        t.start_timeout = Duration::from_millis(1000);
        t.start_max = Duration::from_millis(3600);
        let agent = Reaper(Agent::new(cfg(&env, "chatty"), t));
        // The first run of a fresh script can take a second on macOS (the scan): get it over with, so the
        // start windows below measure the node and not the OS.
        agent.0.refresh_installed(false).await;
        let t0 = Instant::now();
        agent.0.start().await.unwrap();
        let pid = agent.0.view().pid.unwrap();
        // Well past the first window: still starting, still alive.
        tokio::time::sleep(Duration::from_millis(2200)).await;
        assert_eq!(agent.0.state(), ProcState::Starting, "{:?}", agent.0.view());
        assert!(pid_alive(pid));
        // The cap ends it.
        wait_for("cap reached", || agent.0.state() == ProcState::Crashed).await;
        let waited = t0.elapsed();
        assert!(waited >= Duration::from_millis(3400), "stopped after {waited:?}, before the cap");
        assert!(agent.0.view().last_error.unwrap().contains("did not answer"));
        assert!(!pid_alive(pid));
        cleanup(&env);
    }

    #[tokio::test]
    async fn s3_a_chatty_node_that_then_answers_becomes_running() {
        let env = setup("s3-answers");
        let mut t = fast();
        t.start_timeout = Duration::from_millis(800);
        t.start_max = Duration::from_secs(10);
        let agent = Reaper(Agent::new(cfg(&env, "chatty"), t));
        agent.0.refresh_installed(false).await; // pre-warm the script (see above)
        agent.0.start().await.unwrap();
        tokio::time::sleep(Duration::from_millis(1600)).await;
        assert_eq!(agent.0.state(), ProcState::Starting, "{:?}", agent.0.view());
        agent.0.rpc_flag().store(true, Ordering::Relaxed);
        wait_for("running", || agent.0.state() == ProcState::Running).await;
        agent.0.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn s3_a_silent_node_is_still_stopped_after_two_quiet_windows() {
        let env = setup("s3-quiet");
        let mut t = fast();
        t.start_timeout = Duration::from_millis(400);
        t.start_max = Duration::from_secs(20);
        let agent = Reaper(Agent::new(cfg(&env, "run"), t));
        agent.0.refresh_installed(false).await; // pre-warm the script
        let t0 = Instant::now();
        agent.0.start().await.unwrap();
        wait_for("gave up", || agent.0.state() == ProcState::Crashed).await;
        // One extension for the lines it wrote at start, then the quiet window ends it: far below the cap.
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        let e = agent.0.view().last_error.unwrap();
        assert!(e.contains("did not answer") && e.contains("quiet"), "{e}");
        cleanup(&env);
    }

    #[tokio::test]
    async fn s4_adoption_accepts_the_node_in_its_own_network_folder() {
        // The positive side of the data-root check: same root, same network folder, same binary.
        let env = setup("s4-ok");
        let a = Agent::new(cfg(&env, "run"), fast());
        a.start().await.unwrap();
        let pid = a.view().pid.unwrap();
        wait_up(&a).await;
        assert!(runs_in(pid, &networks::dir(&env.root, networks::by_id("regtest").unwrap())));
        assert!(!runs_in(pid, &env.root));
        assert!(!runs_in(pid, &networks::dir(&env.root, networks::by_id("signet").unwrap())));
        a.detach();
        let b = Agent::new(cfg(&env, "run"), fast());
        assert!(b.adopt());
        b.stop_and_wait().await;
        cleanup(&env);
    }

    #[tokio::test]
    async fn s4_a_node_of_another_network_is_reported_not_adopted() {
        let env = setup("s4-other-net");
        let a = Agent::new(cfg(&env, "run"), fast());
        a.start().await.unwrap();
        let pid = a.view().pid.unwrap();
        wait_up(&a).await;
        a.detach();
        let mut c = cfg(&env, "run");
        c.network = networks::by_id("signet");
        let b = Agent::new(c, fast());
        assert!(!b.adopt());
        let e = b.view().last_error.expect("the earlier node is mentioned");
        assert!(e.contains("Regtest") && e.contains(&pid.to_string()), "{e}");
        assert_eq!(b.state(), ProcState::Stopped);
        kill_hard(pid);
        cleanup(&env);
    }
    /// N3: the hold is checked again under the lock that `confirm_stopped` reads `desired.running` with. A Start
    /// that passed the check in `Agent::start` just before the hold went on is refused when it comes to writing
    /// `desired.json` (the test goes straight to the external client, as that start does after the check).
    #[tokio::test]
    async fn n3_an_external_start_or_restart_that_slipped_past_the_hold_check_writes_nothing() {
        let env = setup("n3-ext-hold");
        let agent = Agent::external(cfg(&env, "run"), fast());
        let ext = agent.ext.as_ref().unwrap().clone();
        agent.hold_starts(true);
        assert_eq!(ext.start(&agent.inner).unwrap_err(), WIPING_MSG);
        assert!(crate::control::read_desired(&env.root).is_none(), "no start was written");
        // A Restart of a node that runs: the same write, with `restart_id`.
        agent.hold_starts(false);
        ext.start(&agent.inner).unwrap();
        let d = crate::control::read_desired(&env.root).unwrap();
        assert!(d.running);
        let state = crate::control::StateFile { heartbeat: crate::persist::unix_now(), generation: d.generation, state: "running".into(), ..Default::default() };
        crate::control::write_state(&env.root, &state).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await; // (state.json is cached for 200 ms)
        agent.hold_starts(true);
        assert_eq!(ext.restart(&agent.inner).unwrap_err(), WIPING_MSG);
        let d2 = crate::control::read_desired(&env.root).unwrap();
        assert_eq!((d2.generation, d2.restart_id), (d.generation, d.restart_id), "no restart was written");
        // A Stop and a settings-only write go through under the hold (a wipe allows them).
        ext.request_stop(&agent.inner);
        assert!(!crate::control::read_desired(&env.root).unwrap().running, "Stop is not held back");
        cleanup(&env);
    }

}
