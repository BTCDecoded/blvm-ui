use crate::feed::{advance_feed, FeedItem};
use crate::node_ctl::{self, LaunchSpec, PowerPhase};
use crate::rpc;
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

pub const HIST_BARS: usize = 12;
/// Auto-poll this many times before the page shows a manual RPC field.
pub const MANUAL_AFTER: u32 = 5;

#[derive(Debug, Clone, Serialize)]
pub struct PeerRow {
    pub addr: String,
    pub inbound: bool,
    pub subver: String,
    /// Rough location from the local IP database (`None` when unknown).
    pub geo: Option<crate::geo::PeerGeo>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BanRow {
    pub address: String,
    pub banned_until: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub connected: bool,
    pub rpc_addr: String,
    pub health: &'static str,
    pub health_label: String,
    pub node_status: String,
    pub uptime: String,
    pub blvm_version: String,
    pub network: String,
    pub sync_pct: String,
    pub sync_pct_num: String,
    pub syncing: bool,
    pub behind: u64,
    pub local_height: u64,
    pub network_height: u64,
    pub ibd: bool,
    pub ibd_label: String,
    pub peers: usize,
    pub inbound: usize,
    pub outbound: usize,
    pub accepting_inbound: bool,
    pub network_active: bool,
    pub peer_rows: Vec<PeerRow>,
    /// Rough location of this node (IP lookup, or the machine's time zone).
    pub self_geo: Option<crate::geo::PeerGeo>,
    pub banned: Vec<BanRow>,
    pub disk_used_num: String,
    pub disk_used_unit: String,
    pub disk_used_label: String,
    pub disk_used_bytes: u64,
    pub disk_free_num: String,
    pub disk_free_unit: String,
    pub disk_free_label: String,
    pub disk_free_bytes: u64,
    pub disk_total_bytes: u64,
    pub disk_vol_used_label: String,
    pub feed: Vec<FeedItem>,
    pub feed_waiting: bool,
    pub arrival: Vec<u64>,
    pub last_check: String,
    pub ui_version: String,
    pub ui_uptime: String,
    pub rpc_failures: u32,
    pub show_manual: bool,
    pub error: Option<String>,
    pub frozen: bool,
    pub node_running: bool,
    pub node_busy: bool,
    pub node_power_label: String,
    /// Unix seconds of the node's last answer (so the page can say "as of …").
    pub last_seen: Option<u64>,

    // ---- Managed-node additions (design F, M). All additive. ----
    /// `"standalone"` or `"managed"`.
    pub mode: &'static str,
    /// `false` while the first-run wizard is wanted.
    pub setup_done: bool,
    /// The node agent's view; `None` in standalone mode.
    pub managed: Option<crate::managed::ManagedView>,
    /// Page ids that installed modules add to the sidebar.
    pub modules_pages: Vec<String>,
    /// Last version the node itself reported, while `blvm_version` is `"—"` (design M.1).
    pub blvm_version_seen: Option<VersionSeen>,
    /// A node binary is known to this console, so "run a node for me" is possible.
    pub bundle_available: bool,
}

/// What the node last said its version was, and when (unix seconds). Shown only as "last seen".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct VersionSeen {
    pub version: String,
    pub at: u64,
}

impl Snapshot {
    pub fn disconnected(rpc_addr: &str) -> Self {
        let vol = disk_volume();
        Self {
            connected: false,
            rpc_addr: rpc_addr.to_string(),
            health: "dead",
            health_label: "Node down".into(),
            node_status: "Down".into(),
            uptime: "—".into(),
            blvm_version: "—".into(),
            network: "—".into(),
            sync_pct: "—".into(),
            sync_pct_num: "—".into(),
            syncing: false,
            behind: 0,
            local_height: 0,
            network_height: 0,
            ibd: false,
            ibd_label: "idle".into(),
            peers: 0,
            inbound: 0,
            outbound: 0,
            accepting_inbound: false,
            network_active: false,
            peer_rows: Vec::new(),
            self_geo: None,
            banned: Vec::new(),
            disk_used_num: "—".into(),
            disk_used_unit: "".into(),
            disk_used_label: "—".into(),
            disk_used_bytes: 0,
            disk_free_num: vol.free_num,
            disk_free_unit: vol.free_unit,
            disk_free_label: vol.free_label,
            disk_free_bytes: vol.free_bytes,
            disk_total_bytes: vol.total_bytes,
            disk_vol_used_label: vol.used_label,
            feed: Vec::new(),
            feed_waiting: true,
            arrival: vec![0; HIST_BARS],
            last_check: "just now".into(),
            ui_version: format!("v{}", env!("CARGO_PKG_VERSION")),
            ui_uptime: "—".into(),
            rpc_failures: 0,
            show_manual: false,
            error: None,
            frozen: false,
            node_running: false,
            node_busy: false,
            node_power_label: "Turn Node On".into(),
            last_seen: None,
            mode: "standalone",
            setup_done: true,
            managed: None,
            modules_pages: vec!["mining".to_string()],
            blvm_version_seen: None,
            bundle_available: false,
        }
    }
}

struct ChainFeed {
    last: Option<u64>,
    feed: VecDeque<FeedItem>,
    arrival: VecDeque<u64>,
    bucket_started: Instant,
    bucket_delta: u64,
    connected_since: Option<Instant>,
    ever_synced: bool,
    last_snapshot: Option<Snapshot>,
    /// Last `getblockhash` hit; next poll densifies just above this.
    hashed_lo: u64,
    /// Lowest confirmed `getblockhash` miss, if any.
    hashed_hi: Option<u64>,
    /// Normalized chain name this feed belongs to (saved with it).
    chain: Option<String>,
    /// Consecutive polls where the height sat well below `last` (see `confirm_feed_height`).
    drop_seen: u32,
    /// Feed or last known status changed since the last save.
    dirty: bool,
    /// Node status from its last answer, saved so a frozen node still shows it.
    last_known: Option<crate::persist::LastKnown>,
    /// Version the node itself reported on its last full poll: `(version, unix seconds)`.
    version_seen: Option<(String, u64)>,
}

impl ChainFeed {
    fn new() -> Self {
        Self {
            chain: None,
            drop_seen: 0,
            dirty: false,
            last_known: None,
            version_seen: None,
            last: None,
            feed: VecDeque::new(),
            arrival: VecDeque::from(vec![0; HIST_BARS]),
            bucket_started: Instant::now(),
            bucket_delta: 0,
            connected_since: None,
            ever_synced: false,
            last_snapshot: None,
            hashed_lo: 0,
            hashed_hi: None,
        }
    }
}

pub struct LiveState {
    pub rpc_addr: String,
    pub started: Instant,
    pub fail_streak: u32,
    pub was_connected: bool,
    /// 1s scheduler: full RPC batch when `n % 4 == 1`.
    poll_n: u64,
    views: HashMap<String, ChainFeed>,
    pub snapshot: Snapshot,
    pub power_phase: PowerPhase,
    pub launch: Option<LaunchSpec>,
    pub power_lock: Arc<Mutex<()>>,
    /// Mode, persisted settings and the node agent (managed mode).
    pub console: crate::managed::Console,
}

impl LiveState {
    pub fn new(rpc_addr: String) -> Self {
        let snapshot = Snapshot::disconnected(&rpc_addr);
        Self {
            rpc_addr,
            started: Instant::now(),
            fail_streak: 0,
            was_connected: false,
            poll_n: 0,
            views: HashMap::new(),
            snapshot,
            power_phase: PowerPhase::Idle,
            launch: None,
            power_lock: Arc::new(Mutex::new(())),
            console: crate::managed::Console::inert(),
        }
    }

    /// Bring back saved Latest Blocks history (see `persist`).
    pub fn restore(&mut self, saved: std::collections::BTreeMap<String, crate::persist::StoredView>) {
        for (rpc, v) in saved {
            let mut c = ChainFeed::new();
            c.chain = v.chain;
            c.last = v.last;
            c.version_seen = v
                .last_known
                .as_ref()
                .filter(|lk| !lk.blvm_version.is_empty() && lk.blvm_version != "—")
                .map(|lk| (lk.blvm_version.clone(), lk.at));
            c.last_known = v.last_known;
            c.feed = v.items.iter().map(crate::persist::StoredItem::to_item).collect();
            self.views.insert(rpc, c);
        }
    }

    /// Every view's history if any changed since the last call; clears the dirty flags.
    pub fn take_dirty_feeds(&mut self) -> Option<std::collections::BTreeMap<String, crate::persist::StoredView>> {
        if !self.views.values().any(|c| c.dirty) {
            return None;
        }
        Some(self.all_feeds())
    }

    /// Every view's history (for the final save on shutdown); clears the dirty flags.
    pub fn all_feeds(&mut self) -> std::collections::BTreeMap<String, crate::persist::StoredView> {
        self.views
            .iter_mut()
            .map(|(rpc, c)| {
                c.dirty = false;
                (
                    rpc.clone(),
                    crate::persist::StoredView {
                        chain: c.chain.clone(),
                        last: c.last,
                        items: c.feed.iter().map(crate::persist::StoredItem::from_item).collect(),
                        last_known: c.last_known.clone(),
                    },
                )
            })
            .collect()
    }

