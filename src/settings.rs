//! Settings that can be baked in when the binary is built and overridden at runtime.
//!
//! Build time: `BLVM_UI_SELF_GEO=timezone cargo build --release`
//! Runtime:    `BLVM_UI_SELF_GEO=off ./blvm-ui-next`
//!
//! Runtime wins. Cargo rebuilds automatically when a baked-in value changes.

/// Runtime env var, else the value set when the binary was built (`option_env!`).
pub fn setting(name: &str, built: Option<&'static str>) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or_else(|| built.filter(|v| !v.trim().is_empty()).map(str::to_string))
}

// ---------------------------------------------------------------------------------------------
// Console settings and mode (managed-node design, sections A and K).
//
// Precedence, strongest first: environment variable > `console.json` > bundle marker > default.
// The file lives next to `feeds.json` in the state directory and is written atomically.
// ---------------------------------------------------------------------------------------------

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const CONSOLE_FILE: &str = "console.json";
const CONSOLE_VERSION: u32 = 1;
/// A file with this name next to the console executable, plus a `blvm` binary beside it, means
/// "this is a bundle: manage the node for the user".
pub const BUNDLE_MARKER: &str = "blvm-bundle.toml";
const NODE_BIN_NAME: &str = if cfg!(windows) { "blvm.exe" } else { "blvm" };

/// `<root>/bin/blvm`: the program the in-app installer keeps current (a link to `bin/blvm-<tag>`).
pub fn installed_bin(root: &Path) -> PathBuf {
    root.join("bin").join("blvm")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Standalone,
    Managed,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Standalone => "standalone",
            Mode::Managed => "managed",
        }
    }

    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standalone" => Some(Mode::Standalone),
            "managed" => Some(Mode::Managed),
            _ => None,
        }
    }
}

/// What `console.json` holds. Every field has a default, so older and newer files both load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsoleSettings {
    pub version: u32,
    pub setup_done: bool,
    /// `"standalone"` / `"managed"`; kept as text so a hand-edited typo cannot make the file unreadable.
    pub mode: Option<String>,
    /// Standalone target (`host:port`).
    pub rpc: Option<String>,
    /// Console network id (`mainnet`, `testnet4`, ...). There is no default: nothing starts without one.
    pub network: Option<String>,
    pub data_root: Option<String>,
    pub node_bin: Option<String>,
    /// Start the node when the console starts (`None`: yes in managed mode). Read as `start_on_launch` too:
    /// that was the first name of this setting.
    #[serde(alias = "start_on_launch")]
    pub start_on_boot: Option<bool>,
    /// What the user last asked for: `true` after Start (or a setup, a network switch or an update that
    /// started the node), `false` after Stop. `None`: never asked (an older file), treated as `true`.
    /// "Start when the console opens" only applies while this is true, so a Stop survives any restart.
    pub node_wanted: Option<bool>,
    /// The login of the node's RPC when the console starts the node itself. Generated once, kept here
    /// (this file is private to the user, mode 0600). `BLVM_UI_RPC_USER` / `BLVM_UI_RPC_PASS` win.
    pub rpc_user: Option<String>,
    pub rpc_pass: Option<String>,
    /// Modules the setup walkthrough asked for (installed once module installs exist).
    pub modules_selected: Vec<String>,
    /// Stop the node when the console quits (default). `false` leaves it running.
    pub stop_node_on_exit: bool,
    /// Show the Logs page in the sidebar. Off by default: the page is the node's raw log output.
    pub show_logs: bool,
    pub modules_disabled: Vec<String>,
    /// `self` | `store` | `off`.
    pub updates: String,
    /// Network id -> `host:port` for the node's RPC, when not the network's default.
    pub rpc_overrides: BTreeMap<String, String>,
    /// `None`/`"default"`: per-network default (full validation on Testnet4 and regtest, node
    /// default elsewhere); `"off"`: validate every block; a height or 64-hex block hash.
    pub assume_valid: Option<String>,
    /// Extra environment for the node process; an empty value unsets the variable. The agent adds
    /// `BLVM_SCHNORR_PER_SIG=1` unless it is listed here.
    pub child_env: BTreeMap<String, String>,
}

