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

/// The node's built-in assume-valid height for `net`: blocks below it skip signature checks when the node
/// is started without `--assumevalid` / `--noassumevalid`. Mirrors
/// `blvm-node/src/config/mod.rs::default_assume_valid_height_for_network`; the test
/// `assume_valid_defaults_match_the_node_source` compares the two when the node source sits next to this
/// crate, so the table cannot drift silently. Shown by the wizard and the Validation card.
pub fn assume_valid_default_height(net: &Network) -> u64 {
    match net.id {
        "mainnet" => 912_683,
        "testnet" => 4_550_000,
        "signet" => 267_665,
        // Testnet4 and regtest: 0, i.e. full verification.
        _ => 0,
    }
}

/// The fewest blocks a pruned node may keep. BIP159 (`NODE_NETWORK_LIMITED`) promises the last 288 blocks to
/// peers, and a reorg can never be deeper than what is kept. Enforced by the settings API, not by the struct.
pub const PRUNE_MIN_BLOCKS: u64 = 288;
/// The largest value the API accepts (far above any chain; it only bounds typos).
pub const PRUNE_MAX_BLOCKS: u64 = 100_000_000;

pub fn by_id(id: &str) -> Option<&'static Network> {
    ALL.iter().find(|n| n.id == id)
}

/// The environment variable that moves the node's P2P port for `net` (the Umbrel app sets it from the store's
/// compose file, because Umbrel's own Bitcoin Node holds 8333): `BLVM_AGENT_P2P_PORT_MAINNET`, `..._TESTNET4`,
/// `..._TESTNET` (Testnet3), `..._SIGNET`, `..._REGTEST`.
pub fn p2p_env_name(net: &Network) -> String {
    format!("BLVM_AGENT_P2P_PORT_{}", net.id.to_ascii_uppercase())
}

/// The P2P port overrides found through `get` (a lookup of environment variables), by network id. A value that
/// is not a port (empty, text, 0, above 65535) is ignored.
pub fn p2p_overrides_with(get: impl Fn(&str) -> Option<String>) -> std::collections::BTreeMap<String, u16> {
    ALL.iter()
        .filter_map(|n| {
            let port = get(&p2p_env_name(n))?.trim().parse::<u16>().ok().filter(|p| *p != 0)?;
            Some((n.id.to_string(), port))
        })
        .collect()
}

/// [`p2p_overrides_with`] on this process's environment.
pub fn p2p_overrides() -> std::collections::BTreeMap<String, u16> {
    p2p_overrides_with(|k| std::env::var(k).ok())
}

/// The P2P port a node of the Umbrel app listens on: the override from the environment, else the network's own.
/// For display (the agent gets the same variables); the desktop console does not move the port.
pub fn effective_p2p_port(net: &Network) -> u16 {
    p2p_overrides().get(net.id).copied().unwrap_or(net.p2p_port)
}

