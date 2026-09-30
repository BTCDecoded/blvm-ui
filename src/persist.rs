//! Keeps the Latest Blocks history across page reloads, console restarts, rebuilds,
//! and network switches. One small JSON file, one entry per node RPC address.
//!
//! Location: `BLVM_UI_STATE_DIR` (runtime, then build time), else the per-user state
//! folder for the build target:
//! - Linux / Docker: `$XDG_STATE_HOME/blvm-ui-next` or `~/.local/state/blvm-ui-next`
//! - macOS: `~/Library/Application Support/blvm-ui-next`
//! - Windows: `%APPDATA%\blvm-ui-next`
//!
//! For Umbrel / Start9, point `BLVM_UI_STATE_DIR` at the app's persistent volume.
//! History is only dropped when the node's chain really goes backwards (a wiped
//! data dir) or the node reports a different chain; see `state.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::feed::FeedItem;
use crate::settings::setting;

pub const FILE: &str = "feeds.json";
const VERSION: u32 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StoredView {
    /// Normalized chain name (`main`, `test`, `testnet4`, `signet`, `regtest`).
    pub chain: Option<String>,
    /// Last height already represented in the feed.
    pub last: Option<u64>,
    /// Newest first, like the live feed.
    pub items: Vec<StoredItem>,
    /// Node status from the last answer it gave (shown while it is frozen).
    #[serde(default)]
    pub last_known: Option<LastKnown>,
}

/// The node's status the last time it answered: sync %, heights, chain, version.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastKnown {
    /// Unix seconds of the last successful poll.
    pub at: u64,
    pub local_height: u64,
    pub network_height: u64,
    pub behind: u64,
    pub sync_pct: String,
    pub sync_pct_num: String,
    pub ibd: bool,
    pub network: String,
    pub blvm_version: String,
}

impl LastKnown {
    /// Same status, ignoring when it was seen.
    pub fn same_status(&self, other: &LastKnown) -> bool {
        LastKnown { at: 0, ..self.clone() } == LastKnown { at: 0, ..other.clone() }
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredItem {
    pub kind: String,
    pub start: u64,
    pub end: u64,
}

impl StoredItem {
    pub fn from_item(i: &FeedItem) -> Self {
        Self { kind: i.kind.to_string(), start: i.start, end: i.end }
    }

    pub fn to_item(&self) -> FeedItem {
        if self.kind == "chunk" {
            FeedItem::chunk(self.start, self.end)
        } else {
            FeedItem::block(self.end)
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    views: BTreeMap<String, StoredView>,
}

pub fn state_dir() -> Option<PathBuf> {
    if let Some(p) = setting("BLVM_UI_STATE_DIR", option_env!("BLVM_UI_STATE_DIR")) {
        return Some(PathBuf::from(p));
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    #[cfg(target_os = "macos")]
    let dir = home().map(|h| h.join("Library/Application Support/blvm-ui-next"));
    #[cfg(windows)]
    let dir = std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("blvm-ui-next"));
    #[cfg(not(any(target_os = "macos", windows)))]
    let dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".local/state")))
        .map(|d| d.join("blvm-ui-next"));
    dir.or_else(|| {
        std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("state")))
    })
}

fn file_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join(FILE))
}

/// Saved views, or an empty map (no file yet, unreadable, or a newer format).
pub fn load() -> BTreeMap<String, StoredView> {
    let Some(path) = file_path() else {
        return BTreeMap::new();
    };
    match std::fs::read(&path) {
        Ok(bytes) => match serde_json::from_slice::<Stored>(&bytes) {
            Ok(s) if s.version == VERSION => {
                tracing::info!("block history: loaded {} view(s) from {}", s.views.len(), path.display());
                s.views
            }
            Ok(s) => {
                tracing::warn!("block history: {} has version {}, ignoring", path.display(), s.version);
                BTreeMap::new()
            }
            Err(e) => {
                tracing::warn!("block history: cannot read {}: {e}", path.display());
                BTreeMap::new()
            }
        },
        Err(_) => {
            tracing::info!("block history: starting fresh at {}", path.display());
            BTreeMap::new()
        }
    }
}

/// Atomic write (temp file + rename) so a crash never leaves half a file.
pub fn save(views: &BTreeMap<String, StoredView>) -> std::io::Result<()> {
    let Some(path) = file_path() else {
        return Ok(());
    };
    save_to(&path, views)
}

fn save_to(path: &std::path::Path, views: &BTreeMap<String, StoredView>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&Stored { version: VERSION, views: views.clone() })
        .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_round_trip() {
        for item in [FeedItem::block(42), FeedItem::chunk(100, 250)] {
            assert_eq!(StoredItem::from_item(&item).to_item(), item);
        }
    }

    #[test]
    fn older_files_without_last_known_still_load() {
        let old = r#"{"version":1,"views":{"127.0.0.1:18332":{"chain":"test","last":7,"items":[]}}}"#;
        let s: Stored = serde_json::from_str(old).unwrap();
        assert_eq!(s.views["127.0.0.1:18332"].last_known, None);
    }

    #[test]
    fn save_then_load_file() {
        let dir = std::env::temp_dir().join(format!("blvm-ui-next-test-{}", std::process::id()));
        let path = dir.join(FILE);
        let mut views = BTreeMap::new();
        views.insert(
            "127.0.0.1:18332".to_string(),
            StoredView {
                chain: Some("test".into()),
                last: Some(250),
                items: vec![StoredItem::from_item(&FeedItem::chunk(100, 250))],
                last_known: Some(LastKnown {
                    at: 1_790_000_000,
                    local_height: 250,
                    network_height: 5_000,
                    behind: 4_750,
                    sync_pct: "5.00%".into(),
                    sync_pct_num: "5.00".into(),
                    ibd: true,
                    network: "test".into(),
                    blvm_version: "v0.1.13".into(),
                }),
            },
        );
        save_to(&path, &views).unwrap();
        let back: Stored = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(back.version, VERSION);
        assert_eq!(back.views, views);
        let _ = std::fs::remove_dir_all(dir);
    }
}
