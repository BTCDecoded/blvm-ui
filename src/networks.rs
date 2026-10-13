//! The one table of networks the managed node can run (design C).
//!
//! `id` is the console's name for a network; it is also the folder name under
//! `<data_root>/networks/`. `cli` is the value for the node's `--network`, `chain` is the name the
//! node reports in `getblockchaininfo`. Mirrored by `web/src/lib/networks.ts`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Network {
    pub id: &'static str,
    pub cli: &'static str,
    pub chain: &'static str,
    pub label: &'static str,
    pub rpc_port: u16,
    pub p2p_port: u16,
    /// The node's own default for this network skips script checks for old blocks. Testnet4 and
    /// regtest default to full validation inside the node, but the console states it anyway
    /// (`--noassumevalid`), so a node started here never depends on a default that can change.
    pub full_validation: bool,
    /// Rough disk use of a full sync, when the design has a figure (`None`: not measured yet). Only
    /// used to warn before a switch; nothing is ever refused because of it except a nearly full disk.
    pub estimate_bytes: Option<u64>,
}

const GB: u64 = 1_000_000_000;

pub const ALL: [Network; 5] = [
    Network { id: "mainnet", cli: "mainnet", chain: "main", label: "Mainnet", rpc_port: 8332, p2p_port: 8333, full_validation: false, estimate_bytes: Some(800 * GB) },
    Network { id: "testnet4", cli: "testnet4", chain: "testnet4", label: "Testnet4", rpc_port: 48332, p2p_port: 48333, full_validation: true, estimate_bytes: None },
    Network { id: "testnet", cli: "testnet", chain: "test", label: "Testnet3", rpc_port: 18332, p2p_port: 18333, full_validation: false, estimate_bytes: Some(100 * GB) },
    Network { id: "signet", cli: "signet", chain: "signet", label: "Signet", rpc_port: 38332, p2p_port: 38333, full_validation: false, estimate_bytes: None },
    Network { id: "regtest", cli: "regtest", chain: "regtest", label: "Regtest", rpc_port: 18443, p2p_port: 18444, full_validation: true, estimate_bytes: Some(GB) },
];

pub fn by_id(id: &str) -> Option<&'static Network> {
    ALL.iter().find(|n| n.id == id)
}

/// Default RPC address of a network on this machine.
pub fn default_rpc(net: &Network) -> String {
    format!("127.0.0.1:{}", net.rpc_port)
}

/// `<root>/networks/<id>`: the node's working directory and everything it writes.
pub fn dir(root: &Path, net: &Network) -> PathBuf {
    root.join("networks").join(net.id)
}

pub fn meta_file(root: &Path, net: &Network) -> PathBuf {
    dir(root, net).join("meta.json")
}

pub fn log_file(root: &Path, net: &Network) -> PathBuf {
    dir(root, net).join("logs").join("node.log")
}

/// `networks/<id>/meta.json`: what the switch dialog says about a network without asking the node.
/// Every field is optional so older and newer files both load.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    /// Unix seconds the folder was first used by this console.
    pub created: Option<u64>,
    /// Unix seconds the node on this network was last running (or stopped).
    pub last_used: Option<u64>,
    /// Height from the last successful RPC poll.
    pub last_height: Option<u64>,
    /// Header height from that poll, when known.
    pub last_header: Option<u64>,
}

pub fn read_meta(root: &Path, net: &Network) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(meta_file(root, net)).ok()?).ok()
}

/// Atomic (temp file + rename). The network folder must already exist: this never creates one.
pub fn write_meta(root: &Path, net: &Network, meta: &Meta) -> std::io::Result<()> {
    let path = meta_file(root, net);
    let body = serde_json::to_vec_pretty(meta).map_err(std::io::Error::other)?;
    crate::fsutil::write_atomic_bytes(&path, &body, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_consistent_with_the_node() {
        assert_eq!(ALL.len(), 5);
        assert_eq!(by_id("testnet4").unwrap().rpc_port, 48332);
        assert_eq!(by_id("regtest").unwrap().p2p_port, 18444);
        assert_eq!(by_id("mainnet").unwrap().chain, "main");
        assert_eq!(by_id("testnet").unwrap().chain, "test");
        assert!(by_id("main").is_none(), "ids are console names, not RPC chain names");
        assert!(by_id("").is_none());
        let mut ports: Vec<u16> = ALL.iter().flat_map(|n| [n.rpc_port, n.p2p_port]).collect();
        ports.sort_unstable();
        ports.dedup();
        assert_eq!(ports.len(), 10, "no two networks share a port");
    }

    #[test]
    fn paths_follow_the_layout() {
        let root = Path::new("/data");
        let n = by_id("signet").unwrap();
        assert_eq!(dir(root, n), Path::new("/data/networks/signet"));
        assert_eq!(log_file(root, n), Path::new("/data/networks/signet/logs/node.log"));
        assert_eq!(default_rpc(n), "127.0.0.1:38332");
        assert_eq!(meta_file(root, n), Path::new("/data/networks/signet/meta.json"));
    }

    #[test]
    fn meta_round_trips_tolerates_old_files_and_never_creates_folders() {
        let root = std::env::temp_dir().join(format!("blvm-networks-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let n = by_id("signet").unwrap();
        let m = Meta { created: Some(1), last_used: Some(2), last_height: Some(3), last_header: Some(4) };
        assert!(write_meta(&root, n, &m).is_err(), "no folder, no file");
        assert!(!root.exists());
        assert_eq!(read_meta(&root, n), None);
        std::fs::create_dir_all(dir(&root, n)).unwrap();
        write_meta(&root, n, &m).unwrap();
        assert_eq!(read_meta(&root, n), Some(m));
        assert!(!dir(&root, n).join("meta.json.tmp").exists());
        std::fs::write(meta_file(&root, n), r#"{"last_height":7,"future":true}"#).unwrap();
        let back = read_meta(&root, n).unwrap();
        assert_eq!((back.last_height, back.created), (Some(7), None));
        std::fs::write(meta_file(&root, n), b"{broken").unwrap();
        assert_eq!(read_meta(&root, n), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
