//! The control files the console and the node agent share under `<root>/control/` (managed-node design B).
//!
//! * `desired.json` is written by the console (the only writer): what should be running.
//! * `state.json` is written by the agent (the only writer): what is running, refreshed on change and every
//!   two seconds as a heartbeat.
//!
//! Both are replaced whole (temp file + rename), so a reader never sees half a file. The same code serves the
//! in-process agent (desktop: no files at all) and the agent in the node container (Umbrel: both containers mount
//! the same `/data`).

use crate::agent::ProcState;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DESIRED_FILE: &str = "desired.json";
pub const STATE_FILE: &str = "state.json";
/// Default for [`Desired::binary`]: the program the installer keeps current (a symlink to `bin/blvm-<tag>`).
pub const DEFAULT_BINARY: &str = "bin/blvm";
/// A heartbeat older than this many seconds means the agent is not running.
pub const HEARTBEAT_STALE_SECS: u64 = 10;

pub fn dir(root: &Path) -> PathBuf {
    root.join("control")
}

/// What the console wants. `generation` goes up by one with every write; the agent acts when it sees a
/// generation it has not applied and reports it back in [`StateFile::generation`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Desired {
    pub generation: u64,
    /// Console network id (`mainnet`, `testnet4`, ...). Also read by the Umbrel `exports.sh`.
    pub network: Option<String>,
    pub running: bool,
    /// Relative to the data root (`bin/blvm`), or absolute.
    pub binary: String,
    /// Extra environment for the node; an empty value unsets the variable.
    pub env: BTreeMap<String, String>,
    pub assume_valid: Option<String>,
    /// SIGTERM to SIGKILL, in seconds.
    pub stop_grace_secs: u64,
    /// Port for `--rpc-addr` when not the network's default. The agent supplies the bind address.
    pub rpc_port: Option<u16>,
    /// Goes up when the console wants a restart of a node that is already running.
    pub restart_id: u64,
    /// Unix seconds of the write (informational).
    pub updated: u64,
}

impl Default for Desired {
    fn default() -> Self {
        Self {
            generation: 0,
            network: None,
            running: false,
            binary: DEFAULT_BINARY.into(),
            env: BTreeMap::new(),
            assume_valid: None,
            stop_grace_secs: 200,
            rpc_port: None,
            restart_id: 0,
            updated: 0,
        }
    }
}

/// What the agent reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StateFile {
    pub agent_version: String,
    /// Unix seconds of the write.
    pub heartbeat: u64,
    /// The last [`Desired::generation`] the agent has dealt with.
    pub generation: u64,
    /// `stopped | starting | running | stopping | crashed`.
    pub state: String,
    pub pid: Option<u32>,
    /// Unix seconds the current state began.
    pub since: Option<u64>,
    pub network: Option<String>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub restarts: u32,
    pub last_error: Option<String>,
    /// Another program holds the node's RPC or P2P port.
    pub conflict: bool,
    /// The node was already running when the agent started and was taken over.
    pub adopted: bool,
    /// Last 50 log lines of a crashed node.
    pub crash_log: Vec<String>,
    /// `X.Y.Z` from `<binary> version`.
    pub installed_version: Option<String>,
    /// `blvm X.Y.Z`.
    pub installed_report: Option<String>,
    /// The program the agent runs (absolute), and whether it exists. The console cannot see a binary that
    /// only exists inside the node container.
    pub binary: Option<String>,
    pub binary_found: bool,
    /// The [`Desired::restart_id`] the running node was started under.
    pub restart_id: u64,
}

impl Default for StateFile {
    fn default() -> Self {
        Self {
            agent_version: env!("CARGO_PKG_VERSION").into(),
            heartbeat: 0,
            generation: 0,
            state: "stopped".into(),
            pid: None,
            since: None,
            network: None,
            exit_code: None,
            exit_signal: None,
            restarts: 0,
            last_error: None,
            conflict: false,
            adopted: false,
            crash_log: Vec::new(),
            installed_version: None,
            installed_report: None,
            binary: None,
            binary_found: false,
            restart_id: 0,
        }
    }
}

impl StateFile {
    pub fn proc_state(&self) -> ProcState {
        ProcState::parse(&self.state).unwrap_or(ProcState::Stopped)
    }

    /// The agent wrote this recently.
    pub fn alive(&self, now: u64) -> bool {
        self.heartbeat > 0 && now.saturating_sub(self.heartbeat) <= HEARTBEAT_STALE_SECS
    }
}

pub fn read_desired(root: &Path) -> Option<Desired> {
    serde_json::from_slice(&std::fs::read(dir(root).join(DESIRED_FILE)).ok()?).ok()
}

pub fn read_state(root: &Path) -> Option<StateFile> {
    serde_json::from_slice(&std::fs::read(dir(root).join(STATE_FILE)).ok()?).ok()
}

/// Replace `path` atomically with the JSON of `value`: temp file (named so that no other writer, in this or
/// another container, shares it), flushed to disk, renamed, folder flushed.
pub fn write_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let body = serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?;
    crate::fsutil::write_atomic_bytes(path, &body, None)
}

pub fn write_desired(root: &Path, d: &Desired) -> std::io::Result<()> {
    write_atomic(&dir(root).join(DESIRED_FILE), d)
}

pub fn write_state(root: &Path, s: &StateFile) -> std::io::Result<()> {
    write_atomic(&dir(root).join(STATE_FILE), s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("blvm-control-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn both_files_round_trip_and_tolerate_old_and_broken_content() {
        let root = tmp("rt");
        assert_eq!(read_desired(&root), None);
        assert_eq!(read_state(&root), None);
        let d = Desired {
            generation: 3,
            network: Some("signet".into()),
            running: true,
            binary: "bin/blvm".into(),
            assume_valid: Some("off".into()),
            ..Default::default()
        };
        write_desired(&root, &d).unwrap();
        assert_eq!(read_desired(&root), Some(d));
        let s = StateFile { generation: 3, state: "running".into(), pid: Some(7), heartbeat: 100, ..Default::default() };
        write_state(&root, &s).unwrap();
        assert_eq!(read_state(&root), Some(s));
        assert!(
            std::fs::read_dir(dir(&root)).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains("tmp")),
            "temp files are renamed away"
        );
        // Unknown and missing keys are fine.
        std::fs::write(dir(&root).join(DESIRED_FILE), r#"{"generation":9,"running":true,"future":1}"#).unwrap();
        let d = read_desired(&root).unwrap();
        assert_eq!((d.generation, d.running, d.binary.as_str(), d.stop_grace_secs), (9, true, "bin/blvm", 200));
        // Garbage reads as nothing.
        std::fs::write(dir(&root).join(STATE_FILE), b"{oops").unwrap();
        assert_eq!(read_state(&root), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_heartbeat_goes_stale() {
        let s = StateFile { heartbeat: 1000, ..Default::default() };
        assert!(s.alive(1000) && s.alive(1010));
        assert!(!s.alive(1011));
        assert!(!StateFile::default().alive(5), "no heartbeat at all is not alive");
    }
}
