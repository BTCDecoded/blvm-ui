// Mock console host: serves web/dist and a fake /api/status with mainnet-like numbers.
// No node or Rust host needed. Used for demos and product screenshots.
//
//   npm run build && npm run mock            # http://127.0.0.1:3850
//
// Env:
//   MOCK_PORT        listen port (default 3850)
//   MOCK_SCENARIO    "synced" (default) or "ibd"
//   MOCK_TIP         mainnet tip height; skips the mempool.space lookup
//   MOCK_BLOCK_SECS  seconds between new blocks when synced (default 60, 0 = frozen)
//
// Control (used by the screenshot script):
//   GET /__mock?scenario=ibd&live=0    switch scenario, freeze or unfreeze

import { createReadStream, existsSync, statSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, extname, join, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const DIST = join(HERE, "..", "dist");
const PORT = Number(process.env.MOCK_PORT || 3850);
const FALLBACK_TIP = 969_322; // mainnet tip on 2026-09-30
const BLOCK_SECS = Number(process.env.MOCK_BLOCK_SECS ?? 60);
const RPC = "127.0.0.1:8332";

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

const state = {
  scenario: process.env.MOCK_SCENARIO === "ibd" ? "ibd" : "synced",
  live: true,
  tip: FALLBACK_TIP,
  height: FALLBACK_TIP,
  feed: [],
  arrival: [],
  lastStep: Date.now(),
};

const block = (h) => ({ kind: "block", start: h, end: h, label: String(h), key: `block:${h}` });
const chunk = (a, b) => ({ kind: "chunk", start: a, end: b, label: `${a}–${b}`, key: `chunk:${a}:${b}` });

function reset() {
  if (state.scenario === "synced") {
    state.height = state.tip;
    state.feed = Array.from({ length: 24 }, (_, i) => block(state.tip - i));
    // Blocks per minute over the last 12 minutes: sparse, like the real chain.
    state.arrival = [0, 1, 0, 0, 1, 0, 2, 0, 0, 1, 0, 1];
  } else {
    // Mid-IBD: ~84% through, connecting ~40 blocks a second.
    state.height = Math.round(state.tip * 0.8412);
    const r = rng(8333);
    state.feed = [];
    let hi = state.height;
    for (let i = 0; i < 24; i++) {
      const n = 32 + Math.floor(r() * 18);
      state.feed.push(chunk(hi - n + 1, hi));
      hi -= n;
    }
    state.arrival = [2412, 2388, 2461, 2297, 2530, 2476, 2349, 2502, 2418, 2566, 2443, 1180];
  }
  state.lastStep = Date.now();
}

function step() {
  if (!state.live) return;
  const now = Date.now();
  if (state.scenario === "synced") {
    if (!BLOCK_SECS || now - state.lastStep < BLOCK_SECS * 1000) return;
    state.tip += 1;
    state.height = state.tip;
    state.feed = [block(state.height), ...state.feed].slice(0, 24);
    state.arrival[state.arrival.length - 1] += 1;
  } else {
    if (now - state.lastStep < 1000) return;
    const n = 34 + Math.floor(Math.random() * 14);
    const lo = state.height + 1;
    state.height = Math.min(state.tip, state.height + n);
    state.feed = [chunk(lo, state.height), ...state.feed].slice(0, 24);
    state.arrival[state.arrival.length - 1] += n;
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

function snapshot() {
  step();
  const ibd = state.scenario === "ibd";
  const behind = state.tip - state.height;
  const pct = (state.height / state.tip) * 100;
  const pctNum = behind === 0 ? "100" : pct >= 99 ? pct.toFixed(3) : pct.toFixed(1);
  const elapsed = (Date.now() - started) / 1000;

  // Chain size on disk (blocks + chainstate) and the 2 TB volume it lives on.
  const chainBytes = ibd ? 612_400_000_000 : 784_600_000_000;
  const totalBytes = 2_000_000_000_000;
  const freeBytes = ibd ? 1_214_700_000_000 : 1_042_500_000_000;
  const [usedNum, usedUnit, usedLabel] = bytes(chainBytes);
  const [freeNum, freeUnit, freeLabel] = bytes(freeBytes);

  const peers = PEERS;
  const inbound = peers.filter((p) => p.inbound).length;

  return {
    connected: true,
    rpc_addr: RPC,
    health: ibd ? "heal" : "good",
    health_label: ibd ? "Syncing" : "Healthy",
    node_status: ibd ? "Syncing" : "Online",
    uptime: uptime(ibd ? 5 * 3600 + 12 * 60 + elapsed : NODE_UP_SECS + elapsed),
    blvm_version: "v0.1.13",
    network: "main",
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
    ui_version: "v0.1.0",
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

const server = createServer((req, res) => {
  const url = new URL(req.url, "http://x");
  const p = url.pathname;

  if (p === "/api/status") return json(res, snapshot());
  if (p === "/api/connect" || p === "/api/node") return json(res, { ok: true, message: "Mock node.", status: snapshot() });
  if (p === "/api/rpc") return json(res, { ok: true });
  if (p === "/__mock") {
    const sc = url.searchParams.get("scenario");
    if (sc === "synced" || sc === "ibd") {
      state.scenario = sc;
      reset();
    }
    const live = url.searchParams.get("live");
    if (live !== null) state.live = live !== "0";
    return json(res, { scenario: state.scenario, live: state.live, tip: state.tip, height: state.height });
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

state.tip = await liveTip();
reset();
server.listen(PORT, "127.0.0.1", () => {
  console.log(`mock console on http://127.0.0.1:${PORT}  (${state.scenario}, tip ${state.tip.toLocaleString("en-US")})`);
});
