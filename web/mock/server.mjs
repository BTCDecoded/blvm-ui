// Mock console host: serves web/dist and a fake /api/status with mainnet-like numbers.
// No node or Rust host needed. Used for demos and product screenshots.
//
//   npm run build && npm run mock            # http://127.0.0.1:3850
//
// Env:
//   MOCK_PORT        listen port (default 3850)
//   MOCK_DIST        folder with the built page (default web/dist)
//   MOCK_SCENARIO    one of the scenarios below (default "synced")
//   MOCK_TIP         mainnet tip height; skips the mempool.space lookup
//   MOCK_BLOCK_SECS  seconds between new blocks when synced (default 60, 0 = frozen)
//
// Scenarios. synced and ibd answer like a host from before the managed-node work (no new fields at all);
// the rest are the managed-node screens with in-memory state,
// so every button works (see managed.mjs):
//   first-run          setup wizard (host not set up, a bundled node program exists)
//   managed-stopped    managed node stopped, NO network chosen yet (the host sends managed.network = null)
//   port-conflict      managed Testnet4 refused to start: another program (pid 4242, "blvm") holds port 48332
//   managed-running    managed Mainnet node mid-sync
//   managed-switching  Mainnet -> Signet switch in progress (phases advance every 3 s; done/failed stay 30 s)
//   managed-crashed    Testnet4 node crashed, last log lines shown
//   managed-no-binary  Umbrel-style first run with NO node program on disk: the wizard has a "Get the node" step; the
//                      mock simulates download, check, test, install over ~6 s, then bundle_available is true
//                      (external=0 shows the desktop flow, platform_supported=0 the unsupported-platform message)
//   update-available   running 0.1.59, 0.1.60 is out; Update runs the same job (download, check, test, install, restart)
//   updates-off        running 0.1.59, channel "store" (Umbrel, BLVM_UI_UPDATES=0): the card says where to update
//   modules            Stratum V2 installed (Mining appears in the sidebar), ZMQ installed but off   (MOCK-ONLY routes)
//
// Host parity. /api/node/releases, /api/node/install (GET job, POST start) and /api/node/update exist in every
// managed scenario. Routes the Rust host does not have yet (/api/modules*; U5) exist in the mock ONLY in the scenarios
// marked MOCK-ONLY above (or with &future=1). In every other scenario they answer a JSON 404 like the real host, and
// modules_pages is the host's empty list (nothing can be installed yet).
//
// Control (used by the screenshot script and for hand-testing):
//   GET /__mock?scenario=ibd&live=0    switch scenario, freeze or unfreeze
//   extra knobs: fail_verify=1 / fail_test=1 (the next install or update job fails its checksum check / its test run;
//                an update then reports rolled_back; fail_test=pagesize: the test run aborts on the memory page size)  page_size=16384 (the node's computer
//                uses 16 KB pages: releases say runs_here=false and the install answers 409 unless the body has force:true, which goes on to the test run and fails there with the page-size text; page_fixed=1: a forced install works; page_size=0 clears it)  channel=self|store|off  platform_supported=0|1  can_install=0|1  platform=aarch64-macos
//                show_logs=1|0 (the Logs page switch; off by default)  future=1|0 (turn the not-yet-in-the-host routes on or off)  port_taken=0|1 (the port-conflict program)
//                slow=20 (stretch every delay)  free_gb=3  custom=1  fail_start=1 (next start crashes)  stop_stuck=1 (next switch cannot stop the old node)  external=1 (with first-run: Umbrel-style)
//                agent_alive=0 (external only: the node agent's heartbeat is gone)  offline=1 (GitHub cannot be reached; Try again
//                asks GET /api/node/releases?force=1, counted in force_checks of this answer)
//                pruned=5000 (block pruning on for the current network; pruned= clears it; a running node is taken to run with it)
//                run_keep=300 (the number of blocks the running node was started with; differs from pruned= -> "restart pending"; run_keep= : every block)
//                has_data=1 (pretend chain data is on disk and that a pruning node worked on it, to try the "delete the chain data" flow)
//                pruned_data=0|1 (whether that data was ever touched by a pruning node: the host's ran_pruned; only 1 makes the way back to a full node a deletion)
//                node_pruned=1 (a standalone node that says it is pruned)  pruned_verified=1 (the stored range is "verified")
//                wiping=1 (the host is deleting the current network's chain data: managed.networks[].wiping, "Deleting the chain data..."; wiping=0 clears it)
//                ibd=1 (first sync: the chain is reloaded at ~84% of the tip)  ibd=0 (synced: reloaded at the tip)  stall=1 (the validated tip stays flat, like a
//                missing block that blocks the queue; stall=0 resumes)  crawl=1 (the tip creeps +1..3 blocks a second and the feed has one tile per height, like the host's
//                feed.rs does for steps under 8; crawl=0 back to the ~40 blocks/s bursts)  tip=1000&height=1000 (a tiny chain, to see "All 1,001 blocks" / "288 of 1,001")