impl ConsoleSettings {
    /// Should the console start the node by itself when it starts? On by default for a managed node: the
    /// user set it up once and a restart of the computer or the app must not repeat any of it.
    pub fn start_on_boot_effective(&self, managed: bool) -> bool {
        self.start_on_boot.unwrap_or(managed)
    }

    /// The user did not leave the node stopped.
    pub fn node_wanted_effective(&self) -> bool {
        self.node_wanted.unwrap_or(true)
    }
}

impl Default for ConsoleSettings {
    fn default() -> Self {
        Self {
            version: CONSOLE_VERSION,
            setup_done: false,
            mode: None,
            rpc: None,
            network: None,
            data_root: None,
            node_bin: None,
            start_on_boot: None,
            node_wanted: None,
            rpc_user: None,
            rpc_pass: None,
            modules_selected: Vec::new(),
            stop_node_on_exit: true,
            show_logs: false,
            modules_disabled: Vec::new(),
            updates: "self".into(),
            rpc_overrides: BTreeMap::new(),
            assume_valid: None,
            child_env: BTreeMap::new(),
        }
    }
}

/// `console.json` in `dir`, or defaults when missing or unreadable.
pub fn load_console(dir: &Path) -> ConsoleSettings {
    let path = dir.join(CONSOLE_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<ConsoleSettings>(&bytes) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("console settings: cannot read {}: {e}", path.display());
                ConsoleSettings::default()
            }
        },
        Err(_) => ConsoleSettings::default(),
    }
}

/// Atomic write: temp file, flushed to disk, renamed, folder flushed. The file may hold the RPC password
/// the console generated, so it is private to the user (0600) from the moment it exists.
pub fn save_console(dir: &Path, s: &ConsoleSettings) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let body = serde_json::to_vec_pretty(s).map_err(std::io::Error::other)?;
    crate::fsutil::write_atomic_bytes(&dir.join(CONSOLE_FILE), &body, Some(0o600))
}

/// `n` random bytes as lowercase hex, from the system's random source.
pub fn random_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let ok = cfg!(unix) && std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)).is_ok();
    if !ok {
        // No system source (not expected on the systems the node runs on): the std hasher is seeded randomly.
        use std::hash::{BuildHasher, Hasher};
        for chunk in buf.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u64(std::process::id() as u64);
            let v = h.finish().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The environment variables that matter here, captured once (so tests can pass their own).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EnvOverrides {
    pub mode: Option<String>,
    pub node_bin: Option<String>,
    pub data_root: Option<String>,
    pub rpc: Option<String>,
    pub node_version: Option<String>,
    /// `BLVM_UI_SETUP=skip`.
    pub setup_skip: bool,
    /// `BLVM_UI_AGENT=external`: the node runs under an agent in another container.
    pub agent_external: bool,
    /// `BLVM_UI_NODE_HOST`: where the console finds the node's RPC when it is not on this machine.
    pub node_host: Option<String>,
    /// `BLVM_UI_UPDATES`: `self` | `store` | `off` (also `0` / `1`). Wins over the file.
    pub updates: Option<String>,
    /// `BLVM_UI_RPC_USER` / `BLVM_UI_RPC_PASS`: the node's RPC login (both or neither).
    pub rpc_user: Option<String>,
    pub rpc_pass: Option<String>,
}

/// `BLVM_UI_UPDATES` as a channel: `0 off false no` -> off, `store` -> store, `1 on true yes self` -> self.
pub fn parse_updates_env(v: &str) -> Option<&'static str> {
    match v.trim().to_ascii_lowercase().as_str() {
        "0" | "off" | "false" | "no" | "none" => Some("off"),
        "store" => Some("store"),
        "1" | "on" | "true" | "yes" | "self" => Some("self"),
        _ => None,
    }
}