    pub fn switch_rpc(&mut self, addr: String) {
        self.rpc_addr = addr.clone();
        self.fail_streak = 0;
        self.was_connected = false;
        self.poll_n = 0;
        self.snapshot = self
            .views
            .get(&addr)
            .and_then(|c| c.last_snapshot.clone())
            .unwrap_or_else(|| Snapshot::disconnected(&addr));
        self.snapshot.rpc_addr = addr;
        // The parked snapshot is a memory, not a live answer: no version until this view is polled.
        if self.snapshot.blvm_version != "—" {
            let carried = (self.snapshot.blvm_version.clone(), self.snapshot.last_seen.unwrap_or_else(crate::persist::unix_now));
            let seen = chain_mut(self).version_seen.clone().unwrap_or(carried);
            self.snapshot.blvm_version = "—".into();
            self.snapshot.blvm_version_seen = Some(VersionSeen { version: seen.0, at: seen.1 });
        }
        stamp_power(self);
    }
}

pub fn stamp_power(g: &mut LiveState) {
    let running = g.snapshot.node_running || g.snapshot.connected || g.snapshot.frozen;
    g.snapshot.node_running = running;
    g.snapshot.node_busy = g.power_phase != PowerPhase::Idle;
    g.snapshot.node_power_label = match g.power_phase {
        PowerPhase::Stopping => "Stopping…".into(),
        PowerPhase::Starting => "Starting…".into(),
        PowerPhase::Idle if running => "Turn Node Off".into(),
        PowerPhase::Idle => "Turn Node On".into(),
    };
    crate::managed::stamp(g);
}

fn chain_mut(g: &mut LiveState) -> &mut ChainFeed {
    let rpc = g.rpc_addr.clone();
    g.views.entry(rpc).or_insert_with(ChainFeed::new)
}

pub type Shared = std::sync::Arc<RwLock<LiveState>>;

/// Full probe batch every 4th 1s tick (n = 1, 5, 9, …).
pub fn is_full_poll(n: u64) -> bool {
    n % 4 == 1
}

/// 1s scheduler: cheap height poll most ticks, full batch every 4th.
pub async fn poll_tick(state: &Shared) {
    let full = {
        let mut g = state.write().await;
        g.poll_n = g.poll_n.wrapping_add(1);
        is_full_poll(g.poll_n)
    };
    poll_inner(state, full).await;
    crate::managed::housekeeping(state).await;
}

pub async fn poll_once(state: &Shared) {
    poll_inner(state, true).await;
}

/// The node answered the batch but `getblockchaininfo` inside it did not (the RPC server is up before the
/// chain is: during start-up, or while the state is replayed). The poll fails, the node is not "down".
#[derive(Debug)]
struct ChainInfoMissing;

impl std::fmt::Display for ChainInfoMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("getblockchaininfo failed")
    }
}

impl std::error::Error for ChainInfoMissing {}