/// Serialises the tests that set `BLVM_AGENT_P2P_PORT_*` and the ones that read the ports from the environment.
#[cfg(test)]
pub(crate) static P2P_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    /// `true` once a node was started on this folder with block pruning switched on. Set when the node is
    /// spawned, and cleared only by deleting the chain data (a wipe): going back to a full node needs that
    /// deletion only when the node ever ran pruned here. `None`/`false`: it never did.
    pub ran_pruned: Option<bool>,
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

    /// Parse the literals of the node's `default_assume_valid_height_for_network` and compare. The node source
    /// is a sibling folder in the workspace; in a checkout without it (the org repo, the Docker build) the
    /// check is skipped.
    #[test]
    fn assume_valid_defaults_match_the_node_source() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../blvm-node/src/config/mod.rs");
        let Ok(src) = std::fs::read_to_string(&path) else {
            eprintln!("skipped: {} not found", path.display());
            return;
        };
        let start = src
            .find("pub fn default_assume_valid_height_for_network")
            .expect("the node function is still there (renamed? update networks::assume_valid_default_height)");
        let body = &src[start..];
        let body = &body[..body.find("\n}\n").expect("end of the node function")];
        // Arms look like `"mainnet" | "bitcoinv1" => 912_683,  // comment` and `_ => 0,`.
        let mut arms: Vec<(Vec<String>, u64)> = Vec::new();
        for line in body.lines().map(str::trim) {
            let Some((lhs, rhs)) = line.split_once("=>") else { continue };
            let num: String = rhs.trim().chars().take_while(|c| c.is_ascii_digit() || *c == '_').filter(|c| *c != '_').collect();
            let Ok(h) = num.parse::<u64>() else { continue };
            let names: Vec<String> = lhs
                .split('|')
                .map(|n| n.trim().trim_matches('"').to_string())
                .collect();
            arms.push((names, h));
        }
        assert!(arms.len() >= 5, "parsed {arms:?}");
        for n in ALL.iter() {
            let want = arms
                .iter()
                .find(|(names, _)| names.iter().any(|x| x == n.cli))
                .or_else(|| arms.iter().find(|(names, _)| names.len() == 1 && names[0] == "_"))
                .map(|(_, h)| *h)
                .unwrap();
            assert_eq!(assume_valid_default_height(n), want, "{}: console table vs the node source", n.id);
            // The console states full validation for the networks whose node default is 0.
            assert_eq!(n.full_validation, want == 0, "{}: full_validation vs the node default", n.id);
        }
    }

    #[test]
    fn p2p_overrides_read_the_five_variables_and_ignore_garbage() {
        use std::collections::HashMap;
        let env: HashMap<&str, &str> = [
            ("BLVM_AGENT_P2P_PORT_MAINNET", "8334"),
            ("BLVM_AGENT_P2P_PORT_TESTNET4", " 48334 "),
            ("BLVM_AGENT_P2P_PORT_TESTNET", "18334"),
            ("BLVM_AGENT_P2P_PORT_SIGNET", "not-a-port"),
            ("BLVM_AGENT_P2P_PORT_REGTEST", "70000"),
        ]
        .into();
        let m = p2p_overrides_with(|k| env.get(k).map(|v| v.to_string()));
        assert_eq!(m.get("mainnet"), Some(&8334));
        assert_eq!(m.get("testnet4"), Some(&48334));
        assert_eq!(m.get("testnet"), Some(&18334), "the id of Testnet3 is `testnet`");
        assert_eq!(m.len(), 3, "garbage and out-of-range values are ignored: {m:?}");
        for bad in ["", "0", "-1", "80.5", "0x1f90"] {
            let m = p2p_overrides_with(|k| (k == "BLVM_AGENT_P2P_PORT_MAINNET").then(|| bad.to_string()));
            assert!(m.is_empty(), "{bad:?} -> {m:?}");
        }
        assert_eq!(p2p_env_name(by_id("testnet").unwrap()), "BLVM_AGENT_P2P_PORT_TESTNET");
        assert!(p2p_overrides_with(|_| None).is_empty());
    }

    #[test]
    fn the_effective_p2p_port_follows_the_environment() {
        let _g = P2P_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let main = by_id("mainnet").unwrap();
        std::env::remove_var("BLVM_AGENT_P2P_PORT_MAINNET");
        assert_eq!(effective_p2p_port(main), 8333);
        std::env::set_var("BLVM_AGENT_P2P_PORT_MAINNET", "8334");
        assert_eq!(effective_p2p_port(main), 8334);
        assert_eq!(effective_p2p_port(by_id("signet").unwrap()), 38333, "other networks keep theirs");
        std::env::set_var("BLVM_AGENT_P2P_PORT_MAINNET", "garbage");
        assert_eq!(effective_p2p_port(main), 8333);
        std::env::remove_var("BLVM_AGENT_P2P_PORT_MAINNET");
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
        let m = Meta { created: Some(1), last_used: Some(2), last_height: Some(3), last_header: Some(4), ran_pruned: Some(true) };
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
