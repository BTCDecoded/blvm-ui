// Managed-node part of the mock console host (setup, managed node, network switch,
// updates, modules). Everything lives in memory so buttons work:
// start/stop/restart change the process state, a network switch runs its phases, installs add modules,
// an update progresses and completes, logs stream fake lines. server.mjs owns the fake chain; this file
// owns the process, the data layout, modules, updates, settings and the setup wizard.

export const IDS = ["mainnet", "testnet4", "testnet", "signet", "regtest"];

// Mock figures only. `size` = bytes at full sync, `estimate` = what /api/network/preview reports
// (null = "not measured", design C). The estimates are the host's table (src/networks.rs).
export const NETS = {
  mainnet: { label: "Mainnet", chain: "main", rpc: 8332, size: 784.6e9, estimate: 800e9, peers: 12 },
  testnet4: { label: "Testnet4", chain: "testnet4", rpc: 48332, size: 55e9, estimate: null, peers: 6 },
  testnet: { label: "Testnet3", chain: "test", rpc: 18332, size: 140e9, estimate: 100e9, peers: 4 },
  signet: { label: "Signet", chain: "signet", rpc: 38332, size: 20e9, estimate: null, peers: 5 },
  regtest: { label: "Regtest", chain: "regtest", rpc: 18443, size: 0.05e9, estimate: 1e9, peers: 0 },
};

const LINUX = ["x86_64-linux", "aarch64-linux"];
const INDEX = [
  { name: "blvm-stratum-v2", repo: "BTCDecoded/blvm-stratum-v2", title: "Stratum V2 mining", description: "Let mining machines connect to your node.", category: "mining", pages: ["mining"], ports: [3333], platforms: LINUX, version: "0.1.3" },
  { name: "blvm-datum", repo: "BTCDecoded/blvm-datum", title: "DATUM", description: "Mine with the Ocean pool.", category: "mining", pages: ["mining"], ports: [], platforms: LINUX, version: "0.1.5" },
  { name: "blvm-gridpool", repo: "BTCDecoded/blvm-gridpool", title: "GridPool", description: "Pooled mining on your own node.", category: "mining", pages: ["mining"], platforms: [], coming_soon: true, version: "0.0.0" },
  { name: "blvm-zmq", repo: "BTCDecoded/blvm-zmq", title: "ZMQ notifications", description: "Publish block and transaction events to other programs.", category: "tools", pages: [], platforms: LINUX, version: "0.1.2" },
  { name: "blvm-fibre", repo: "BTCDecoded/blvm-fibre", title: "FIBRE relay", description: "Fast block relay between cooperating nodes.", category: "network", pages: [], platforms: LINUX, version: "0.1.4" },
  { name: "blvm-miniscript", repo: "BTCDecoded/blvm-miniscript", title: "Miniscript", description: "Policy and Miniscript helpers for scripts.", category: "tools", pages: [], platforms: LINUX, version: "0.1.1" },
  { name: "blvm-governance", repo: "BTCDecoded/blvm-governance", title: "Governance", description: "Signed governance messages from the node.", category: "tools", pages: [], platforms: LINUX, version: "0.1.2" },
];

const DATA_ROOT = "/Users/demo/Library/Application Support/blvm";

const idleJob = () => ({ phase: "idle", tag: null, bytes: 0, total: 0, error: null, rolled_back: false });
const ASSET_BYTES = 35_200_000; // v0.1.60 linux raw binary, about 35 MB
const LATEST_TAG = "v0.1.60";