async fn poll_inner(state: &Shared, full: bool) {
    let rpc_addr = { state.read().await.rpc_addr.clone() };
    if rpc_addr.is_empty() {
        // Managed console with no network chosen: nothing to poll, only the agent's state to show.
        let mut g = state.write().await;
        stamp_power(&mut g);
        return;
    }
    match refresh(&rpc_addr, state, full).await {
        Ok(()) => {}
        Err(e) => {
            let listening = node_ctl::node_is_up(&rpc_addr).await;
            let mut g = state.write().await;
            if g.rpc_addr != rpc_addr {
                return;
            }
            // "Did the RPC answer" is the agent's start-up signal. A batch that came back (even without
            // chain info) answered; only a failure to get any answer takes the signal down.
            if e.downcast_ref::<ChainInfoMissing>().is_none() {
                if let Some(a) = &g.console.agent {
                    a.rpc_flag().store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
            mark_unreachable(&mut g, e.to_string(), listening);
        }
    }
}

async fn refresh(rpc_addr: &str, state: &Shared, full: bool) -> anyhow::Result<()> {
    let (refine_lo, refine_hi, prev) = {
        let g = state.read().await;
        let bounds = g
            .views
            .get(rpc_addr)
            .map(|c| (c.hashed_lo, c.hashed_hi))
            .unwrap_or((0, None));
        (bounds.0, bounds.1, g.snapshot.clone())
    };
    let probe_heights = if full {
        header_probe_heights(refine_lo, refine_hi)
    } else {
        Vec::new()
    };
    let mut calls = vec![
        rpc::RpcCall::method("getblockchaininfo"),
        rpc::RpcCall::method("getblockcount"),
    ];
    if full {
        calls.push(rpc::RpcCall::method("getpeerinfo"));
        calls.push(rpc::RpcCall::method("getnetworkinfo"));
        calls.push(rpc::RpcCall::method("listbanned"));
        calls.push(rpc::RpcCall::method("uptime"));
        for h in &probe_heights {
            calls.push(rpc::RpcCall::with_params(
                "getblockhash",
                serde_json::json!([h]),
            ));
        }
    }
    let mut answers = rpc::call_batch_calls(rpc_addr, &calls).await?;
    {
        // The node's RPC server answered something: that is all `starting -> running` needs (design B asks
        // for one `getblockcount`, not the whole probe batch). Only for the address we still follow.
        let g = state.read().await;
        if g.rpc_addr == rpc_addr {
            if let Some(a) = &g.console.agent {
                a.rpc_flag().store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    let chain = answers.get_mut(0).and_then(Option::take).ok_or(ChainInfoMissing)?;
    let counted = answers
        .get_mut(1)
        .and_then(Option::take)
        .and_then(|v| json_u64(&v))
        .unwrap_or(0);

    let (hashed_tip, next_lo, next_hi, peer_list, net, banned_v, node_uptime) = if full {
        let peers_v = answers
            .get_mut(2)
            .and_then(Option::take)
            .unwrap_or(serde_json::json!([]));
        let net = answers.get_mut(3).and_then(Option::take);
        let banned_v = answers
            .get_mut(4)
            .and_then(Option::take)
            .unwrap_or(serde_json::json!([]));
        let node_uptime = answers
            .get_mut(5)
            .and_then(Option::take)
            .and_then(|v| json_u64(&v))
            .map(|s| format_uptime(Duration::from_secs(s)))
            .unwrap_or_else(|| prev.uptime.clone());
        let probe_answers = &answers[6..];
        let hashed_tip = hashed_header_tip(probe_answers, &probe_heights);
        let (next_lo, next_hi) =
            apply_probe_bounds(refine_lo, refine_hi, &probe_heights, probe_answers);
        (
            hashed_tip,
            next_lo,
            next_hi,
            peers_v.as_array().cloned().unwrap_or_default(),
            net,
            banned_v,
            node_uptime,
        )
    } else {
        (
            0,
            refine_lo,
            refine_hi,
            Vec::new(),
            None,
            serde_json::json!([]),
            prev.uptime.clone(),
        )
    };

    let blocks = json_u64_field(&chain, "blocks")
        .or_else(|| json_u64_field(&chain, "height"))
        .unwrap_or(0)
        .max(counted);
    let headers = json_u64_field(&chain, "headers").unwrap_or(blocks);
    let peer_tip = peer_best_height(&peer_list);
    let rpc_ibd = chain
        .get("initialblockdownload")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let chain_name = chain
        .get("chain")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let size_on_disk = json_u64_field(&chain, "size_on_disk").unwrap_or(0);

    let (peers, inbound, outbound, version, peer_rows, banned, network_active) = if full {
        let peers = peer_list.len();
        let inbound = peer_list
            .iter()
            .filter(|p| json_bool(p, "inbound").unwrap_or(false))
            .count();
        let outbound = peers.saturating_sub(inbound);
        let version = net
            .as_ref()
            .and_then(|n| n.get("subversion").and_then(|v| v.as_str()))
            .map(parse_blvm_version)
            .unwrap_or_else(|| "—".into());
        let peer_rows: Vec<PeerRow> = peer_list
            .iter()
            .map(|p| PeerRow {
                geo: p.get("addr").and_then(|v| v.as_str()).and_then(crate::geo::locate),
                addr: p
                    .get("addr")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                inbound: p.get("inbound").and_then(|v| v.as_bool()).unwrap_or(false),
                subver: p
                    .get("subver")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
            .filter(|p| !p.addr.is_empty())
            .collect();
        let banned: Vec<BanRow> = banned_v
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|b| BanRow {
                address: b
                    .get("address")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                banned_until: b.get("banned_until").and_then(|v| v.as_u64()),
            })
            .filter(|b| !b.address.is_empty())
            .collect();
        let network_active = net
            .as_ref()
            .and_then(|n| n.get("networkactive").and_then(|v| v.as_bool()))
            .unwrap_or(true);
        (
            peers,
            inbound,
            outbound,
            version,
            peer_rows,
            banned,
            network_active,
        )
    } else {
        (
            prev.peers,
            prev.inbound,
            prev.outbound,
            prev.blvm_version.clone(),
            prev.peer_rows.clone(),
            prev.banned.clone(),
            prev.network_active,
        )
    };
    let self_geo = if full {
        crate::geo::self_geo(net.as_ref(), &peer_list)
    } else {
        prev.self_geo.clone()
    };

    let mut g = state.write().await;
    if g.rpc_addr != rpc_addr {
        return Ok(());
    }
    g.fail_streak = 0;
    g.was_connected = true;
    if let Some(a) = &g.console.agent {
        a.rpc_flag().store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let (ibd, healing, health, health_label, node_status, feed_waiting, feed_items, arrival, network_height, behind, pct, sync_pct, sync_pct_num) = {
        let c = chain_mut(&mut g);
        c.hashed_lo = next_lo.max(hashed_tip);
        c.hashed_hi = next_hi;
        let before = (c.last, c.feed.front().map(|f| f.key.clone()), c.feed.len());
        // A different chain behind the same RPC address starts a fresh history.
        // Only trust the name once the node reports blocks (its startup fallback says
        // "regtest" at height 0).
        if blocks > 0 {
            if let Some(name) = norm_chain(&chain_name) {
                if c.chain.as_deref().is_some_and(|old| old != name) {
                    c.feed.clear();
                    c.last = None;
                }
                c.chain = Some(name);
            }
        }
        // Headers (and this poll's getblockhash probe) show whether the node still
        // has its chain. A restart that re-validates from a checkpoint keeps them.
        let feed_height = confirm_feed_height(c, blocks, headers.max(hashed_tip));
        rewind_chain_if_needed(c, feed_height);
        // Stock RPC `headers` often tracks the body cursor after header IBD, so
        // a poll with no getblockhash hit would collapse network height to
        // local+prefetch (~99%). Keep the best header tip already seen.
        let network_height = chain_tip(headers, blocks, peer_tip, hashed_tip, c.hashed_lo);
        let behind = network_height.saturating_sub(blocks);
        let (pct, sync_pct, sync_pct_num) = format_sync_pct(blocks, network_height, behind);
        if behind == 0 && network_height > 0 {
            c.ever_synced = true;
        }
        let ibd = is_ibd(rpc_ibd, blocks, network_height, behind, c.ever_synced);
        let healing = behind > 0 && !ibd;
        let (health, health_label, node_status) = if ibd {
            ("heal", "Syncing", "Syncing")
        } else if healing {
            ("heal", "Healing", "Healing")
        } else if peers == 0 {
            ("heal", "No peers", "No peers")
        } else {
            ("good", "Healthy", "Online")
        };
        if c.connected_since.is_none() {
            c.connected_since = Some(Instant::now());
            if c.feed.is_empty() && c.last.is_none() {
                c.arrival = VecDeque::from(vec![0; HIST_BARS]);
                c.bucket_started = Instant::now();
                c.bucket_delta = 0;
            }
        }
        let delta = {
            let ChainFeed { feed, last, .. } = c;
            advance_feed(feed, last, feed_height)
        };
        if (c.last, c.feed.front().map(|f| f.key.clone()), c.feed.len()) != before {
            c.dirty = true;
        }
        roll_arrival(c, delta);
        let mut arrival: Vec<u64> = c.arrival.iter().copied().collect();
        if arrival.len() == HIST_BARS {
            arrival[HIST_BARS - 1] = c.bucket_delta;
        }
        let feed_waiting = c.feed.is_empty();
        let feed_items: Vec<FeedItem> = c.feed.iter().cloned().collect();
        (
            ibd,
            healing,
            health,
            health_label,
            node_status,
            feed_waiting,
            feed_items,
            arrival,
            network_height,
            behind,
            pct,
            sync_pct,
            sync_pct_num,
        )
    };
    if let Some(a) = &g.console.agent {
        a.note_height(&rpc_addr, blocks, network_height);
    }

    // Live-only version (design M.1): remember what the node said, show it only while it answers.
    let version_seen = {
        let c = chain_mut(&mut g);
        if version != "—" {
            c.version_seen = Some((version.clone(), crate::persist::unix_now()));
        }
        c.version_seen.clone()
    };
    let blvm_version_now = version.clone();
    node_ctl::remember_if_running(rpc_addr, &mut g.launch);
    let size_on_disk = chain_bytes_on_disk(rpc_addr, &g.launch, size_on_disk, blocks, headers);
    let (disk_num, disk_unit, disk_label) = format_bytes_parts(size_on_disk);
    let vol = disk_volume();

    g.snapshot = Snapshot {
        connected: true,
        rpc_addr: rpc_addr.to_string(),
        health,
        health_label: health_label.into(),
        node_status: node_status.into(),
        uptime: node_uptime,
        blvm_version: version,
        network: chain_name,
        sync_pct,
        sync_pct_num,
        syncing: ibd || healing,
        behind,
        local_height: blocks,
        network_height,
        ibd,
        ibd_label: if ibd {
            "active".into()
        } else if healing {
            "healing".into()
        } else {
            "idle".into()
        },
        peers,
        inbound,
        outbound,
        accepting_inbound: true,
        network_active,
        peer_rows,
        self_geo,
        banned,
        disk_used_num: disk_num,
        disk_used_unit: disk_unit,
        disk_used_label: disk_label,
        disk_used_bytes: size_on_disk,
        disk_free_num: vol.free_num,
        disk_free_unit: vol.free_unit,
        disk_free_label: vol.free_label,
        disk_free_bytes: vol.free_bytes,
        disk_total_bytes: vol.total_bytes,
        disk_vol_used_label: vol.used_label,
        feed: feed_items,
        feed_waiting,
        arrival,
        last_check: "just now".into(),
        ui_version: format!("v{}", env!("CARGO_PKG_VERSION")),
        ui_uptime: format_uptime(g.started.elapsed()),
        rpc_failures: 0,
        show_manual: false,
        error: None,
        frozen: false,
        node_running: true,
        node_busy: false,
        node_power_label: "Turn Node Off".into(),
        last_seen: Some(crate::persist::unix_now()),
        mode: prev.mode,
        setup_done: prev.setup_done,
        managed: None,
        modules_pages: prev.modules_pages.clone(),
        blvm_version_seen: if blvm_version_now == "—" {
            version_seen.map(|(version, at)| VersionSeen { version, at })
        } else {
            None
        },
        bundle_available: prev.bundle_available,
    };
    stamp_power(&mut g);
    let snap = g.snapshot.clone();
    let lk = crate::persist::LastKnown {
        at: snap.last_seen.unwrap_or_else(crate::persist::unix_now),
        local_height: snap.local_height,
        network_height: snap.network_height,
        behind: snap.behind,
        sync_pct: snap.sync_pct.clone(),
        sync_pct_num: snap.sync_pct_num.clone(),
        ibd: snap.ibd,
        network: snap.network.clone(),
        blvm_version: snap.blvm_version.clone(),
    };
    let c = chain_mut(&mut g);
    // A cheap tick right after a reconnect has no version yet; keep the saved one.
    let lk = if lk.blvm_version == "—" {
        crate::persist::LastKnown {
            blvm_version: c.last_known.as_ref().map(|k| k.blvm_version.clone()).unwrap_or(lk.blvm_version.clone()),
            ..lk
        }
    } else {
        lk
    };
    // Save when the status changes, and refresh the "last seen" time once a minute.
    let changed = match &c.last_known {
        Some(old) => !old.same_status(&lk) || lk.at.saturating_sub(old.at) >= 60,
        None => true,
    };
    if changed {
        c.last_known = Some(lk);
        c.dirty = true;
    }
    c.last_snapshot = Some(snap);
    let _ = pct;
    Ok(())
}

/// Datadir wipe / reindex / reorg: height went backwards. Arrival bars and
/// "already synced" must not keep the previous chain's state. Feed tiles are
/// trimmed in [`advance_feed`].
/// Height drops deeper than a normal reorg must hold this many polls (~1s each)
/// before the saved history is dropped. A restarting node can briefly report 0.
const DROP_CONFIRM_POLLS: u32 = 15;
const REORG_TOLERANCE: u64 = 6;

/// Height to feed the Latest Blocks history with. Only a real wipe clears the history:
/// - while the node's header tip still reaches the saved height, a lower block height
///   is a restart re-validating from a checkpoint, so the history is kept as is;
/// - otherwise the drop must hold for `DROP_CONFIRM_POLLS` polls (a restarting node
///   can briefly report 0 for everything).
fn confirm_feed_height(c: &mut ChainFeed, blocks: u64, header_tip: u64) -> u64 {
    match c.last {
        Some(last) if blocks.saturating_add(REORG_TOLERANCE) < last => {
            if header_tip.saturating_add(REORG_TOLERANCE) >= last {
                c.drop_seen = 0;
                return last;
            }
            c.drop_seen += 1;
            if c.drop_seen < DROP_CONFIRM_POLLS {
                last
            } else {
                c.drop_seen = 0;
                blocks
            }
        }
        _ => {
            c.drop_seen = 0;
            blocks
        }
    }
}

/// `main` / `test` / `testnet4` / `signet` / `regtest`; `None` if unknown.
fn norm_chain(name: &str) -> Option<String> {
    let n = name.trim().to_ascii_lowercase();
    let n = match n.as_str() {
        "main" | "mainnet" | "bitcoinv1" => "main",
        "test" | "testnet" | "testnet3" => "test",
        "testnet4" => "testnet4",
        "signet" => "signet",
        "regtest" => "regtest",
        _ => return None,
    };
    Some(n.to_string())
}

fn rewind_chain_if_needed(c: &mut ChainFeed, blocks: u64) {
    if c.last.is_some_and(|h| blocks < h) {
        c.arrival = VecDeque::from(vec![0; HIST_BARS]);
        c.bucket_started = Instant::now();
        c.bucket_delta = 0;
        c.ever_synced = false;
        c.connected_since = Some(Instant::now());
        c.hashed_lo = 0;
        c.hashed_hi = None;
    }
}

fn network_from_rpc(rpc: &str) -> String {
    match rpc.rsplit(':').next().unwrap_or("") {
        "38332" => "signet".into(),
        "48332" => "testnet4".into(),
        "8332" => "main".into(),
        "18332" => "test".into(),
        "18443" => "regtest".into(),
        _ => "—".into(),
    }
}

/// RPC timeouts in a row (~1s apart) while the node is still listening before the
/// console calls it frozen. One slow answer is normal during heavy sync.
const FROZEN_AFTER: u32 = 5;

fn mark_unreachable(g: &mut LiveState, err: String, listening: bool) {
    g.fail_streak = g.fail_streak.saturating_add(1);
    let hung = listening && crate::rpc::is_frozen_error(&err) && g.fail_streak >= FROZEN_AFTER;
    let show_manual = g.fail_streak >= MANUAL_AFTER;
    let ui_uptime = format_uptime(g.started.elapsed());
    let refused = crate::rpc::is_connect_refused(&err);
    let parked = g.views.get(&g.rpc_addr).and_then(|c| c.last_snapshot.clone());
    let (feed, arrival, saved_height, last_known, version_seen) = {
        let c = chain_mut(g);
        (
            c.feed.iter().cloned().collect::<Vec<_>>(),
            c.arrival.iter().copied().collect::<Vec<_>>(),
            c.last,
            c.last_known.clone(),
            c.version_seen.clone(),
        )
    };
    let mut snap = parked.unwrap_or_else(|| Snapshot::disconnected(&g.rpc_addr));
    let had_live = snap.connected
        || snap.frozen
        || snap.ibd
        || snap.local_height > 0
        || snap.network_height > 0;
    // A failed RPC poll is not a dead node. The process is down only when
    // nothing is listening (or connect was refused and we have no live view).
    let node_up = listening || (had_live && !refused);
    snap.rpc_addr = g.rpc_addr.clone();
    if snap.network == "—" {
        snap.network = network_from_rpc(&g.rpc_addr);
    }
    if node_up && hung {
        // Listening but not answering for a while: frozen. Keep the last known
        // status; if this console never got an answer, use the saved block height.
        snap.connected = false;
        snap.frozen = true;
        snap.node_running = true;
        snap.health = "dead";
        snap.health_label = "Not responding".into();
        snap.node_status = "Frozen".into();
        if !had_live {
            // This console never heard from the node since it started: show the
            // saved status from the node's last answer instead of zeros.
            if let Some(lk) = &last_known {
                snap.local_height = lk.local_height;
                snap.network_height = lk.network_height;
                snap.behind = lk.behind;
                snap.sync_pct = lk.sync_pct.clone();
                snap.sync_pct_num = lk.sync_pct_num.clone();
                snap.ibd = lk.ibd;
                snap.syncing = lk.ibd || lk.behind > 0;
                if !lk.network.is_empty() && lk.network != "—" {
                    snap.network = lk.network.clone();
                }
                snap.last_seen = Some(lk.at);
            } else if let Some(h) = saved_height {
                snap.local_height = h;
            }
        }
    } else if node_up {
        snap.connected = had_live;
        snap.frozen = false;
        snap.node_running = true;
        if !had_live {
            snap.health = "heal";
            snap.health_label = "Looking".into();
            snap.node_status = "Looking".into();
        }
    } else {
        snap.connected = false;
        snap.frozen = false;
        snap.node_running = false;
        snap.health = "dead";
        snap.health_label = if show_manual {
            "Can't reach node".into()
        } else {
            "Node down".into()
        };
        snap.node_status = "Down".into();
        snap.network_active = false;
        snap.accepting_inbound = false;
    }
    // Live-only version (design M.1): never show one next to "node is down". What the node last said
    // moves to `blvm_version_seen`, for a muted "last seen" line.
    if snap.connected {
        snap.blvm_version_seen = None;
    } else {
        let carried = (snap.blvm_version != "—").then(|| {
            (snap.blvm_version.clone(), snap.last_seen.unwrap_or_else(crate::persist::unix_now))
        });
        let saved = last_known
            .as_ref()
            .filter(|lk| !lk.blvm_version.is_empty() && lk.blvm_version != "—")
            .map(|lk| (lk.blvm_version.clone(), lk.at));
        snap.blvm_version_seen = version_seen
            .or(carried)
            .or(saved)
            .map(|(version, at)| VersionSeen { version, at });
        snap.blvm_version = "—".into();
    }
    snap.error = Some(err);
    snap.rpc_failures = g.fail_streak;
    snap.show_manual = show_manual && !node_up;
    snap.ui_uptime = ui_uptime;
    snap.last_check = "just now".into();
    if snap.feed.is_empty() {
        snap.feed = feed;
    }
    snap.feed_waiting = snap.feed.is_empty();
    if snap.arrival.is_empty() {
        snap.arrival = arrival;
    }
    g.snapshot = snap;
    stamp_power(g);
}

fn roll_arrival(c: &mut ChainFeed, delta: u64) {
    c.bucket_delta = c.bucket_delta.saturating_add(delta);
    if c.bucket_started.elapsed() >= Duration::from_secs(60) {
        if c.arrival.len() != HIST_BARS {
            c.arrival = VecDeque::from(vec![0; HIST_BARS]);
        }
        c.arrival.pop_front();
        c.arrival.push_back(c.bucket_delta);
        c.bucket_delta = 0;
        c.bucket_started = Instant::now();
    }
}

/// `/BitcoinCommons:0.1.0/` or `/blvm-node:0.1.0/` → `v0.1.0`
pub fn parse_blvm_version(sub: &str) -> String {
    let s = sub.trim().trim_matches('/');
    if let Some((_, ver)) = s.rsplit_once(':') {
        let ver = ver.trim().trim_matches('/');
        if !ver.is_empty() {
            return if ver.starts_with('v') {
                ver.to_string()
            } else {
                format!("v{ver}")
            };
        }
    }
    s.to_string()
}

/// First download vs catch-up after a completed sync.
///
/// Node RPC only sets `initialblockdownload` at height 0. Any lag before this
/// console has seen network tip is IBD (Syncing). After tip, lag is Healing.
/// If the console restarts against an almost-synced node, a ≥99.9% chain with
/// at most ~1 day of headers remaining is Healing, not a new IBD.
fn is_ibd(rpc_ibd: bool, blocks: u64, network_height: u64, behind: u64, ever_synced: bool) -> bool {
    if rpc_ibd {
        return true;
    }
    if behind == 0 {
        return false;
    }
    if ever_synced {
        return false;
    }
    if blocks == 0 {
        return true;
    }
    let near_tip =
        network_height > 0 && (blocks as u128).saturating_mul(1000) / (network_height as u128) >= 999;
    !(near_tip && behind <= 144)
}

fn format_sync_pct(blocks: u64, network_height: u64, behind: u64) -> (f64, String, String) {
    let pct = if network_height == 0 {
        0.0
    } else {
        (blocks as f64 / network_height as f64 * 100.0).clamp(0.0, 100.0)
    };
    if network_height == 0 {
        return (0.0, "—".into(), "—".into());
    }
    if behind == 0 && pct >= 100.0 {
        return (100.0, "100%".into(), "100".into());
    }
    if pct > 0.0 && pct < 1.0 {
        let n = format!("{pct:.2}");
        return (pct, format!("{n}%"), n);
    }
    if pct >= 99.0 && pct < 100.0 {
        let n = format!("{pct:.3}");
        return (pct, format!("{n}%"), n);
    }
    let n = format!("{pct:.1}");
    (pct, format!("{n}%"), n)
}

fn json_u64(v: &serde_json::Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_i64().and_then(|i| u64::try_from(i).ok()))
        .or_else(|| v.as_f64().and_then(|f| if f >= 0.0 { Some(f as u64) } else { None }))
}

fn json_u64_field(obj: &serde_json::Value, key: &str) -> Option<u64> {
    obj.get(key).and_then(json_u64)
}

fn json_bool(obj: &serde_json::Value, key: &str) -> Option<bool> {
    obj.get(key).and_then(|v| v.as_bool())
}

/// Core `headers` is the header tip. Stock BLVM often reports a body/prefetch
/// cursor there after header IBD (`headers ≈ blocks`). Only treat it as chain
/// tip when it is clearly ahead of the validated tip, or we have no bodies yet.
fn rpc_header_tip(headers: u64, blocks: u64) -> u64 {
    if blocks == 0 {
        return headers;
    }
    if headers > blocks.saturating_add(10_000) {
        headers
    } else {
        0
    }
}

/// Best known chain height. `remembered` is the last `getblockhash` header hit
/// so a later poll that only sees the body cursor cannot reset network height.
fn chain_tip(headers: u64, blocks: u64, peer_tip: u64, hashed_tip: u64, remembered: u64) -> u64 {
    rpc_header_tip(headers, blocks)
        .max(blocks)
        .max(peer_tip)
        .max(hashed_tip)
        .max(remembered)
}

/// Stock `size_on_disk` is ~1 MiB × block count, not `du`.
fn rpc_size_is_placeholder(rpc_bytes: u64, blocks: u64, headers: u64) -> bool {
    if rpc_bytes < 10_000_000 {
        return false;
    }
    for n in [blocks, headers, blocks.max(headers)] {
        if n == 0 {
            continue;
        }
        for unit in [1_000_000u64, 1_048_576] {
            let guess = n.saturating_mul(unit);
            if guess == 0 {
                continue;
            }
            let lo = guess.saturating_mul(8) / 10;
            let hi = guess.saturating_mul(13) / 10;
            if rpc_bytes >= lo && rpc_bytes <= hi {
                return true;
            }
        }
    }
    false
}

fn chain_bytes_on_disk(
    rpc_addr: &str,
    launch: &Option<node_ctl::LaunchSpec>,
    rpc_bytes: u64,
    blocks: u64,
    headers: u64,
) -> u64 {
    if let Some(n) = node_ctl::datadir_usage(rpc_addr, launch) {
        if n > 0 {
            return n;
        }
    }
    if rpc_size_is_placeholder(rpc_bytes, blocks, headers) {
        0
    } else {
        rpc_bytes
    }
}

/// Best advertised height on a peer. Stock BLVM currently hardcodes
/// `startingheight = 0` / `synced_headers = -1`; ignore non-positive values.
fn peer_best_height(peers: &[serde_json::Value]) -> u64 {
    peers
        .iter()
        .filter_map(|p| {
            ["startingheight", "synced_headers", "synced_blocks"]
                .iter()
                .filter_map(|k| json_u64_field(p, k).filter(|h| *h > 0))
                .max()
        })
        .max()
        .unwrap_or(0)
}

/// Heights to probe with stock `getblockhash`. Some nodes leave
/// `getblockchaininfo.headers` at 0 during header IBD while the height
/// index (used by `getblockhash`) already has those headers.
///
/// Keep the batch small (one TCP / poll, node rate-limits new sockets).
/// Powers of two alone stick at 2^n; quartiles plus a lo/hi refine move
/// the number as headers arrive.
fn header_probe_heights(lo: u64, hi: Option<u64>) -> Vec<u64> {
    use std::collections::BTreeSet;
    let mut set = BTreeSet::new();
    const CAP: u64 = 8_388_608;
    match hi {
        None if lo == 0 => {
            let mut p = 1u64 << 10;
            while p <= CAP {
                set.insert(p);
                if p >= 1 << 16 {
                    set.insert(p + p / 4);
                    set.insert(p + p / 2);
                    set.insert(p + 3 * p / 4);
                }
                match p.checked_mul(2) {
                    Some(n) => p = n,
                    None => break,
                }
            }
        }
        None => {
            for d in [1, 256, 2_048, 16_384, 65_536, 262_144, 1 << 20, 1 << 21] {
                let h = lo.saturating_add(d);
                if h <= CAP {
                    set.insert(h);
                }
            }
        }
        Some(h) if h > lo + 1 => {
            let span = h - lo;
            for j in 1..8u64 {
                set.insert(lo + span * j / 8);
            }
            set.insert(lo.saturating_add(1));
            for d in [1, 4_096, 65_536, 262_144] {
                set.insert(h.saturating_add(d));
            }
        }
        Some(_) => {
            for d in [1, 2, 16, 128, 1_024, 8_192, 65_536, 262_144] {
                let h = lo.saturating_add(d);
                if h <= CAP {
                    set.insert(h);
                }
            }
        }
    }
    set.into_iter().filter(|&h| h > 0).collect()
}

fn apply_probe_bounds(
    mut lo: u64,
    mut hi: Option<u64>,
    heights: &[u64],
    answers: &[Option<serde_json::Value>],
) -> (u64, Option<u64>) {
    for (h, ans) in heights.iter().zip(answers.iter()) {
        let hit = ans
            .as_ref()
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.len() >= 16);
        if hit {
            lo = lo.max(*h);
        } else if *h > lo {
            hi = Some(hi.map_or(*h, |x| x.min(*h)));
        }
    }
    if hi.is_some_and(|h| h <= lo) {
        hi = None;
    }
    (lo, hi)
}

fn hashed_header_tip(answers: &[Option<serde_json::Value>], heights: &[u64]) -> u64 {
    heights
        .iter()
        .zip(answers.iter())
        .filter_map(|(h, ans)| {
            ans.as_ref()
                .and_then(|v| v.as_str())
                .filter(|s| s.len() >= 16)
                .map(|_| *h)
        })
        .max()
        .unwrap_or(0)
}

fn format_uptime(d: Duration) -> String {
    let s = d.as_secs();
    let days = s / 86400;
    let hours = (s % 86400) / 3600;
    let mins = (s % 3600) / 60;
    if days > 0 {
        format!("{days}d {hours:02}h {mins:02}m")
    } else {
        format!("{hours:02}h {mins:02}m")
    }
}

struct DiskVolume {
    free_num: String,
    free_unit: String,
    free_label: String,
    used_label: String,
    free_bytes: u64,
    total_bytes: u64,
}

fn disk_volume() -> DiskVolume {
    let (total, free) = volume_space();
    let used = total.saturating_sub(free);
    let (free_num, free_unit, free_label) = format_bytes_parts(free);
    let (_, _, used_label) = format_bytes_parts(used);
    DiskVolume {
        free_num,
        free_unit,
        free_label,
        used_label,
        free_bytes: free,
        total_bytes: total,
    }
}

fn volume_space() -> (u64, u64) {
    let path = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    volume_space_at(&path)
}

/// (total, available) bytes of the volume holding `path`, or of its nearest existing parent.
#[cfg(unix)]
pub fn volume_space_at(path: &std::path::Path) -> (u64, u64) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let mut path = path.to_path_buf();
    while !path.exists() {
        if !path.pop() {
            break;
        }
    }
    let path = path.canonicalize().unwrap_or(path);
    let Ok(c_path) = CString::new(path.as_os_str().as_bytes()) else {
        return (0, 0);
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) } != 0 {
        return (0, 0);
    }
    let frag = stat.f_frsize as u64;
    if frag == 0 {
        return (0, 0);
    }
    let total = (stat.f_blocks as u64).saturating_mul(frag);
    let available = (stat.f_bavail as u64).saturating_mul(frag);
    (total, available)
}

#[cfg(not(unix))]
pub fn volume_space_at(_path: &std::path::Path) -> (u64, u64) {
    (0, 0)
}

fn format_bytes_parts(n: u64) -> (String, String, String) {
    const TB: f64 = 1_000_000_000_000.0;
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    if n == 0 {
        return ("—".into(), "".into(), "—".into());
    }
    let t = n as f64 / TB;
    if t >= 1.0 {
        let num = format!("{t:.2}");
        return (num.clone(), "TB".into(), format!("{num} TB"));
    }
    let g = n as f64 / GB;
    if g >= 1.0 {
        let num = format!("{g:.1}");
        return (num.clone(), "GB".into(), format!("{num} GB"));
    }
    let num = format!("{:.0}", n as f64 / MB);
    (num.clone(), "MB".into(), format!("{num} MB"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uptime_from_node_seconds() {
        assert_eq!(format_uptime(Duration::from_secs(0)), "00h 00m");
        assert_eq!(format_uptime(Duration::from_secs(90)), "00h 01m");
        assert_eq!(format_uptime(Duration::from_secs(3661)), "01h 01m");
        assert_eq!(format_uptime(Duration::from_secs(90_061)), "1d 01h 01m");
        assert_eq!(json_u64(&serde_json::json!(3661)), Some(3661));
    }

    #[cfg(unix)]
    #[test]
    fn volume_space_reads_this_disk() {
        let (total, free) = volume_space();
        assert!(total > 0, "statvfs should see this volume");
        assert!(free <= total);
        let vol = disk_volume();
        assert_eq!(vol.total_bytes, total);
        assert_eq!(vol.free_bytes, free);
        assert_ne!(vol.free_label, "—");
    }

    #[test]
    fn parses_bitcoin_commons_subversion() {
        assert_eq!(parse_blvm_version("/BitcoinCommons:0.2.1/"), "v0.2.1");
        assert_eq!(parse_blvm_version("/blvm-node:0.1.0/"), "v0.1.0");
        assert_eq!(parse_blvm_version("v1.0.0"), "v1.0.0");
    }

    #[test]
    fn sync_pct_near_tip_has_three_decimals() {
        let (_, label, num) = format_sync_pct(927_545, 927_551, 6);
        assert_eq!(label, "99.999%");
        assert_eq!(num, "99.999");
    }

    #[test]
    fn sync_pct_early_ibd_has_two_decimals() {
        let (_, label, num) = format_sync_pct(128, 322_544, 322_416);
        assert_eq!(label, "0.04%");
        assert_eq!(num, "0.04");
    }

    #[test]
    fn manual_connect_after_five_misses() {
        assert_eq!(MANUAL_AFTER, 5);
    }

    #[test]
    fn disconnected_is_down_not_healing() {
        let s = Snapshot::disconnected("127.0.0.1:38332");
        assert_eq!(s.health, "dead");
        assert_eq!(s.health_label, "Node down");
        assert_eq!(s.node_status, "Down");
        assert!(!s.connected);
        assert!(!s.node_running);
        assert_eq!(s.node_power_label, "Turn Node On");
    }

    #[test]
    fn first_sync_is_ibd_not_healing() {
        assert!(is_ibd(true, 0, 322_549, 322_549, false));
        assert!(is_ibd(false, 2_000, 322_549, 320_549, false));
        assert!(is_ibd(false, 300_000, 322_549, 22_549, false));
    }

    #[test]
    fn stock_header_ibd_uses_headers_not_peer_startingheight() {
        let chain = serde_json::json!({
            "chain": "test",
            "blocks": 0,
            "headers": 1_348_001,
            "initialblockdownload": true
        });
        let peers = vec![serde_json::json!({
            "addr": "1.2.3.4:18333",
            "startingheight": 0,
            "synced_headers": -1,
            "inbound": false
        })];
        let blocks = json_u64_field(&chain, "blocks").unwrap_or(0);
        let headers = json_u64_field(&chain, "headers").unwrap_or(blocks);
        let network_height = headers.max(blocks).max(peer_best_height(&peers));
        let behind = network_height.saturating_sub(blocks);
        let (_pct, _label, num) = format_sync_pct(blocks, network_height, behind);
        assert_eq!(blocks, 0);
        assert_eq!(network_height, 1_348_001);
        assert_eq!(behind, 1_348_001);
        assert_eq!(num, "0.0");
        assert!(is_ibd(true, blocks, network_height, behind, false));
    }

    #[test]
    fn stock_zero_headers_uses_getblockhash_probe() {
        let heights = header_probe_heights(0, None);
        assert!(heights.contains(&1_048_576));
        assert!(heights.iter().any(|h| *h > 1_048_576 && *h < 1_572_864));
        let mut answers = vec![None; heights.len()];
        for (i, h) in heights.iter().enumerate() {
            if *h <= 1_350_000 {
                answers[i] = Some(serde_json::json!("ab".repeat(32)));
            }
        }
        let hashed = hashed_header_tip(&answers, &heights);
        assert!(hashed > 1_048_576, "stuck on power-of-two {hashed}");
        assert!(hashed <= 1_350_000);
        let network_height = hashed;
        let (_pct, _, num) = format_sync_pct(0, network_height, network_height);
        assert_eq!(num, "0.0");
    }

    #[test]
    fn body_ibd_keeps_header_tip_not_validation_cursor() {
        let remembered = 5_150_270;
        let blocks = 29_000;
        let headers = 29_170;
        let network_height = chain_tip(headers, blocks, 0, 0, remembered);
        assert_eq!(network_height, remembered);
        let behind = network_height.saturating_sub(blocks);
        let (_pct, label, num) = format_sync_pct(blocks, network_height, behind);
        assert!(!label.starts_with("99"), "got {label}");
        assert_eq!(num, "0.56");
        assert!(is_ibd(true, blocks, network_height, behind, false));
    }

    #[test]
    fn rpc_one_mb_per_block_is_placeholder() {
        assert!(rpc_size_is_placeholder(29_000 * 1_000_000, 29_000, 29_170));
        assert!(rpc_size_is_placeholder(74_000 * 1_000_000, 74_000, 74_000));
        assert!(!rpc_size_is_placeholder(640 * 1_000_000, 29_000, 5_150_270));
    }

    #[test]
    fn catch_up_after_tip_is_healing_not_ibd() {
        assert!(!is_ibd(false, 322_540, 322_549, 9, true));
        assert!(!is_ibd(false, 322_549, 322_549, 0, true));
    }

    #[test]
    fn restart_near_tip_is_healing() {
        assert!(!is_ibd(false, 322_540, 322_549, 9, false));
        assert!(is_ibd(false, 322_000, 322_549, 549, false));
    }

    #[test]
    fn switching_rpc_keeps_each_chain_feed() {
        let mut g = LiveState::new("127.0.0.1:38332".into());
        {
            let c = chain_mut(&mut g);
            c.last = Some(100);
            c.feed.push_front(FeedItem::block(100));
        }
        g.rpc_addr = "127.0.0.1:48332".into();
        {
            let c = chain_mut(&mut g);
            c.last = Some(50);
            c.feed.push_front(FeedItem::block(50));
        }
        g.rpc_addr = "127.0.0.1:38332".into();
        let signet = chain_mut(&mut g);
        assert_eq!(signet.last, Some(100));
        assert_eq!(signet.feed.front().unwrap().label, "100");
        g.rpc_addr = "127.0.0.1:48332".into();
        let testnet = chain_mut(&mut g);
        assert_eq!(testnet.last, Some(50));
        assert_eq!(testnet.feed.front().unwrap().label, "50");
    }

    #[test]
    fn switching_rpc_back_restores_that_networks_history_and_last_status() {
        // Managed switches move the poller between each network's default RPC address; the block
        // history, the last status and the saved version are all keyed by that address.
        let regtest = "127.0.0.1:18443";
        let signet = "127.0.0.1:38332";
        let mut g = LiveState::new(regtest.into());
        {
            let c = chain_mut(&mut g);
            c.last = Some(150);
            c.feed.push_front(FeedItem::block(150));
            let mut snap = Snapshot::disconnected(regtest);
            snap.connected = true;
            snap.local_height = 150;
            snap.network = "regtest".into();
            snap.feed = vec![FeedItem::block(150)];
            c.last_snapshot = Some(snap);
            c.dirty = true;
        }
        let saved_before = g.all_feeds();
        g.switch_rpc(signet.into());
        assert_eq!(g.snapshot.rpc_addr, signet);
        assert!(g.snapshot.feed.is_empty(), "a network never polled shows no blocks of another");
        assert_eq!(g.snapshot.local_height, 0);
        {
            let c = chain_mut(&mut g);
            c.last = Some(210_000);
            c.feed.push_front(FeedItem::block(210_000));
        }
        g.switch_rpc(regtest.into());
        assert_eq!(g.snapshot.rpc_addr, regtest);
        assert_eq!(g.snapshot.local_height, 150, "last status of that network is back at once");
        assert_eq!(g.snapshot.feed.len(), 1);
        assert_eq!(chain_mut(&mut g).last, Some(150));
        assert_eq!(chain_mut(&mut g).feed.front().unwrap().label, "150");
        let saved_after = g.all_feeds();
        assert_eq!(saved_after[regtest], saved_before[regtest], "regtest history untouched by the signet detour");
        assert_eq!(saved_after[signet].last, Some(210_000));
    }

    #[test]
    fn height_rewind_clears_synced_and_arrival() {
        let mut g = LiveState::new("127.0.0.1:48332".into());
        {
            let c = chain_mut(&mut g);
            c.last = Some(152_885);
            c.feed.push_front(FeedItem::block(152_885));
            c.ever_synced = true;
            c.bucket_delta = 40;
            c.arrival = VecDeque::from(vec![9; HIST_BARS]);
        }
        {
            let c = chain_mut(&mut g);
            rewind_chain_if_needed(c, 0);
            crate::feed::advance_feed(&mut c.feed, &mut c.last, 0);
            assert!(!c.ever_synced);
            assert_eq!(c.bucket_delta, 0);
            assert!(c.arrival.iter().all(|&n| n == 0));
            assert_eq!(c.feed.len(), 1);
            assert_eq!(c.feed[0], FeedItem::block(0));
        }
    }

    #[test]
    fn brief_zero_height_keeps_history_but_a_real_wipe_clears_it() {
        let mut c = ChainFeed::new();
        c.last = Some(5_000);
        c.feed.push_front(FeedItem::chunk(1, 5_000));
        // Node restarting: reports 0 for a few polls, then its real height again.
        for _ in 0..5 {
            assert_eq!(confirm_feed_height(&mut c, 0, 0), 5_000);
        }
        assert_eq!(confirm_feed_height(&mut c, 5_001, 5_001), 5_001);
        assert_eq!(c.drop_seen, 0);
        // Wiped data dir: stays low, so the drop goes through after the window.
        let mut seen = 5_001;
        for _ in 0..DROP_CONFIRM_POLLS {
            seen = confirm_feed_height(&mut c, 3, 3);
        }
        assert_eq!(seen, 3);
        // Small reorgs pass straight through.
        c.last = Some(100);
        assert_eq!(confirm_feed_height(&mut c, 97, 97), 97);
    }

    #[test]
    fn restart_that_revalidates_keeps_history() {
        let mut c = ChainFeed::new();
        c.last = Some(501_000);
        c.feed.push_front(FeedItem::chunk(500_001, 501_000));
        // Node came back at a low checkpoint but still has all its headers.
        for _ in 0..(DROP_CONFIRM_POLLS * 4) {
            assert_eq!(confirm_feed_height(&mut c, 3_111, 501_059), 501_000);
        }
        // Once blocks pass the saved height again, the feed moves on normally.
        assert_eq!(confirm_feed_height(&mut c, 501_200, 501_200), 501_200);
    }

    #[test]
    fn chain_names_normalize() {
        assert_eq!(norm_chain("testnet").as_deref(), Some("test"));
        assert_eq!(norm_chain("Test").as_deref(), Some("test"));
        assert_eq!(norm_chain("mainnet").as_deref(), Some("main"));
        assert_eq!(norm_chain("testnet4").as_deref(), Some("testnet4"));
        assert_eq!(norm_chain("unknown"), None);
    }

    #[test]
    fn restore_and_export_round_trip() {
        let mut saved = std::collections::BTreeMap::new();
        saved.insert(
            "127.0.0.1:18332".to_string(),
            crate::persist::StoredView {
                chain: Some("test".into()),
                last: Some(250),
                items: vec![
                    crate::persist::StoredItem::from_item(&FeedItem::block(250)),
                    crate::persist::StoredItem::from_item(&FeedItem::chunk(100, 249)),
                ],
                last_known: None,
            },
        );
        let mut g = LiveState::new("127.0.0.1:18332".into());
        g.restore(saved.clone());
        assert!(g.take_dirty_feeds().is_none(), "restoring is not a change");
        assert_eq!(chain_mut(&mut g).feed[0], FeedItem::block(250));
        chain_mut(&mut g).dirty = true;
        assert_eq!(g.take_dirty_feeds().unwrap(), saved);
        assert!(g.take_dirty_feeds().is_none());
    }

    #[test]
    fn unreachable_keeps_last_chain_status() {
        let mut g = LiveState::new("127.0.0.1:38332".into());
        g.snapshot.connected = true;
        g.snapshot.local_height = 184_800;
        g.snapshot.network_height = 322_555;
        g.snapshot.network = "signet".into();
        g.snapshot.sync_pct_num = "57.3".into();
        g.snapshot.network_active = true;
        g.snapshot.ibd = true;
        g.snapshot.health = "heal";
        g.snapshot.health_label = "Syncing".into();
        g.snapshot.node_status = "Syncing".into();
        chain_mut(&mut g).last_snapshot = Some(g.snapshot.clone());
        mark_unreachable(&mut g, "RPC 127.0.0.1:38332 timed out".into(), true);
        assert!(g.snapshot.connected);
        assert!(g.snapshot.node_running);
        assert!(!g.snapshot.frozen);
        assert_eq!(g.snapshot.health, "heal");
        assert_eq!(g.snapshot.health_label, "Syncing");
        assert_eq!(g.snapshot.node_status, "Syncing");
        assert_eq!(g.snapshot.local_height, 184_800);
        assert_eq!(g.snapshot.network, "signet");
        assert_eq!(g.snapshot.sync_pct_num, "57.3");
        assert!(g.snapshot.network_active);
        assert_eq!(g.snapshot.node_power_label, "Turn Node Off");
    }

    #[test]
    fn long_run_of_timeouts_while_listening_is_frozen() {
        let mut g = LiveState::new("127.0.0.1:18332".into());
        chain_mut(&mut g).last = Some(490_000);
        for _ in 0..(FROZEN_AFTER - 1) {
            mark_unreachable(&mut g, "RPC 127.0.0.1:18332 frozen".into(), true);
            assert!(!g.snapshot.frozen, "a few slow answers are not frozen yet");
        }
        mark_unreachable(&mut g, "RPC 127.0.0.1:18332 frozen".into(), true);
        assert!(g.snapshot.frozen);
        assert!(!g.snapshot.connected);
        assert!(g.snapshot.node_running);
        assert_eq!(g.snapshot.node_status, "Frozen");
        assert_eq!(g.snapshot.local_height, 490_000, "falls back to the saved height");

        // With a saved status from the node's last answer, show all of it.
        let mut s = LiveState::new("127.0.0.1:18332".into());
        chain_mut(&mut s).last_known = Some(crate::persist::LastKnown {
            at: 1_790_000_000,
            local_height: 490_000,
            network_height: 5_150_380,
            behind: 4_660_380,
            sync_pct: "9.51%".into(),
            sync_pct_num: "9.51".into(),
            ibd: true,
            network: "test".into(),
            blvm_version: "v0.1.13".into(),
        });
        for _ in 0..FROZEN_AFTER {
            mark_unreachable(&mut s, "RPC 127.0.0.1:18332 frozen".into(), true);
        }
        assert!(s.snapshot.frozen);
        assert_eq!(s.snapshot.sync_pct_num, "9.51");
        assert_eq!(s.snapshot.network_height, 5_150_380);
        assert_eq!(s.snapshot.local_height, 490_000);
        assert_eq!(s.snapshot.last_seen, Some(1_790_000_000));
        // Refused connections are still Down, never frozen.
        let mut d = LiveState::new("127.0.0.1:18332".into());
        for _ in 0..10 {
            mark_unreachable(&mut d, "connect 127.0.0.1:18332".into(), false);
        }
        assert!(!d.snapshot.frozen);
    }

    #[test]
    fn full_poll_is_every_fourth_one_second_tick() {
        assert!(is_full_poll(1));
        assert!(!is_full_poll(2));
        assert!(!is_full_poll(3));
        assert!(!is_full_poll(4));
        assert!(is_full_poll(5));
    }

    #[test]
    fn rate_limit_while_up_is_not_down() {
        let mut g = LiveState::new("127.0.0.1:18332".into());
        g.snapshot.connected = true;
        g.snapshot.health = "heal";
        g.snapshot.health_label = "Syncing".into();
        g.snapshot.node_status = "Syncing".into();
        g.snapshot.ibd = true;
        g.snapshot.local_height = 868_000;
        g.snapshot.network_height = 5_111_808;
        chain_mut(&mut g).last_snapshot = Some(g.snapshot.clone());
        mark_unreachable(
            &mut g,
            "Connection rate limit exceeded for IP 127.0.0.1".into(),
            true,
        );
        assert_ne!(g.snapshot.health_label, "Node down");
        assert_ne!(g.snapshot.node_status, "Down");
        assert_eq!(g.snapshot.health_label, "Syncing");
        assert!(g.snapshot.node_running);
    }

    #[test]
    fn connection_refused_is_down_not_frozen() {
        let mut g = LiveState::new("127.0.0.1:38332".into());
        mark_unreachable(&mut g, "connect 127.0.0.1:38332".into(), false);
        assert!(!g.snapshot.frozen);
        assert!(!g.snapshot.node_running);
        assert_eq!(g.snapshot.health, "dead");
        assert_eq!(g.snapshot.health_label, "Node down");
        assert_eq!(g.snapshot.network, "signet");
    }

    #[test]
    fn refused_after_live_poll_is_down() {
        let mut g = LiveState::new("127.0.0.1:18332".into());
        g.snapshot.connected = true;
        g.snapshot.health = "heal";
        g.snapshot.health_label = "Syncing".into();
        g.snapshot.local_height = 100;
        chain_mut(&mut g).last_snapshot = Some(g.snapshot.clone());
        mark_unreachable(&mut g, "connect 127.0.0.1:18332".into(), false);
        assert_eq!(g.snapshot.health_label, "Node down");
        assert_eq!(g.snapshot.node_status, "Down");
        assert!(!g.snapshot.node_running);
    }
    // ---- version truth (design M.1) ----

    fn live_snapshot(g: &mut LiveState) {
        g.snapshot.connected = true;
        g.snapshot.local_height = 100;
        g.snapshot.blvm_version = "v0.1.13".into();
        g.snapshot.last_seen = Some(1_790_000_500);
        chain_mut(g).last_snapshot = Some(g.snapshot.clone());
    }

    #[test]
    fn version_is_dash_when_the_node_stops_answering_and_last_seen_keeps_it() {
        let mut g = LiveState::new("127.0.0.1:18332".into());
        live_snapshot(&mut g);
        chain_mut(&mut g).version_seen = Some(("v0.1.13".into(), 1_790_000_400));

        // Process gone: refused connection.
        mark_unreachable(&mut g, "connect 127.0.0.1:18332".into(), false);
        assert!(!g.snapshot.connected);
        assert_eq!(g.snapshot.blvm_version, "—");
        assert_eq!(
            g.snapshot.blvm_version_seen,
            Some(VersionSeen { version: "v0.1.13".into(), at: 1_790_000_400 })
        );
        let v = serde_json::to_value(&g.snapshot).unwrap();
        assert_eq!(v["blvm_version"], "—");
        assert_eq!(v["blvm_version_seen"]["version"], "v0.1.13");
        assert_eq!(v["blvm_version_seen"]["at"], 1_790_000_400u64);

        // It stays "—" on later failed polls and keeps the same last-seen.
        mark_unreachable(&mut g, "connect 127.0.0.1:18332".into(), false);
        assert_eq!(g.snapshot.blvm_version, "—");
        assert_eq!(g.snapshot.blvm_version_seen.as_ref().unwrap().version, "v0.1.13");
    }

    #[test]
    fn a_frozen_node_shows_no_version_either_but_remembers_the_saved_one() {
        let mut g = LiveState::new("127.0.0.1:18332".into());
        chain_mut(&mut g).last_known = Some(crate::persist::LastKnown {
            at: 1_790_000_000,
            local_height: 5,
            network_height: 10,
            behind: 5,
            sync_pct: "50%".into(),
            sync_pct_num: "50".into(),
            ibd: true,
            network: "test".into(),
            blvm_version: "v0.1.13".into(),
        });
        for _ in 0..FROZEN_AFTER {
            mark_unreachable(&mut g, "RPC 127.0.0.1:18332 frozen".into(), true);
        }
        assert!(g.snapshot.frozen);
        assert_eq!(g.snapshot.blvm_version, "—", "no live source, no version");
        assert_eq!(
            g.snapshot.blvm_version_seen,
            Some(VersionSeen { version: "v0.1.13".into(), at: 1_790_000_000 })
        );
    }

    #[test]
    fn a_hiccup_while_the_node_is_still_considered_up_keeps_the_live_version() {
        let mut g = LiveState::new("127.0.0.1:18332".into());
        live_snapshot(&mut g);
        mark_unreachable(&mut g, "Connection rate limit exceeded for IP 127.0.0.1".into(), true);
        assert!(g.snapshot.connected);
        assert_eq!(g.snapshot.blvm_version, "v0.1.13");
        assert_eq!(g.snapshot.blvm_version_seen, None);
    }

    #[test]
    fn saved_version_comes_back_only_as_last_seen_after_a_console_restart() {
        let mut views = std::collections::BTreeMap::new();
        views.insert(
            "127.0.0.1:18332".to_string(),
            crate::persist::StoredView {
                chain: Some("test".into()),
                last: Some(7),
                items: vec![],
                last_known: Some(crate::persist::LastKnown {
                    at: 1_790_000_000,
                    local_height: 7,
                    network_height: 9,
                    behind: 2,
                    sync_pct: "77%".into(),
                    sync_pct_num: "77".into(),
                    ibd: false,
                    network: "test".into(),
                    blvm_version: "v0.1.13".into(),
                }),
            },
        );
        let mut g = LiveState::new("127.0.0.1:18332".into());
        g.restore(views);
        mark_unreachable(&mut g, "connect 127.0.0.1:18332".into(), false);
        assert_eq!(g.snapshot.blvm_version, "—");
        assert_eq!(
            g.snapshot.blvm_version_seen,
            Some(VersionSeen { version: "v0.1.13".into(), at: 1_790_000_000 })
        );
    }

    #[test]
    fn switching_the_target_clears_the_version_of_the_parked_view() {
        let mut g = LiveState::new("127.0.0.1:18332".into());
        live_snapshot(&mut g);
        g.switch_rpc("127.0.0.1:38332".into());
        assert_eq!(g.snapshot.blvm_version, "—");
        g.switch_rpc("127.0.0.1:18332".into());
        assert_eq!(g.snapshot.blvm_version, "—", "the parked snapshot is a memory, not a live answer");
        assert_eq!(g.snapshot.blvm_version_seen.as_ref().map(|v| v.version.as_str()), Some("v0.1.13"));
    }

    /// A one-connection-at-a-time JSON-RPC server: every request is a batch; `chain` is what it says to
    /// `getblockchaininfo` (None: an error object), `count` to `getblockcount`.
    async fn fake_rpc(chain: Option<serde_json::Value>, count: u64) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { return };
                let chain = chain.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        let mut tmp = [0u8; 4096];
                        let n = match s.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => n,
                        };
                        buf.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&buf).to_string();
                        let Some((head, body)) = text.split_once("\r\n\r\n") else { continue };
                        let want: usize = head
                            .lines()
                            .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap_or(0)))
                            .unwrap_or(0);
                        if body.len() < want {
                            continue;
                        }
                        let req: serde_json::Value = serde_json::from_str(body).unwrap();
                        let out: Vec<serde_json::Value> = req
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|c| match c["method"].as_str().unwrap() {
                                "getblockchaininfo" => match &chain {
                                    Some(v) => serde_json::json!({"id": c["id"], "result": v}),
                                    None => serde_json::json!({"id": c["id"], "error": {"code": -28, "message": "Loading block index"}}),
                                },
                                "getblockcount" => serde_json::json!({"id": c["id"], "result": count}),
                                _ => serde_json::json!({"id": c["id"], "error": {"code": -32601, "message": "no"}}),
                            })
                            .collect();
                        let body = serde_json::to_string(&out).unwrap();
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                        if s.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                        buf.clear();
                    }
                });
            }
        });
        addr
    }

    fn state_with_agent(addr: &str) -> (Shared, crate::agent::Agent) {
        let root = std::env::temp_dir().join(format!("blvm-state-flag-{}", std::process::id()));
        let agent = crate::agent::Agent::new(
            crate::agent::AgentConfig {
                bin: None,
                root,
                network: crate::networks::by_id("regtest"),
                rpc_addr: Some(addr.to_string()),
                assume_valid: None,
                child_env: Default::default(),
                rpc_auth: None,
                p2p_port: None,
            },
            crate::agent::Timings::default(),
        );
        let mut live = LiveState::new(addr.to_string());
        live.console.agent = Some(agent.clone());
        (Arc::new(RwLock::new(live)), agent)
    }

    #[tokio::test]
    async fn s3_any_answer_from_the_rpc_counts_as_answered_for_the_agent() {
        // The RPC server is up but the chain is not ready: getblockchaininfo errors, getblockcount answers.
        let addr = fake_rpc(None, 5).await;
        let (state, agent) = state_with_agent(&addr.to_string());
        assert!(!agent.rpc_flag().load(std::sync::atomic::Ordering::Relaxed));
        poll_once(&state).await;
        assert!(
            agent.rpc_flag().load(std::sync::atomic::Ordering::Relaxed),
            "the node answered the batch; waiting for the whole probe to succeed would kill a slow start"
        );
        let g = state.read().await;
        assert!(!g.snapshot.connected, "no chain info yet: not shown as a connected node");
        drop(g);

        // And with chain info the usual way: connected, flag up.
        let addr = fake_rpc(Some(serde_json::json!({"chain": "regtest", "blocks": 3, "headers": 3})), 3).await;
        let (state, agent) = state_with_agent(&addr.to_string());
        poll_once(&state).await;
        assert!(agent.rpc_flag().load(std::sync::atomic::Ordering::Relaxed));
        assert!(state.read().await.snapshot.connected);
    }

    #[tokio::test]
    async fn s3_nothing_listening_is_not_an_answer() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        drop(l);
        let (state, agent) = state_with_agent(&addr);
        agent.rpc_flag().store(true, std::sync::atomic::Ordering::Relaxed);
        poll_once(&state).await;
        assert!(!agent.rpc_flag().load(std::sync::atomic::Ordering::Relaxed), "a refused connection clears the signal");
    }

    #[test]
    fn old_status_fields_are_all_still_serialized() {
        let v = serde_json::to_value(Snapshot::disconnected("127.0.0.1:48332")).unwrap();
        for k in [
            "connected", "rpc_addr", "health", "health_label", "node_status", "uptime", "blvm_version", "network",
            "sync_pct", "sync_pct_num", "syncing", "behind", "local_height", "network_height", "ibd", "ibd_label",
            "peers", "inbound", "outbound", "accepting_inbound", "network_active", "peer_rows", "self_geo", "banned",
            "disk_used_num", "disk_used_unit", "disk_used_label", "disk_used_bytes", "disk_free_num", "disk_free_unit",
            "disk_free_label", "disk_free_bytes", "disk_total_bytes", "disk_vol_used_label", "feed", "feed_waiting",
            "arrival", "last_check", "ui_version", "ui_uptime", "rpc_failures", "show_manual", "error", "frozen",
            "node_running", "node_busy", "node_power_label", "last_seen",
        ] {
            assert!(v.get(k).is_some(), "status lost field {k}");
        }
        for k in ["mode", "setup_done", "managed", "modules_pages", "blvm_version_seen", "bundle_available"] {
            assert!(v.get(k).is_some(), "status is missing new field {k}");
        }
    }
}