impl EnvOverrides {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let mode = get("BLVM_UI_MODE").or_else(|| {
            // Alias: BLVM_UI_MANAGED=1 means managed.
            get("BLVM_UI_MANAGED")
                .filter(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
                .map(|_| "managed".to_string())
        });
        Self {
            mode,
            node_bin: get("BLVM_UI_NODE_BIN"),
            data_root: get("BLVM_UI_DATA_ROOT"),
            rpc: get("BLVM_UI_RPC"),
            node_version: get("BLVM_UI_NODE_VERSION"),
            setup_skip: get("BLVM_UI_SETUP").is_some_and(|v| v.trim().eq_ignore_ascii_case("skip")),
            agent_external: get("BLVM_UI_AGENT").is_some_and(|v| v.trim().eq_ignore_ascii_case("external")),
            node_host: get("BLVM_UI_NODE_HOST").map(|h| h.trim().to_string()),
            updates: get("BLVM_UI_UPDATES").and_then(|v| parse_updates_env(&v)).map(str::to_string),
            rpc_user: get("BLVM_UI_RPC_USER"),
            rpc_pass: std::env::var("BLVM_UI_RPC_PASS").ok().filter(|v| !v.is_empty()),
        }
    }
}

/// A node binary shipped beside the console (`blvm-bundle.toml` + `blvm`).
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    pub bin: PathBuf,
    /// `version = "0.1.60"` from the marker, when present.
    pub version: Option<String>,
}

pub fn detect_bundle(exe_dir: &Path) -> Option<Bundle> {
    let marker = exe_dir.join(BUNDLE_MARKER);
    let bin = exe_dir.join(NODE_BIN_NAME);
    if !marker.is_file() || !bin.is_file() {
        return None;
    }
    let version = std::fs::read_to_string(&marker).ok().and_then(|t| {
        t.lines().find_map(|l| {
            let (k, v) = l.split_once('=')?;
            (k.trim() == "version").then(|| v.trim().trim_matches('"').trim_matches('\'').to_string())
        })
    });
    Some(Bundle { bin, version: version.filter(|v| !v.is_empty()) })
}

/// Where the data root goes when nothing else says: `~/Library/Application Support/blvm` (macOS),
/// `$XDG_DATA_HOME/blvm` or `~/.local/share/blvm` (Linux), `%APPDATA%\blvm` (Windows).
pub fn default_data_root() -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let dir = home().map(|h| h.join("Library/Application Support/blvm"));
    #[cfg(windows)]
    let dir = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("blvm"));
    #[cfg(not(any(target_os = "macos", windows)))]
    let dir = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".local/share")))
        .map(|d| d.join("blvm"));
    dir.unwrap_or_else(|| PathBuf::from("blvm-data"))
}

/// What the console actually uses, after applying the precedence.
#[derive(Debug, Clone, PartialEq)]
pub struct Effective {
    pub mode: Mode,
    /// `env` | `file` | `bundle` | `default`.
    pub mode_source: &'static str,
    pub node_bin: Option<PathBuf>,
    pub node_bin_source: &'static str,
    pub data_root: PathBuf,
    pub data_root_source: &'static str,
    pub bundle: Option<Bundle>,
    pub setup_done: bool,
    /// Settings the environment pins (the file value is ignored while they are set).
    pub env_locked: Vec<&'static str>,
}

pub fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(h) = std::env::var_os("HOME") {
            return PathBuf::from(h).join(rest);
        }
    }
    PathBuf::from(p)
}

/// What the state directory already held when the console started. Computed once at startup (a console
/// that writes `feeds.json` itself must not flip its own answer halfway through a run).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateDirFacts {
    /// `console.json` exists (somebody saved settings).
    pub console_json: bool,
    /// `feeds.json` exists: this console has run here before, so this is not a fresh install.
    pub feeds_json: bool,
}

pub fn state_dir_facts(dir: Option<&Path>) -> StateDirFacts {
    match dir {
        Some(d) => StateDirFacts {
            console_json: d.join(CONSOLE_FILE).is_file(),
            feeds_json: d.join(crate::persist::FILE).is_file(),
        },
        None => StateDirFacts::default(),
    }
}