import { createReadStream, existsSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";
import { createManaged, NETS } from "./managed.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const DIST = process.env.MOCK_DIST ? normalize(process.env.MOCK_DIST) : join(HERE, "..", "dist");
const PORT = Number(process.env.MOCK_PORT || 3850);
const FALLBACK_TIP = 971_325; // mainnet tip on 2026-10-13 (mempool.space)
const BLOCK_SECS = Number(process.env.MOCK_BLOCK_SECS ?? 60);

// Deterministic randomness so every run (and every screenshot) looks the same.
function rng(seed) {
  let a = seed >>> 0;
  return () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

// ---- peers ---------------------------------------------------------------

const PLACES = [
  ["Ashburn", "United States", 39.04, -77.49],
  ["New York", "United States", 40.71, -74.01],
  ["San Francisco", "United States", 37.77, -122.42],
  ["Dallas", "United States", 32.78, -96.8],
  ["Toronto", "Canada", 43.65, -79.38],
  ["Frankfurt", "Germany", 50.11, 8.68],
  ["Amsterdam", "Netherlands", 52.37, 4.9],
  ["Paris", "France", 48.86, 2.35],
  ["London", "United Kingdom", 51.51, -0.13],
  ["Helsinki", "Finland", 60.17, 24.94],
  ["Stockholm", "Sweden", 59.33, 18.07],
  ["Zurich", "Switzerland", 47.38, 8.54],
  ["Prague", "Czechia", 50.08, 14.44],
  ["Warsaw", "Poland", 52.23, 21.01],
  ["Madrid", "Spain", 40.42, -3.7],
  ["Tokyo", "Japan", 35.68, 139.69],
  ["Seoul", "South Korea", 37.57, 126.98],
  ["Singapore", "Singapore", 1.35, 103.82],
  ["Sydney", "Australia", -33.87, 151.21],
  ["São Paulo", "Brazil", -23.55, -46.63],
  ["Buenos Aires", "Argentina", -34.6, -58.38],
  ["San Salvador", "El Salvador", 13.69, -89.22],
  ["Mexico City", "Mexico", 19.43, -99.13],
  ["Johannesburg", "South Africa", -26.2, 28.05],
  ["Lagos", "Nigeria", 6.52, 3.38],
  ["Nairobi", "Kenya", -1.29, 36.82],
  ["Mumbai", "India", 19.08, 72.88],
];

const SUBVERS = [
  "/Satoshi:30.0.0/",
  "/Satoshi:30.0.0/",
  "/Satoshi:30.1.0/",
  "/Satoshi:29.1.0/",
  "/Satoshi:29.0.0/",
  "/Satoshi:28.1.0/",
  "/Satoshi:27.2.0/",
  "/Satoshi:29.1.0/Knots:20250914/",
  "/BitcoinCommons:0.1.13/",
  "/btcwire:0.5.0/btcd:0.24.2/",
];

function makePeers() {
  const r = rng(21_000_000);
  const pick = (xs) => xs[Math.floor(r() * xs.length)];
  const ipv4 = () => {
    const first = pick([5, 23, 37, 45, 51, 62, 78, 85, 91, 95, 104, 136, 144, 157, 172, 185, 193, 195, 213]);
    return [first, 1 + Math.floor(r() * 254), Math.floor(r() * 256), 1 + Math.floor(r() * 254)].join(".");
  };

  const rows = [];
  // 8 full-relay outbound, 4 inbound.
  for (let i = 0; i < 8; i++) rows.push({ inbound: false });
  for (let i = 0; i < 4; i++) rows.push({ inbound: true });
  return rows.map((row, i) => {
    const [city, country, lat, lon] = pick(PLACES);
    return {
      addr: row.inbound ? `${ipv4()}:${40000 + Math.floor(r() * 25000)}` : `${ipv4()}:8333`,
      inbound: row.inbound,
      subver: pick(SUBVERS),
      geo: { lat: lat + (r() - 0.5) * 1.6, lon: lon + (r() - 0.5) * 1.6, city, country },
    };
  });
}

const PEERS = makePeers();
const BANNED = [
  { address: "185.220.101.0/24", banned_until: null },
  { address: "45.155.205.233/32", banned_until: 0 },
];

// ---- chain state -----------------------------------------------------------

const started = Date.now();
const NODE_UP_SECS = 12 * 86400 + 7 * 3600 + 41 * 60; // node has been up ~12 days
const UI_UP_SECS = 3 * 86400 + 2 * 3600 + 18 * 60;

// Tip heights as read from mempool.space on 2026-10-13 (the same figures as `knownHeight` in
// `src/lib/networks.ts`); mainnet's is read again at start. Regtest is invented.
const TIPS = { mainnet: FALLBACK_TIP, testnet4: 156_470, testnet: 5_158_765, signet: 326_255, regtest: 150 };

const state = {
  scenario: process.env.MOCK_SCENARIO || "synced",
  live: true,
  net: "mainnet",
  chainMode: "synced", // "synced" | "ibd": the shape of the chain data below
  tip: FALLBACK_TIP,
  height: FALLBACK_TIP,
  feed: [],
  arrival: [],
  lastStep: Date.now(),
  stall: false, // knob stall=1: while syncing, the validated tip does not move
  crawl: false, // knob crawl=1: while syncing, +1..3 blocks a second with one feed tile per height (the host's feed.rs below 8)
};

const block = (h) => ({ kind: "block", start: h, end: h, label: String(h), key: `block:${h}` });
const chunk = (a, b) => ({ kind: "chunk", start: a, end: b, label: `${a}–${b}`, key: `chunk:${a}:${b}` });

/** Loads a network's chain view at `height`: synced when it is at the tip, else mid-IBD. */
function loadChain(net, height) {
  state.net = net;
  state.tip = TIPS[net];
  state.height = Math.min(height, state.tip);
  state.chainMode = state.tip - state.height <= 2 ? "synced" : "ibd";
  state.arrival = Array(12).fill(0);
  state.feed = [];
  if (state.chainMode === "synced") {
    state.feed = Array.from({ length: Math.min(24, state.height + 1) }, (_, i) => block(state.height - i));
    // Blocks per minute over the last 12 minutes: sparse, like the real chain.
    state.arrival = [0, 1, 0, 0, 1, 0, 2, 0, 0, 1, 0, 1];
  } else if (state.height > 0) {
    // Mid-IBD: connecting ~40 blocks a second.
    const r = rng(8333);
    let hi = state.height;
    for (let i = 0; i < 24 && hi > 0; i++) {
      const n = Math.min(hi, 32 + Math.floor(r() * 18));
      state.feed.push(chunk(hi - n + 1, hi));
      hi -= n;
    }
    state.arrival = [2412, 2388, 2461, 2297, 2530, 2476, 2349, 2502, 2418, 2566, 2443, 1180];
  }
  state.lastStep = Date.now();
}

function reset() {
  if (state.scenario === "ibd") loadChain("mainnet", Math.round(FALLBACK_TIP * 0.8412));
  else loadChain("mainnet", FALLBACK_TIP);
}

function step() {
  if (!state.live || !managed.isRunning()) return;
  const now = Date.now();
  if (state.chainMode === "synced") {
    if (!BLOCK_SECS || now - state.lastStep < BLOCK_SECS * 1000) return;
    state.tip += 1;
    TIPS[state.net] = state.tip;
    state.height = state.tip;
    state.feed = [block(state.height), ...state.feed].slice(0, 24);
    state.arrival[state.arrival.length - 1] += 1;
  } else {
    if (now - state.lastStep < 1000) return;
    if (state.stall) {
      state.lastStep = now;
      return;
    }
    const n = state.crawl ? 1 + Math.floor(Math.random() * 3) : 34 + Math.floor(Math.random() * 14);
    const lo = state.height + 1;
    state.height = Math.min(state.tip, state.height + n);
    // Like feed.rs: a step under 8 blocks is one tile per height (newest first), a bigger one a single "from-to" tile.
    const fresh = state.crawl ? Array.from({ length: state.height - lo + 1 }, (_, i) => block(state.height - i)) : [chunk(lo, state.height)];
    state.feed = [...fresh, ...state.feed].slice(0, 24);
    state.arrival[state.arrival.length - 1] += state.height - lo + 1;
    if (state.tip - state.height <= 2) state.chainMode = "synced";
  }
  state.lastStep = now;
}

// ---- snapshot --------------------------------------------------------------

function uptime(secs) {
  const d = Math.floor(secs / 86400);
  const h = String(Math.floor((secs % 86400) / 3600)).padStart(2, "0");
  const m = String(Math.floor((secs % 3600) / 60)).padStart(2, "0");
  return d > 0 ? `${d}d ${h}h ${m}m` : `${h}h ${m}m`;
}

function bytes(n) {
  const TB = 1e12;
  const GB = 1e9;
  if (n >= TB) {
    const num = (n / TB).toFixed(2);
    return [num, "TB", `${num} TB`];
  }
  const num = (n / GB).toFixed(1);
  return [num, "GB", `${num} GB`];
}

function baseSnapshot() {
  step();
  const ibd = state.chainMode === "ibd";
  const behind = state.tip - state.height;
  const pct = (state.height / state.tip) * 100;
  const pctNum = behind === 0 ? "100" : pct >= 99 ? pct.toFixed(3) : pct.toFixed(1);
  const elapsed = (Date.now() - started) / 1000;
  const mg = managed.mg;
  const isManaged = managed.managedMode();

  // Chain size on disk (blocks + chainstate) and the 2 TB volume it lives on.
  const legacyMainnet = state.net === "mainnet" && !isManaged;
  const chainBytes = legacyMainnet ? (ibd ? 612_400_000_000 : 784_600_000_000) : Math.round(NET_SIZE[state.net] * Math.min(1, state.height / state.tip));
  const totalBytes = 2_000_000_000_000;
  const freeBytes = isManaged || mg.mode === "standalone" ? mg.freeBytes : ibd ? 1_214_700_000_000 : 1_042_500_000_000;
  const [usedNum, usedUnit, usedLabel] = bytes(chainBytes);
  const [freeNum, freeUnit, freeLabel] = bytes(freeBytes);

  const peers = PEERS.slice(0, state.net === "mainnet" ? PEERS.length : NET_PEERS[state.net]);
  const inbound = peers.filter((p) => p.inbound).length;

  return {
    connected: true,
    rpc_addr: `127.0.0.1:${NET_RPC[state.net]}`,
    health: ibd ? "heal" : "good",
    health_label: ibd ? "Syncing" : "Healthy",
    node_status: ibd ? "Syncing" : "Online",
    uptime: uptime(ibd ? 5 * 3600 + 12 * 60 + elapsed : NODE_UP_SECS + elapsed),
    blvm_version: "v0.1.13",
    network: NET_CHAIN[state.net],
    sync_pct: `${pctNum}%`,
    sync_pct_num: pctNum,
    syncing: ibd,
    behind,
    local_height: state.height,
    network_height: state.tip,
    ibd,
    ibd_label: ibd ? "active" : "idle",
    peers: peers.length,
    inbound,
    outbound: peers.length - inbound,
    accepting_inbound: true,
    network_active: true,
    peer_rows: peers,
    self_geo: { lat: 52.52, lon: 13.405, city: "Berlin", country: "Germany", source: "ip" },
    banned: BANNED.map((b) => ({
      ...b,
      banned_until: b.banned_until === 0 ? Math.floor(Date.now() / 1000) + 19 * 3600 : b.banned_until,
    })),
    disk_used_num: usedNum,
    disk_used_unit: usedUnit,
    disk_used_label: usedLabel,
    disk_used_bytes: chainBytes,
    disk_free_num: freeNum,
    disk_free_unit: freeUnit,
    disk_free_label: freeLabel,
    disk_free_bytes: freeBytes,
    disk_total_bytes: totalBytes,
    disk_vol_used_label: bytes(totalBytes - freeBytes)[2],
    feed: state.feed,
    feed_waiting: false,
    arrival: state.arrival,
    last_check: "just now",
    ui_version: "v0.2.0",
    ui_uptime: uptime(UI_UP_SECS + elapsed),
    rpc_failures: 0,
    show_manual: false,
    error: null,
    frozen: false,
    node_running: true,
    node_busy: false,
    node_power_label: "Turn Node Off",
    last_seen: Math.floor(Date.now() / 1000),
  };
}

// Legacy scenarios (synced, ibd) get exactly the old payload: no managed-node fields at all.
const snapshot = () => managed.decorate(baseSnapshot());

const NET_SIZE = Object.fromEntries(Object.entries(NETS).map(([k, v]) => [k, v.size]));
const NET_RPC = Object.fromEntries(Object.entries(NETS).map(([k, v]) => [k, v.rpc]));
const NET_CHAIN = Object.fromEntries(Object.entries(NETS).map(([k, v]) => [k, v.chain]));
const NET_PEERS = Object.fromEntries(Object.entries(NETS).map(([k, v]) => [k, v.peers]));

const managed = createManaged({
  tip: (id) => TIPS[id],
  height: () => state.height,
  mode: () => state.chainMode,
  peers: () => (state.net === "mainnet" ? PEERS.length : NET_PEERS[state.net]),
  load: loadChain,
  step,
  snapshot: () => snapshot(),
});

// ---- http ------------------------------------------------------------------

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".svg": "image/svg+xml",
  ".woff2": "font/woff2",
  ".woff": "font/woff",
  ".ttf": "font/ttf",
  ".json": "application/json",
};

function json(res, body, code = 200) {
  res.writeHead(code, { "content-type": "application/json", "cache-control": "no-store" });
  res.end(JSON.stringify(body));
}

function serveFile(res, path) {
  res.writeHead(200, { "content-type": TYPES[extname(path)] || "application/octet-stream" });
  createReadStream(path).pipe(res);
}

const SCENARIOS = ["synced", "ibd", "first-run", "managed-stopped", "managed-running", "managed-switching", "managed-crashed", "port-conflict", "update-available", "updates-off", "managed-no-binary", "modules"];

function setScenario(name) {
  state.scenario = name;
  state.stall = false;
  state.crawl = false;
  if (!managed.applyScenario(name)) reset();
}

/** Chain knobs (ibd, stall, crawl, tip, height); the managed ones are in managed.mjs. */
function chainKnobs(q) {
  const g = (k) => q.get(k);
  if (g("stall") !== null) state.stall = g("stall") !== "0";
  if (g("crawl") !== null) state.crawl = g("crawl") !== "0";
  if (g("tip") !== null && Number(g("tip")) > 0) TIPS[state.net] = Number(g("tip"));
  if (g("height") !== null && Number(g("height")) >= 0) loadChain(state.net, Number(g("height")));
  else if (g("ibd") === "1" && state.chainMode !== "ibd") loadChain(state.net, Math.round(TIPS[state.net] * 0.8412));
  else if (g("ibd") === "0") loadChain(state.net, TIPS[state.net]);
  else if (g("tip") !== null) loadChain(state.net, Math.min(state.height, TIPS[state.net]));
  // A freshly loaded syncing chain has bursty tiles; crawl mode wants per-height ones from the start.
  if (state.crawl && state.chainMode === "ibd" && state.feed[0]?.kind === "chunk") state.feed = [block(state.height), ...state.feed.slice(1)];
}

async function readBody(req) {
  const chunks = [];
  for await (const c of req) chunks.push(c);
  try {
    return JSON.parse(Buffer.concat(chunks).toString() || "{}");
  } catch {
    return {};
  }
}

const server = createServer(async (req, res) => {
  const url = new URL(req.url, "http://x");
  const p = url.pathname;

  if (p === "/__mock") {
    const sc = url.searchParams.get("scenario");
    if (url.searchParams.get("slow")) managed.mg.slow = Number(url.searchParams.get("slow")) || 1;
    if (SCENARIOS.includes(sc)) setScenario(sc);
    managed.knobs(url.searchParams);
    chainKnobs(url.searchParams);
    const live = url.searchParams.get("live");
    if (live !== null) state.live = live !== "0";
    return json(res, { scenario: state.scenario, live: state.live, tip: state.tip, height: state.height, scenarios: SCENARIOS, force_checks: managed.mg.forceChecks });
  }
  if (p.startsWith("/api/")) {
    const body = req.method === "POST" ? await readBody(req) : undefined;
    if (managed.route(req, res, url, body, json)) return;
    if (p === "/api/status") return json(res, snapshot());
    if (p === "/api/connect" || p === "/api/node") return json(res, { ok: true, message: "Mock node.", status: snapshot() });
    if (p === "/api/rpc") return json(res, { ok: true });
    return json(res, { error: "not found" }, 404);
  }

  const file = normalize(join(DIST, decodeURIComponent(p)));
  if (file.startsWith(DIST) && existsSync(file) && statSync(file).isFile()) return serveFile(res, file);
  serveFile(res, join(DIST, "index.html"));
});

async function liveTip() {
  if (process.env.MOCK_TIP) return Number(process.env.MOCK_TIP);
  try {
    const r = await fetch("https://mempool.space/api/blocks/tip/height", { signal: AbortSignal.timeout(3000) });
    const n = Number(await r.text());
    if (Number.isFinite(n) && n > FALLBACK_TIP - 1000) return n;
  } catch {}
  return FALLBACK_TIP;
}

if (!existsSync(join(DIST, "index.html"))) {
  console.error("web/dist is missing. Run `npm run build` first.");
  process.exit(1);
}

TIPS.mainnet = await liveTip();
state.tip = TIPS.mainnet;
setScenario(SCENARIOS.includes(state.scenario) ? state.scenario : "synced");
server.listen(PORT, "127.0.0.1", () => {
  console.log(`mock console on http://127.0.0.1:${PORT}  (${state.scenario}, tip ${state.tip.toLocaleString("en-US")})`);
});