export function createManaged(chain) {
  const timeline = [];
  const later = (ms, fn) => timeline.push({ at: Date.now() + ms * mg.slow, fn });
  const secs = () => Math.floor(Date.now() / 1000);

  const mg = {
    // undefined => behave like a host from before the managed work (no new fields at all).
    mode: undefined,
    setupDone: true,
    bundle: false,
    standaloneUp: true,
    rpc: "127.0.0.1:8332",
    external: false,
    agentAlive: true, // external only: false = the agent's heartbeat is gone (knob agent_alive=0)
    offline: false, // knob offline=1: GitHub cannot be reached (the releases answer says so)
    forceChecks: 0, // how many GET /api/node/releases?force=1 arrived
    proc: { state: "stopped", pid: null, since: null, exit_code: null, restarts: 0, last_error: null },
    network: "mainnet", // null until the user picks one, like the host
    future: false, // MOCK-ONLY routes (modules, releases, update) that the Rust host does not have yet
    portTaken: null, // {port, pid, name}: another program holds the node's RPC port
    heights: { mainnet: 0, testnet4: 0, testnet: 0, signet: 0, regtest: 0 },
    lastUsed: {},
    freeBytes: 1_042_500_000_000,
    nodeVersion: "0.1.60",
    custom: false,
    latest: "0.1.60",
    channel: "self",
    platformSupported: true,
    job: idleJob(), // install / update job (design D): the same object feeds /api/node/install and managed.update
    jobEnd: 0, // ms when the job ended (done/failed stay in managed.update for 30 s)
    failVerify: false, // knob: the next job fails its checksum check
    failTest: false, // knob: the next job's test run fails (the new program does not start)
    sw: null, // {id, from, to, phase: stopping|starting|done|failed, started, error?, log?}; done/failed linger 30 s
    stopStuck: false, // knob: the next switch cannot stop the old node (fails in the stopping phase)
    installed: [], // {name, version, enabled}
    canInstall: true,
    platform: "x86_64-linux",
    settings: { version: 1, start_on_boot: true, start_on_launch: true, stop_node_on_exit: true, show_logs: false, updates: "self" },
    lastRunning: secs(),
    logs: [],
    seq: 0,
    failNextStart: false,
    slow: 1, // multiplies every delay (screenshots hold a phase with slow=20)
  };

  // ---- logs --------------------------------------------------------------
  const stamp = () => new Date().toISOString().replace(/\.\d+Z$/, "Z");
  function log(text, level = "INFO") {
    mg.seq += 1;
    mg.logs.push({ seq: mg.seq, text: `${stamp()}  ${level.padEnd(5)} ${text}` });
    if (mg.logs.length > 2000) mg.logs.splice(0, mg.logs.length - 2000);
  }
  const logFile = () => (mg.network ? `${mg.settings.data_root || DATA_ROOT}/networks/${mg.network}/logs/node.log` : null);

  function logTick() {
    if (mg.proc.state !== "running") {
      mg.loggedH = null;
      return;
    }
    chain.step();
    const h = chain.height();
    if (chain.mode() === "ibd") {
      const from = (mg.loggedH ?? h - 40) + 1;
      if (h >= from) log(`[IBD] connected blocks ${from}-${h} (${h - from + 1} blocks), peers ${chain.peers()}`);
    } else if (mg.loggedH !== null && h > mg.loggedH) {
      log(`[TIP_FOLLOW] block ${h} connected: fetch ${20 + Math.floor(Math.random() * 60)} ms, validate ${80 + Math.floor(Math.random() * 120)} ms`);
    } else if (Math.random() < 0.1) {
      log(`peer manager: ${chain.peers()} peers connected`, "DEBUG");
    }
    mg.loggedH = h;
  }

  // ---- process -----------------------------------------------------------
  function netStart(extra) {
    const net = mg.network;
    if (mg.portTaken) {
      // Same as the host: refused before anything is spawned, with the program named.
      const c = mg.portTaken;
      mg.proc = {
        state: "crashed", pid: null, since: Date.now(), exit_code: null, restarts: 0, conflict: true,
        last_error: `The RPC port ${c.port} is in use by pid ${c.pid} (${c.name}). Stop that program first, then start the node again.`,
      };
      return;
    }
    mg.proc = { state: "starting", pid: 4000 + Math.floor(Math.random() * 900), since: Date.now(), exit_code: null, restarts: mg.proc.restarts, last_error: null };
    log(`blvm ${mg.nodeVersion} starting on ${net} (data dir networks/${net}/chain)`);
    log(`rpc listening on 127.0.0.1:${NETS[net].rpc}`);
    later(3000, () => {
      if (mg.proc.state !== "starting") return;
      if (mg.failNextStart) {
        mg.failNextStart = false;
        crash(101, "thread 'main' panicked at storage open: database is locked (mock)");
        return;
      }
      mg.proc = { ...mg.proc, state: "running", since: Date.now() };
      mg.lastRunning = secs();
      log("node is up, RPC answering");
      extra?.();
    });
  }
  function netStop(then) {
    if (!["running", "starting"].includes(mg.proc.state)) return then?.();
    mg.proc = { ...mg.proc, state: "stopping" };
    log("Shutdown signal received", "WARN");
    log("Saving chain state…");
    later(2000, () => {
      mg.heights[mg.network] = chain.height();
      mg.lastUsed[mg.network] = secs();
      mg.proc = { state: "stopped", pid: null, since: Date.now(), exit_code: 0, restarts: mg.proc.restarts, last_error: null };
      log("Clean shutdown");
      then?.();
    });
  }
  function crash(code, msg) {
    mg.proc = { state: "crashed", pid: null, since: Date.now(), exit_code: code, restarts: 3, last_error: msg };
    log(msg, "ERROR");
  }
  function nodeAction(action) {
    const st = mg.proc.state;
    if (swActive()) return { ok: false, message: "", error: "A network switch is running. Wait for it to finish." };
    if (st === "stopping") return { ok: false, message: "", error: "Busy. Wait for the current change to finish." };
    if (action === "toggle") action = st === "running" || st === "starting" ? "stop" : "start";
    if (action === "start") {
      if (!mg.network) return { ok: false, message: "", error: "Choose a network first." };
      if (st === "running" || st === "starting") return { ok: true, message: "The node is already running." };
      netStart();
      if (mg.proc.conflict) return { ok: false, message: "", error: mg.proc.last_error };
      return { ok: true, message: "Starting the node." };
    }
    if (action === "stop") {
      netStop();
      return { ok: true, message: "Stopping the node. Saving chain state first." };
    }
    if (action === "restart") {
      if (!mg.network) return { ok: false, message: "", error: "Choose a network first." };
      netStop(() => netStart());
      return { ok: true, message: "Restarting the node." };
    }
    return { ok: false, message: "", error: `Unknown action ${action}.` };
  }

  // ---- network switch (design C) ----------------------------------------
  // Same shape as the Rust host (src/switch.rs): phases stopping -> starting -> done | failed, `managed.switch`
  // stays for 30 s after the end. Phases advance every 3 s (a stop, then the new node's start).
  let swSeq = 0;
  const swActive = () => !!mg.sw && (mg.sw.phase === "stopping" || mg.sw.phase === "starting");
  const lastLines = (n = 50) => mg.logs.slice(-n).map((l) => l.text);
  function endSwitch(phase, error) {
    const mine = mg.sw;
    mine.phase = phase;
    if (error) {
      mine.error = error;
      mine.log = lastLines();
    }
    later(30000, () => {
      if (mg.sw === mine) mg.sw = null;
    });
  }
  function beginSwitch(to) {
    const from = mg.network;
    const wasUp = ["running", "starting"].includes(mg.proc.state);
    mg.sw = { id: `sw-mock-${++swSeq}`, from, to, phase: wasUp ? "stopping" : "starting", started: secs() };
    const mine = mg.sw;
    log(`Switching network ${from} -> ${to}`);
    if (wasUp) {
      mg.proc = { ...mg.proc, state: "stopping" };
      log("Shutdown signal received", "WARN");
    }
    const startNew = () => {
      mg.network = to;
      chain.load(to, mg.heights[to]);
      mine.phase = "starting";
      netStart();
      const watch = () => {
        if (mg.sw !== mine) return;
        if (mg.proc.state === "running") return endSwitch("done");
        if (mg.proc.state === "crashed") return endSwitch("failed", mg.proc.last_error || "The node stopped unexpectedly.");
        later(300, watch);
      };
      later(300, watch);
    };
    if (!wasUp) return startNew();
    later(3000, () => {
      if (mg.stopStuck) {
        // The old node would not go away: nothing changes, the switch fails.
        mg.stopStuck = false;
        mg.proc = { ...mg.proc, state: "running" };
        return endSwitch("failed", `The node on ${NETS[from].label} did not stop within 200 seconds, so the switch was cancelled and nothing was changed.`);
      }
      mg.heights[from] = chain.height();
      mg.lastUsed[from] = secs();
      startNew();
    });
  }

  // ---- install / update job (design D) ----------------------------------------------------------
  // Phases: download (bytes/total, ~3 s) -> verify -> test -> install (about 6 s in all) -> for an update on a
  // running node: restart -> done. A failure ends in `failed {error, rolled_back}`. rolled_back is true for an
  // update (the previous version is still in place); an install has nothing to go back to.
  const jobActive = () => ["download", "verify", "test", "install", "restart"].includes(mg.job.phase);
  const canInstallNow = () => mg.platformSupported && (mg.channel === "self" || !mg.nodeVersion);
  function installReason() {
    if (!mg.platformSupported) return "No official node program is published for this computer yet.";
    if (mg.channel === "store") return "Updates come from the Umbrel App Store. Open the store to update Commons Node View.";
    if (mg.channel === "off") return "Updates are turned off for this console (BLVM_UI_UPDATES=0).";
    return null;
  }
  function runJob(kind, tag) {
    const ver = tag.replace(/^v/, "");
    const mine = (mg.job = { phase: "download", tag, bytes: 0, total: ASSET_BYTES, error: null, rolled_back: false });
    mg.jobEnd = 0;
    log(`${kind} ${tag}: downloading ${ASSET_BYTES} bytes`);
    const step = (ms, fn) => later(ms, () => mg.job === mine && fn());
    const N = 12;
    for (let i = 1; i <= N; i++) step((3000 / N) * i, () => (mine.bytes = Math.round((ASSET_BYTES * i) / N)));
    const fail = (error) => {
      mine.phase = "failed";
      mine.error = error;
      mine.rolled_back = kind === "update";
      mg.jobEnd = Date.now();
      log(`${kind} ${tag} failed: ${error}`, "ERROR");
    };
    const done = () => {
      mine.phase = "done";
      mg.jobEnd = Date.now();
      log(`${kind} ${tag}: done`);
    };
    step(3000, () => (mine.phase = "verify"));
    if (mg.failVerify) {
      mg.failVerify = false;
      step(3800, () => fail(`The download does not match the checksum published for ${tag} (mock fail_verify).`));
      return;
    }
    step(4000, () => (mine.phase = "test"));
    if (mg.failTest) {
      mg.failTest = false;
      step(5000, () => fail(`${tag} failed to start: version 'GLIBC_2.39' not found (mock fail_test).`));
      return;
    }
    step(5000, () => (mine.phase = "install"));
    step(6000, () => {
      mg.nodeVersion = ver;
      mg.custom = false;
      mg.bundle = true;
      mg.installedAt = secs();
      if (kind === "update" && ["running", "starting"].includes(mg.proc.state)) {
        mine.phase = "restart";
        netStop(() => {
          netStart();
          later(3500, () => mg.job === mine && done());
        });
      } else done();
    });
  }
  const jobBody = () => ({ ...mg.job });
  /** `managed.update`: the job while it runs, and for 30 s after it ends. */
  const updateBody = () => {
    const shown = jobActive() || (mg.jobEnd && Date.now() - mg.jobEnd < 30000) ? mg.job : idleJob();
    return {
      available: !mg.custom && !!mg.nodeVersion && mg.latest !== mg.nodeVersion,
      latest: mg.latest ? `v${mg.latest}` : null,
      channel: mg.channel,
      ...shown,
    };
  };
  const releasesBody = () => {
    const ok = canInstallNow();
    if (mg.offline) {
      // Like the host: 200, nothing known about GitHub, and the reason in plain words.
      return {
        installed: mg.nodeVersion && !mg.custom ? { tag: `v${mg.nodeVersion}`, installed_at: mg.installedAt ?? secs() - 14 * 86400 } : null,
        latest: null,
        asset: null,
        platform: mg.platformSupported ? "linux-x86_64" : "unsupported",
        can_install: false,
        reason: "Could not reach GitHub. Check the internet connection.",
        available: false,
        channel: mg.channel,
        offline: true,
        unavailable_reason: "Could not reach GitHub. Check the internet connection.",
      };
    }
    return {
      installed: mg.nodeVersion && !mg.custom ? { tag: `v${mg.nodeVersion}`, installed_at: mg.installedAt ?? secs() - 14 * 86400 } : null,
      latest: { tag: `v${mg.latest}`, published_at: secs() - 9 * 86400, notes_url: "https://github.com/BTCDecoded/blvm/releases" },
      asset: mg.platformSupported ? { name: `blvm-${mg.latest}-linux-x86_64`, size: ASSET_BYTES, platform: "linux-x86_64" } : null,
      platform: mg.platformSupported ? "linux-x86_64" : "unsupported",
      can_install: ok,
      reason: ok ? null : installReason(),
      available: !mg.custom && !!mg.nodeVersion && mg.latest !== mg.nodeVersion,
      channel: mg.channel,
    };
  };

  // ---- modules (design E, L) --------------------------------------------
  const entry = (name) => INDEX.find((e) => e.name === name);
  function modulePages() {
    // The host has no module store yet (U5), so it sends an empty list: nothing installed, no module pages.
    if (!mg.future) return [];
    const out = [];
    for (const m of mg.installed) {
      if (!m.enabled) continue;
      for (const p of entry(m.name)?.pages ?? []) if (!out.includes(p)) out.push(p);
    }
    return out;
  }
  const modulesBody = () => ({
    index: INDEX.map(({ version, ...e }) => e),
    installed: mg.installed.map((m) => ({
      name: m.name,
      version: m.version,
      enabled: m.enabled,
      loaded: m.enabled && mg.proc.state === "running",
      pages: entry(m.name)?.pages ?? [],
    })),
    can_install: mg.canInstall,
    platform: mg.platform,
    ...(mg.canInstall ? {} : { reason: "This computer cannot download modules (mock: can_install=0)." }),
  });
  function installModule(name) {
    const e = entry(name);
    later(2500, () => {
      if (!mg.installed.some((m) => m.name === name)) mg.installed.push({ name, version: e.version, enabled: true });
      log(`module ${name} installed (${e.version})`);
    });
  }

  // ---- status fields ---------------------------------------------------------
  function bytesOf(id) {
    if (!id || !NETS[id]) return 0;
    const h = id === mg.network ? chain.height() : mg.heights[id];
    return h > 0 ? Math.round(NETS[id].size * Math.min(1, h / chain.tip(id))) : 0;
  }
  function managedBody() {
    const p = mg.proc;
    return {
      process: {
        state: p.state,
        pid: p.pid,
        since: p.since ? Math.floor(p.since / 1000) : null,
        exit_code: p.exit_code,
        exit_signal: p.exit_signal ?? null,
        restarts: p.restarts,
        last_error: p.last_error ?? null,
        conflict: !!p.conflict,
      },
      network: mg.network,
      data_root: mg.settings.data_root || DATA_ROOT,
      free_bytes: mg.freeBytes,
      networks: IDS.map((id) => {
        const bytes = bytesOf(id);
        const h = id === mg.network ? chain.height() : mg.heights[id];
        return {
          id,
          label: NETS[id].label,
          rpc: `127.0.0.1:${NETS[id].rpc}`,
          bytes,
          last_height: h > 0 ? h : null,
          last_used: id === mg.network && ["running", "starting", "stopping"].includes(mg.proc.state) ? secs() : (mg.lastUsed[id] ?? null),
          has_data: bytes > 0,
        };
      }),
      // One text on every card, like the host: the release tag, or the binary's own report for a custom build.
      node_version: mg.nodeVersion ? (mg.custom ? "blvm 0.1.0" : `v${mg.nodeVersion}`) : null,
      custom_build: mg.custom,
      update: updateBody(),
      switch: mg.sw ? { ...mg.sw } : null,
      agent: { external: mg.external, alive: mg.external ? mg.agentAlive : true },
    };
  }

  /** Adds (managed/standalone-with-new-host) fields and the "node is not running" overrides to a snapshot. */
  function decorate(snap) {
    if (mg.mode === undefined) return { ...snap, show_logs: mg.settings.show_logs === true, disk_used_source: "node" }; // old host shape: Logs switch only
    const out = { ...snap, mode: mg.mode, setup_done: mg.setupDone, bundle_available: mg.mode === "managed" ? !!mg.nodeVersion : mg.bundle, modules_pages: modulePages(), show_logs: mg.settings.show_logs === true, disk_used_source: mg.mode === "managed" ? "folder" : "node" };
    if (mg.mode === "managed") {
      out.managed = managedBody();
      const st = mg.proc.state;
      // No network chosen yet (Umbrel-style first run): nothing to show, nothing defaulted.
      out.network = mg.network ? NETS[mg.network].chain : "—";
      out.rpc_addr = mg.network ? `127.0.0.1:${NETS[mg.network].rpc}` : "—";
      if (st !== "running") {
        const label = { starting: "Starting", stopping: "Stopping", stopped: "Node stopped", crashed: "Node crashed" }[st];
        Object.assign(out, {
          connected: false,
          health: st === "crashed" || st === "stopped" ? "dead" : "heal",
          health_label: label,
          node_status: st === "starting" ? "Starting" : st === "stopping" ? "Stopping" : "Down",
          uptime: "—",
          blvm_version: "—",
          // What the node itself reported last: the library version, not the release tag (design M).
          blvm_version_seen: { version: "v0.1.13", at: mg.lastRunning },
          peers: 0,
          inbound: 0,
          outbound: 0,
          accepting_inbound: false,
          network_active: false,
          peer_rows: [],
          node_running: st === "starting" || st === "stopping",
          node_busy: st === "starting" || st === "stopping",
          node_power_label: "Turn Node On",
          error: st === "crashed" ? mg.proc.last_error : null,
          last_seen: mg.lastRunning,
        });
      }
      out.disk_used_bytes = bytesOf(mg.network);
      if (swActive()) {
        out.node_busy = true;
        out.node_power_label = "Switching…";
      }
    } else {
      out.managed = null;
      if (!mg.standaloneUp) {
        Object.assign(out, {
          connected: false,
          health: "dead",
          health_label: "Can't reach node",
          node_status: "Down",
          uptime: "—",
          blvm_version: "—",
          peers: 0,
          inbound: 0,
          outbound: 0,
          peer_rows: [],
          node_running: false,
          node_power_label: "Turn Node On",
          show_manual: true,
        });
      }
    }
    return out;
  }

  // ---- scenarios -----------------------------------------------------------
  function resetAll() {
    timeline.length = 0;
    Object.assign(mg, {
      mode: undefined, setupDone: true, bundle: false, standaloneUp: true, external: false, agentAlive: true, offline: false, forceChecks: 0, future: false, portTaken: null,
      proc: { state: "stopped", pid: null, since: null, exit_code: null, restarts: 0, last_error: null },
      network: "mainnet", heights: { mainnet: 0, testnet4: 0, testnet: 0, signet: 0, regtest: 0 }, lastUsed: {},
      freeBytes: 1_042_500_000_000, nodeVersion: "0.1.60", custom: false, latest: "0.1.60", channel: "self",
      platformSupported: true, sw: null, installed: [], canInstall: true,
      platform: "x86_64-linux", lastRunning: secs(), logs: [], seq: 0, failNextStart: false, stopStuck: false,
      settings: { version: 1, start_on_boot: true, start_on_launch: true, stop_node_on_exit: true, show_logs: false, updates: "self" },
      job: idleJob(), jobEnd: 0, failVerify: false, failTest: false,
    });
  }
  const runningSince = (ms) => (mg.proc = { state: "running", pid: 4242, since: Date.now() - ms, exit_code: null, restarts: 0, last_error: null });
  const MAIN_H = () => 712_004; // mid-sync

  /** Returns false for the legacy scenarios (synced / ibd), which server.mjs handles. */
  function applyScenario(name) {
    const managedNames = ["first-run", "managed-no-binary", "updates-off", "managed-stopped", "managed-running", "managed-switching", "managed-crashed", "port-conflict", "update-available", "modules"];
    if (!managedNames.includes(name)) {
      resetAll();
      return false;
    }
    resetAll();
    mg.mode = "managed";
    mg.heights.mainnet = 712_004;
    mg.heights.signet = 210_000;
    mg.lastUsed = { mainnet: secs() - 3 * 86400, signet: secs() - 86400 };
    log("console started");
    if (name === "first-run") {
      mg.mode = "standalone";
      mg.setupDone = false;
      mg.bundle = true;
      mg.standaloneUp = false;
      mg.heights = { mainnet: 0, testnet4: 0, testnet: 0, signet: 0, regtest: 0 };
      mg.freeBytes = 612_000_000_000;
      chain.load("mainnet", chain.tip("mainnet"));
    } else if (name === "managed-no-binary") {
      // Umbrel-style first run: managed (the node runs elsewhere), setup not done, NO node program on disk yet.
      // The wizard gets its "Get the node" step; the mock simulates the download. `external=0` shows the desktop flow.
      mg.setupDone = false;
      mg.bundle = false;
      mg.external = true;
      mg.network = null;
      mg.nodeVersion = null;
      mg.heights = { mainnet: 0, testnet4: 0, testnet: 0, signet: 0, regtest: 0 };
      mg.lastUsed = {};
      mg.freeBytes = 612_000_000_000;
      mg.proc = { state: "stopped", pid: null, since: Date.now(), exit_code: null, restarts: 0, last_error: null };
      chain.load("mainnet", chain.tip("mainnet"));
    } else if (name === "managed-stopped") {
      // Managed, nothing running and NO network chosen: the host's `managed.network` is null here (for example
      // BLVM_UI_SETUP=skip, or a settings change that cleared it). There is no default network.
      mg.network = null;
      mg.setupDone = true;
      mg.proc = { state: "stopped", pid: null, since: Date.now() - 40 * 60000, exit_code: null, restarts: 0, last_error: null };
      mg.lastRunning = secs() - 40 * 60;
    } else if (name === "port-conflict") {
      mg.network = "testnet4";
      chain.load("testnet4", 52_619);
      mg.heights.testnet4 = 52_619;
      mg.portTaken = { port: NETS.testnet4.rpc, pid: 4242, name: "blvm" };
      netStart(); // refused, like the host
    } else if (name === "managed-crashed") {
      mg.network = "testnet4";
      mg.heights.testnet4 = 52_619;
      chain.load("testnet4", 52_619);
      for (let i = 0; i < 55; i++) log(`[IBD] connected blocks ${52_000 + i * 12}-${52_011 + i * 12} (12 blocks, 11.8 blk/s), peers 5`);
      log("[IBD_RESUME] parallel IBD re-validating from 52619", "WARN");
      crash(101, "thread 'tokio-runtime-worker' panicked at 'called `Result::unwrap()` on an `Err` value' (mock)");
    } else {
      mg.network = "mainnet";
      chain.load("mainnet", MAIN_H());
      runningSince(3 * 3600e3 + 12 * 60e3);
      mg.lastRunning = secs();
      log(`blvm ${mg.nodeVersion} running on mainnet`);
      if (name === "managed-switching") beginSwitch("signet");
      if (name === "update-available") {
        mg.future = true;
        mg.nodeVersion = "0.1.59";
        mg.latest = "0.1.60";
      }
      if (name === "updates-off") {
        // Umbrel: BLVM_UI_UPDATES=0 / store channel. The node is already installed; the card says where to update.
        mg.nodeVersion = "0.1.59";
        mg.latest = "0.1.60";
        mg.channel = "store";
        mg.external = true;
      }
      if (name === "modules") {
        mg.future = true;
        mg.installed = [
          { name: "blvm-stratum-v2", version: "0.1.3", enabled: true },
          { name: "blvm-zmq", version: "0.1.2", enabled: false },
        ];
      }
    }
    return true;
  }

  // ---- http ----------------------------------------------------------------
  // The same rules as `judge` in src/switch.rs: no network chosen, a switch running, the same network, under 5 GB free.
  function preview(to) {
    const from = mg.network;
    const toH = to === from ? chain.height() : mg.heights[to];
    const fromH = !from ? null : mg.proc.state === "running" ? chain.height() : mg.heights[from];
    let reason = null;
    if (!from) reason = "No network has been chosen yet. Pick one in setup first.";
    else if (swActive()) reason = "A network switch is already running.";
    else if (to === from) reason = `Already on ${NETS[to].label}.`;
    else if (mg.freeBytes < 5e9) reason = "Less than 5 GB is free on this disk, so the switch is blocked until you free up space.";
    return {
      from,
      to,
      from_bytes: bytesOf(from),
      to_bytes: bytesOf(to),
      from_height: fromH > 0 ? fromH : null,
      to_height: toH > 0 ? toH : null,
      to_last_used: mg.lastUsed[to] ?? null,
      from_ibd_pct: from && chain.mode() === "ibd" && mg.proc.state === "running" ? Math.round((chain.height() / chain.tip(from)) * 1000) / 10 : null,
      free_bytes: mg.freeBytes,
      estimate_bytes: NETS[to].estimate,
      blocked: !!from && !swActive() && to !== from && mg.freeBytes < 5e9,
      warnings: to === "testnet4" ? ["Testnet4 checks every block in full (assume-valid is off), so it syncs more slowly."] : [],
      allowed: reason === null,
      reason,
    };
  }

  /** Returns true when the request was handled. `json(res, body, code)` and the parsed `body` come from server.mjs. */
  function route(req, res, url, body, json) {
    const p = url.pathname;
    const method = req.method;
    if (!p.startsWith("/api/")) return false;
    const mutating = method === "POST";
    if (mutating && req.headers["x-blvm-ui"] !== "1") {
      json(res, { error: "missing X-BLVM-UI header" }, 403);
      return true;
    }
    const isManaged = mg.mode === "managed";

    if (p === "/api/node" && mutating && isManaged) {
      const r = nodeAction(String(body?.action ?? ""));
      json(res, { message: "", error: null, ...r, status: chain.snapshot() });
      return true;
    }
    const nm = /^\/api\/node\/(start|stop|restart)$/.exec(p);
    if (nm && mutating) {
      // The host also has these three; a standalone host answers ok:false (restart) or does the old thing.
      if (!isManaged) return json(res, { ok: false, message: "", error: "This console is not managing a node.", status: chain.snapshot() }), true;
      const r = nodeAction(nm[1]);
      json(res, { message: "", error: null, ...r, status: chain.snapshot() });
      return true;
    }
    // MOCK-ONLY routes (modules, U5): the Rust host does not have them yet, so outside the scenarios that opt in they are a JSON 404.
    // /api/node/releases|install|update exist in every managed scenario (the host implements them with U4).
    const future = /^\/api\/modules(\/(install|enable|disable|remove))?$/.test(p);
    if (future && !mg.future) {
      json(res, { ok: false, error: "This console has no such API route." }, 404);
      return true;
    }
    if (p === "/api/network/preview" && (method === "GET" || mutating)) {
      const to = method === "GET" ? url.searchParams.get("to") : (body?.to ?? body?.network);
      if (!isManaged) return json(res, { ok: false, error: "This console is not managing a node." }, 404), true;
      if (!NETS[to]) return json(res, { ok: false, error: `Unknown network "${to ?? ""}".` }, 400), true;
      json(res, preview(to));
      return true;
    }
    if (p === "/api/network/switch" && mutating) {
      const to = body?.to ?? body?.network;
      if (!isManaged) return json(res, { ok: false, error: "This console is not managing a node." }, 404), true;
      if (!NETS[to]) return json(res, { ok: false, error: `Unknown network "${to ?? ""}".` }, 400), true;
      const pv = preview(to);
      if (!pv.allowed) {
        // 409 {error:"busy"} while a switch runs; 400 {ok:false,error:<reason>} for the rest (src/switch.rs).
        if (swActive()) return json(res, { ok: false, error: "busy", reason: pv.reason }, 409), true;
        return json(res, { ok: false, error: pv.reason }, 400), true;
      }
      beginSwitch(to);
      json(res, { ok: true, id: mg.sw.id, switch: { ...mg.sw }, status: chain.snapshot() }, 202);
      return true;
    }
    if (p === "/api/node/logs" && method === "GET") {
      if (!isManaged) return json(res, { ok: false, error: "This console is not managing a node." }, 404), true;
      const after = url.searchParams.get("after");
      const n = Math.min(1000, Number(url.searchParams.get("lines") || 200));
      const lines = after !== null && after !== "" ? mg.logs.filter((l) => l.seq > Number(after)) : mg.logs.slice(-n);
      json(res, { lines, file: logFile() });
      return true;
    }
    if (p === "/api/node/releases" && method === "GET") {
      if (!isManaged) return json(res, { error: "not managed" }, 404), true;
      if (url.searchParams.get("force") === "1") {
        mg.forceChecks++;
        log(`releases checked again (force), ${mg.forceChecks} so far`);
      }
      json(res, releasesBody());
      return true;
    }
    if (p === "/api/node/install" && method === "GET") {
      if (!isManaged) return json(res, { error: "not managed" }, 404), true;
      json(res, jobBody());
      return true;
    }
    if ((p === "/api/node/install" || p === "/api/node/update") && mutating) {
      if (!isManaged) return json(res, { error: "not managed" }, 404), true;
      const kind = p.endsWith("install") ? "install" : "update";
      const tag = String(body?.tag ?? (body?.version ? `v${String(body.version).replace(/^v/, "")}` : LATEST_TAG));
      if (!/^v\d+\.\d+\.\d+$/.test(tag)) return json(res, { ok: false, error: "tag must look like v0.1.60" }, 400), true;
      if (jobActive() || swActive()) return json(res, { ok: false, error: "busy" }, 409), true;
      if (!canInstallNow()) return json(res, { ok: false, error: installReason() ?? "Not available on this setup." }, 400), true;
      if (kind === "update" && !mg.nodeVersion) return json(res, { ok: false, error: "No node program is installed yet. Install one first." }, 400), true;
      if (kind === "update" && !mg.custom && tag === `v${mg.nodeVersion}`) return json(res, { ok: false, error: `Already on ${tag}.` }, 400), true;
      runJob(kind, tag);
      json(res, { ok: true, job: jobBody() }, 202);
      return true;
    }
    if (p === "/api/modules" && method === "GET") {
      // The wizard reads the index before the mode is chosen, so any new-style host answers.
      if (mg.mode === undefined) return json(res, { error: "not managed" }, 404), true;
      json(res, modulesBody());
      return true;
    }
    const mm = /^\/api\/modules\/(install|enable|disable|remove)$/.exec(p);
    if (mm && mutating) {
      const name = String(body?.name ?? "");
      const e = entry(name);
      const have = mg.installed.find((m) => m.name === name);
      if (!isManaged || !e) return json(res, { ok: false, error: "unknown module" }, 404), true;
      if (mm[1] === "install") {
        if (!mg.canInstall) return json(res, { ok: false, error: "Installing is not available on this setup." }, 400), true;
        if (e.coming_soon) return json(res, { ok: false, error: "No release yet." }, 400), true;
        if (have) return json(res, { ok: true }), true;
        installModule(name);
        return json(res, { ok: true }, 202), true;
      }
      if (!have) return json(res, { ok: false, error: "not installed" }, 404), true;
      if (mm[1] === "enable") have.enabled = true;
      if (mm[1] === "disable") have.enabled = false;
      if (mm[1] === "remove") mg.installed = mg.installed.filter((m) => m.name !== name);
      log(`module ${name} ${mm[1]}d`);
      json(res, { ok: true });
      return true;
    }
    if (p === "/api/settings") {
      if (mg.mode === undefined) {
        // The old-host shape has only the page switch.
        if (mutating && "show_logs" in (body ?? {})) mg.settings.show_logs = body.show_logs === true;
        return json(res, { version: 1, show_logs: mg.settings.show_logs === true }), true;
      }
      if (mutating) {
        if ("show_logs" in (body ?? {})) mg.settings.show_logs = body.show_logs === true;
        for (const k of ["start_on_launch", "stop_node_on_exit", "updates", "data_root"]) if (k in (body ?? {})) mg.settings[k] = body[k];
        // Skip on Umbrel: only marks setup as done, nothing else moves.
        if ("setup_done" in (body ?? {})) mg.setupDone = !!body.setup_done;
        // The host's name is start_on_boot; the older one stays in step.
        if ("start_on_boot" in (body ?? {})) mg.settings.start_on_launch = mg.settings.start_on_boot = !!body.start_on_boot;
        else if ("start_on_launch" in (body ?? {})) mg.settings.start_on_boot = !!body.start_on_launch;
        if (body?.updates) mg.channel = body.updates;
      }
      json(res, {
        ...mg.settings,
        data_root: mg.settings.data_root || DATA_ROOT,
        keep_node_running_on_exit: mg.settings.stop_node_on_exit === false,
        setup_done: mg.setupDone,
        mode: mg.mode,
        network: mg.network,
        rpc: mg.rpc,
        node_bin: null,
        rpc_overrides: {},
        assume_valid: null,
        child_env: {},
        env_locked: [],
        sources: { mode: "file", node_bin: "none", data_root: "default" },
        modules_disabled: mg.installed.filter((m) => !m.enabled).map((m) => m.name),
      });
      return true;
    }
    if (p === "/api/setup/test" && mutating) {
      const rpc = String(body?.rpc ?? "");
      if (!rpc || /:9999$|bad/.test(rpc)) return json(res, { ok: false, error: `connect ${rpc || "?"}: connection refused` }), true;
      json(res, { ok: true, chain: "testnet4", height: 52_619, version: "0.1.13" });
      return true;
    }
    if (p === "/api/setup" && mutating) {
      if (mg.mode === undefined) return json(res, { error: "not found" }, 404), true;
      const b = body ?? {};
      if (mg.external && b.mode === "standalone") {
        return json(res, { ok: false, error: "This app always runs its own node, so it cannot be switched to watching another one. (The mode is fixed by BLVM_UI_MODE.)" }, 409), true;
      }
      if (b.mode === "managed") {
        if (!NETS[b.network]) return json(res, { ok: false, error: "Pick a network first." }, 400), true;
        if (b.start && !mg.nodeVersion) return json(res, { ok: false, error: "No node program is installed yet." }, 400), true;
        mg.mode = "managed";
        mg.setupDone = true;
        mg.network = b.network;
        if (b.data_root) mg.settings.data_root = b.data_root;
        mg.settings.stop_node_on_exit = b.stop_on_exit !== false;
        chain.load(b.network, mg.heights[b.network]);
        mg.proc = { state: "stopped", pid: null, since: Date.now(), exit_code: null, restarts: 0, last_error: null };
        // Like the host, which ignores `modules` until the module manager exists.
        if (mg.future) for (const name of b.modules ?? []) if (entry(name) && !entry(name).coming_soon) installModule(name);
        if (b.start) netStart();
      } else if (b.mode === "standalone") {
        mg.mode = "standalone";
        mg.setupDone = true;
        mg.standaloneUp = true;
        if (b.rpc) mg.rpc = b.rpc;
      } else {
        return json(res, { ok: false, error: "mode must be managed or standalone" }, 400), true;
      }
      json(res, { ok: true, error: null, status: chain.snapshot() });
      return true;
    }
    return false;
  }

  /** Runtime knobs from /__mock for states that are awkward to reach by clicking. */
  function knobs(q) {
    const g = (k) => q.get(k);
    if (g("channel")) mg.channel = g("channel");
    if (g("platform_supported") !== null) mg.platformSupported = g("platform_supported") !== "0";
    if (g("can_install") !== null) mg.canInstall = g("can_install") !== "0";
    if (g("platform")) mg.platform = g("platform");
    if (g("free_gb") !== null) mg.freeBytes = Number(g("free_gb")) * 1e9;
    if (g("custom") !== null) mg.custom = g("custom") !== "0";
    if (g("fail_start") !== null) mg.failNextStart = g("fail_start") !== "0";
    if (g("fail_verify") !== null) mg.failVerify = g("fail_verify") !== "0";
    if (g("fail_test") !== null) mg.failTest = g("fail_test") !== "0";
    if (g("stop_stuck") !== null) mg.stopStuck = g("stop_stuck") !== "0";
    if (g("show_logs") !== null) mg.settings.show_logs = g("show_logs") !== "0";
    if (g("future") !== null) mg.future = g("future") !== "0";
    if (g("port_taken") !== null) mg.portTaken = g("port_taken") !== "0" ? { port: NETS[mg.network ?? "testnet4"].rpc, pid: 4242, name: "blvm" } : null;
    if (g("agent_alive") !== null) mg.agentAlive = g("agent_alive") !== "0";
    if (g("offline") !== null) mg.offline = g("offline") !== "0";
    if (g("external") !== null) {
      mg.external = g("external") !== "0";
      if (mg.external && mg.mode === "standalone" && !mg.setupDone) {
        mg.mode = "managed"; // Umbrel bundle: managed, no data yet, setup not done
        mg.network = null; // and no network: there is no default
        mg.proc = { state: "stopped", pid: null, since: Date.now(), exit_code: null, restarts: 0, last_error: null };
      }
    }
  }

  setInterval(() => {
    const now = Date.now();
    const due = timeline.filter((t) => t.at <= now).sort((a, b) => a.at - b.at);
    for (const t of due) {
      timeline.splice(timeline.indexOf(t), 1);
      t.fn();
    }
  }, 250);
  setInterval(logTick, 1000);

  return { mg, decorate, route, applyScenario, knobs, isRunning: () => mg.mode !== "managed" || mg.proc.state === "running", managedMode: () => mg.mode === "managed" };
}