/// [`resolve_in`] for a state directory that held nothing yet.
pub fn resolve(file: &ConsoleSettings, env: &EnvOverrides, exe_dir: Option<&Path>) -> Effective {
    resolve_in(file, env, exe_dir, StateDirFacts::default())
}

pub fn resolve_in(file: &ConsoleSettings, env: &EnvOverrides, exe_dir: Option<&Path>, facts: StateDirFacts) -> Effective {
    let bundle = exe_dir.and_then(detect_bundle);
    let env_mode = env.mode.as_deref().and_then(Mode::parse);
    let file_mode = file.mode.as_deref().and_then(Mode::parse);
    let (mode, mode_source) = if let Some(m) = env_mode {
        (m, "env")
    } else if let Some(m) = file_mode {
        (m, "file")
    } else if bundle.is_some() {
        (Mode::Managed, "bundle")
    } else {
        (Mode::Standalone, "default")
    };

    let (data_root, data_root_source) = if let Some(r) = &env.data_root {
        (expand_home(r), "env")
    } else if let Some(r) = file.data_root.as_deref().filter(|r| !r.trim().is_empty()) {
        (expand_home(r), "file")
    } else {
        (default_data_root(), "default")
    };

    // The program: the environment decides first; then one the installer put in the data folder (a
    // downloaded node is found again on the next start); then the settings file; then a bundle.
    let (node_bin, node_bin_source) = if let Some(b) = &env.node_bin {
        (Some(expand_home(b)), "env")
    } else if installed_bin(&data_root).is_file() {
        (Some(installed_bin(&data_root)), "installed")
    } else if let Some(b) = file.node_bin.as_deref().filter(|b| !b.trim().is_empty()) {
        (Some(expand_home(b)), "file")
    } else if let Some(b) = &bundle {
        (Some(b.bin.clone()), "bundle")
    } else {
        (None, "none")
    };

    let network_chosen = file.network.as_deref().and_then(crate::networks::by_id).is_some();
    let node_bin_present = node_bin.as_deref().is_some_and(|p| p.is_file());
    // The wizard is for a genuinely fresh install that has a node program to offer. Everybody else keeps
    // the console they had: packages and scripted installs (env), anyone who ever saved settings, anyone
    // whose state folder already holds block history (a console from before the wizard existed), and any
    // console with no node program beside it (nothing to set up beyond the RPC target, which Settings
    // already handles). A managed console with no network chosen still needs the wizard: there is no
    // default network. The wizard stays reachable from Settings ("Run setup again", "Use a managed node").
    let setup_done = file.setup_done
        || env.setup_skip
        || (mode == Mode::Standalone
            && (env.rpc.is_some()
                || env_mode.is_some()
                || facts.console_json
                || facts.feeds_json
                || !node_bin_present))
        || (mode == Mode::Managed && network_chosen);

    let mut env_locked = Vec::new();
    if env_mode.is_some() {
        env_locked.push("mode");
    }
    if env.node_bin.is_some() {
        env_locked.push("node_bin");
    }
    if env.data_root.is_some() {
        env_locked.push("data_root");
    }
    if env.rpc.is_some() {
        env_locked.push("rpc");
    }
    if env.updates.is_some() {
        env_locked.push("updates");
    }

    Effective {
        mode,
        mode_source,
        node_bin,
        node_bin_source,
        data_root,
        data_root_source,
        bundle,
        setup_done,
        env_locked,
    }
}

#[cfg(test)]
mod console_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("blvm-ui-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_are_standalone_with_no_network() {
        let eff = resolve(&ConsoleSettings::default(), &EnvOverrides::default(), None);
        assert_eq!(eff.mode, Mode::Standalone);
        assert_eq!(eff.mode_source, "default");
        assert!(eff.node_bin.is_none());
        assert!(eff.setup_done, "no node program to offer: nothing to set up beyond the RPC target");
        assert!(ConsoleSettings::default().network.is_none(), "there is no default network");
        assert!(ConsoleSettings::default().stop_node_on_exit);
    }

    #[test]
    fn env_beats_file_beats_bundle() {
        let dir = tmp("prec");
        std::fs::write(dir.join(BUNDLE_MARKER), "version = \"0.1.60\"\n").unwrap();
        std::fs::write(dir.join(NODE_BIN_NAME), b"#!/bin/sh\n").unwrap();

        // Bundle alone: managed, bundle binary, bundle version.
        let eff = resolve(&ConsoleSettings::default(), &EnvOverrides::default(), Some(&dir));
        assert_eq!((eff.mode, eff.mode_source), (Mode::Managed, "bundle"));
        assert_eq!(eff.node_bin.as_deref(), Some(dir.join(NODE_BIN_NAME).as_path()));
        assert_eq!(eff.bundle.as_ref().and_then(|b| b.version.clone()).as_deref(), Some("0.1.60"));

        // File beats bundle.
        let file = ConsoleSettings {
            mode: Some("standalone".into()),
            node_bin: Some("/opt/file/blvm".into()),
            data_root: Some("/data/file".into()),
            ..Default::default()
        };
        let eff = resolve(&file, &EnvOverrides::default(), Some(&dir));
        assert_eq!((eff.mode, eff.mode_source), (Mode::Standalone, "file"));
        assert_eq!(eff.node_bin.as_deref(), Some(Path::new("/opt/file/blvm")));
        assert_eq!(eff.data_root, Path::new("/data/file"));
        assert!(eff.env_locked.is_empty());

        // Env beats file.
        let env = EnvOverrides {
            mode: Some("managed".into()),
            node_bin: Some("/opt/env/blvm".into()),
            data_root: Some("/data/env".into()),
            ..Default::default()
        };
        let eff = resolve(&file, &env, Some(&dir));
        assert_eq!((eff.mode, eff.mode_source), (Mode::Managed, "env"));
        assert_eq!(eff.node_bin.as_deref(), Some(Path::new("/opt/env/blvm")));
        assert_eq!(eff.node_bin_source, "env");
        assert_eq!(eff.data_root, Path::new("/data/env"));
        assert_eq!(eff.env_locked, vec!["mode", "node_bin", "data_root"]);

        // A binary without the marker (a dev tree) never means managed.
        std::fs::remove_file(dir.join(BUNDLE_MARKER)).unwrap();
        let eff = resolve(&ConsoleSettings::default(), &EnvOverrides::default(), Some(&dir));
        assert_eq!(eff.mode, Mode::Standalone);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_garbage_mode_is_ignored_not_fatal() {
        let file = ConsoleSettings { mode: Some("nonsense".into()), ..Default::default() };
        let env = EnvOverrides { mode: Some("also-nonsense".into()), ..Default::default() };
        assert_eq!(resolve(&file, &env, None).mode, Mode::Standalone);
    }

    #[test]
    fn setup_done_rules() {
        // Env RPC or mode skips the wizard for standalone installs.
        let env = EnvOverrides { rpc: Some("127.0.0.1:8332".into()), ..Default::default() };
        assert!(resolve(&ConsoleSettings::default(), &env, None).setup_done);
        let env = EnvOverrides { mode: Some("standalone".into()), ..Default::default() };
        assert!(resolve(&ConsoleSettings::default(), &env, None).setup_done);
        let env = EnvOverrides { setup_skip: true, ..Default::default() };
        assert!(resolve(&ConsoleSettings::default(), &env, None).setup_done);
        // Managed by env but no network chosen yet: still needs the wizard.
        let env = EnvOverrides { mode: Some("managed".into()), ..Default::default() };
        assert!(!resolve(&ConsoleSettings::default(), &env, None).setup_done);
        // ... until a network is chosen.
        let file = ConsoleSettings { network: Some("regtest".into()), ..Default::default() };
        assert!(resolve(&file, &env, None).setup_done);
        let file = ConsoleSettings { network: Some("bogus".into()), ..Default::default() };
        assert!(!resolve(&file, &env, None).setup_done);
        // The wizard's own marker.
        let file = ConsoleSettings { setup_done: true, ..Default::default() };
        assert!(resolve(&file, &EnvOverrides::default(), None).setup_done);
    }

    /// A node program the console could manage: `BLVM_UI_NODE_BIN` pointing at a real file.
    fn with_node_bin(dir: &Path) -> EnvOverrides {
        let bin = dir.join(NODE_BIN_NAME);
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        EnvOverrides { node_bin: Some(bin.display().to_string()), ..Default::default() }
    }

    #[test]
    fn a_genuinely_fresh_install_with_a_node_program_gets_the_wizard() {
        let dir = tmp("b1-fresh");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let env = with_node_bin(&dir);
        let facts = state_dir_facts(Some(&state));
        assert_eq!(facts, StateDirFacts::default());
        let eff = resolve_in(&ConsoleSettings::default(), &env, None, facts);
        assert_eq!(eff.mode, Mode::Standalone);
        assert!(!eff.setup_done, "fresh state dir + a node program: the wizard is wanted");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_existing_standalone_install_never_gets_the_wizard() {
        // B1 (U0/U1 review): a state dir from before the wizard existed holds `feeds.json` and no
        // `console.json`; with no env at all it must not turn into a first-run screen.
        let dir = tmp("b1-pre-u1");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join(crate::persist::FILE), br#"{"version":1,"views":{}}"#).unwrap();
        let facts = state_dir_facts(Some(&state));
        assert!(facts.feeds_json && !facts.console_json);
        let env = with_node_bin(&dir); // even with a node program beside the console
        let eff = resolve_in(&ConsoleSettings::default(), &env, None, facts);
        assert_eq!((eff.mode, eff.mode_source), (Mode::Standalone, "default"));
        assert!(eff.setup_done, "pre-U1 state dir (feeds.json, no console.json, no env) must not get the wizard");

        // Same for a state dir where only console.json exists (settings were saved once).
        let state2 = dir.join("state2");
        std::fs::create_dir_all(&state2).unwrap();
        save_console(&state2, &ConsoleSettings::default()).unwrap();
        let facts2 = state_dir_facts(Some(&state2));
        assert!(facts2.console_json);
        assert!(resolve_in(&ConsoleSettings::default(), &env, None, facts2).setup_done);

        // A console with no state folder at all and no node program: nothing to set up.
        let eff = resolve_in(&ConsoleSettings::default(), &EnvOverrides::default(), None, StateDirFacts::default());
        assert!(eff.setup_done);

        // A managed console with no network still needs the wizard, whatever the state folder holds.
        let menv = EnvOverrides { mode: Some("managed".into()), ..env };
        assert!(!resolve_in(&ConsoleSettings::default(), &menv, None, facts).setup_done);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn persistence_round_trips_and_tolerates_old_and_broken_files() {
        let dir = tmp("persist");
        assert_eq!(load_console(&dir), ConsoleSettings::default(), "no file yet");

        let mut s = ConsoleSettings {
            setup_done: true,
            mode: Some("managed".into()),
            network: Some("regtest".into()),
            data_root: Some("/data/x".into()),
            node_bin: Some("/bin/blvm".into()),
            stop_node_on_exit: false,
            show_logs: true,
            ..Default::default()
        };
        s.rpc_overrides.insert("regtest".into(), "127.0.0.1:19999".into());
        s.child_env.insert("RUST_LOG".into(), "info".into());
        save_console(&dir, &s).unwrap();
        assert_eq!(load_console(&dir), s);
        assert!(!dir.join("console.json.tmp").exists(), "temp file is renamed away");

        // A file with only some fields (older or hand written) still loads, with defaults.
        std::fs::write(dir.join(CONSOLE_FILE), r#"{"setup_done":true,"network":"signet","future_field":1}"#).unwrap();
        let back = load_console(&dir);
        assert!(back.setup_done);
        assert_eq!(back.network.as_deref(), Some("signet"));
        assert!(back.stop_node_on_exit, "missing field takes its default");

        // Garbage falls back to defaults instead of failing.
        std::fs::write(dir.join(CONSOLE_FILE), b"{not json").unwrap();
        assert_eq!(load_console(&dir), ConsoleSettings::default());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn env_alias_and_parse() {
        assert_eq!(Mode::parse(" Managed "), Some(Mode::Managed));
        assert_eq!(Mode::parse("standalone"), Some(Mode::Standalone));
        assert_eq!(Mode::parse(""), None);
    }

    #[test]
    fn a_program_the_installer_put_in_the_data_folder_is_found_again() {
        let dir = tmp("installed-bin");
        let root = dir.join("root");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let env = EnvOverrides { mode: Some("managed".into()), data_root: Some(root.display().to_string()), ..Default::default() };
        let file = ConsoleSettings { node_bin: Some("/opt/file/blvm".into()), ..Default::default() };
        // Nothing installed yet: the file's choice.
        assert_eq!(resolve(&file, &env, None).node_bin_source, "file");
        assert_eq!(resolve(&ConsoleSettings::default(), &env, None).node_bin, None);
        // Installed: found with no variable set, ahead of the file.
        std::fs::write(installed_bin(&root), b"#!/bin/sh\n").unwrap();
        let eff = resolve(&file, &env, None);
        assert_eq!((eff.node_bin_source, eff.node_bin.as_deref()), ("installed", Some(installed_bin(&root).as_path())));
        // The environment still wins.
        let pinned = EnvOverrides { node_bin: Some("/opt/env/blvm".into()), ..env.clone() };
        let eff = resolve(&file, &pinned, None);
        assert_eq!((eff.node_bin_source, eff.node_bin.as_deref()), ("env", Some(Path::new("/opt/env/blvm"))));
        // A dangling link is not a program.
        std::fs::remove_file(installed_bin(&root)).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("blvm-v9", installed_bin(&root)).unwrap();
        assert_eq!(resolve(&ConsoleSettings::default(), &env, None).node_bin, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_updates_variable_means_on_off_or_store() {
        for (v, want) in [("0", "off"), ("off", "off"), ("False", "off"), ("1", "self"), ("self", "self"), ("on", "self"), ("store", "store")] {
            assert_eq!(parse_updates_env(v), Some(want), "{v}");
        }
        assert_eq!(parse_updates_env(""), None);
        assert_eq!(parse_updates_env("maybe"), None);
    }

    #[test]
    fn saving_console_json_is_private_flushed_and_leaves_no_temp_file() {
        let dir = tmp("save");
        let before = crate::fsutil::sync_count();
        let s = ConsoleSettings { node_wanted: Some(false), rpc_user: Some("commons".into()), rpc_pass: Some("x".into()), ..Default::default() };
        save_console(&dir, &s).unwrap();
        assert!(crate::fsutil::sync_count() >= before + 2, "the file and its folder are flushed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(dir.join(CONSOLE_FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(load_console(&dir), s);
        assert!(std::fs::read_dir(&dir).unwrap().flatten().all(|e| e.file_name() == CONSOLE_FILE));
        // The new keys are optional: a file from before them loads, and "never asked" means "wanted".
        std::fs::write(dir.join(CONSOLE_FILE), r#"{"setup_done":true,"network":"signet"}"#).unwrap();
        let old = load_console(&dir);
        assert_eq!((old.node_wanted, old.rpc_user.clone(), old.rpc_pass.clone()), (None, None, None));
        assert!(old.node_wanted_effective());
        assert!(!ConsoleSettings { node_wanted: Some(false), ..Default::default() }.node_wanted_effective());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn random_hex_is_the_right_length_and_not_the_same_twice() {
        let (a, b) = (random_hex(24), random_hex(24));
        assert_eq!((a.len(), b.len()), (48, 48));
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_ne!(a, "0".repeat(48));
    }
}
