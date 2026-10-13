export type Health = "good" | "heal" | "dead";

export type FeedItem = {
  kind: string;
  start: number;
  end: number;
  label: string;
  key: string;
};

export type PeerGeo = {
  lat: number;
  lon: number;
  city?: string;
  country?: string;
  /** Only for this node: "ip" (address lookup) or "timezone" (machine time zone). */
  source?: "ip" | "timezone";
};

export type PeerRow = {
  addr: string;
  inbound: boolean;
  subver: string;
  /** Rough location from an IP lookup, when the host provides one. */
  geo?: PeerGeo | null;
};

export type BanRow = {
  address: string;
  banned_until: number | null;
};

export type Snapshot = {
  connected: boolean;
  rpc_addr: string;
  health: Health | string;
  health_label: string;
  node_status: string;
  uptime: string;
  blvm_version: string;
  network: string;
  sync_pct: string;
  sync_pct_num: string;
  syncing: boolean;
  behind: number;
  local_height: number;
  network_height: number;
  ibd: boolean;
  ibd_label: string;
  peers: number;
  inbound: number;
  outbound: number;
  accepting_inbound: boolean;
  network_active: boolean;
  peer_rows: PeerRow[];
  self_geo?: PeerGeo | null;
  banned: BanRow[];
  disk_used_num: string;
  disk_used_unit: string;
  disk_used_label: string;
  disk_used_bytes: number;
  disk_free_num: string;
  disk_free_unit: string;
  disk_free_label: string;
  disk_free_bytes: number;
  disk_total_bytes: number;
  disk_vol_used_label: string;
  feed: FeedItem[];
  feed_waiting: boolean;
  arrival: number[];
  last_check: string;
  ui_version: string;
  ui_uptime: string;
  rpc_failures: number;
  show_manual: boolean;
  error: string | null;
  frozen: boolean;
  node_running: boolean;
  node_busy: boolean;
  node_power_label: string;
  /** Unix seconds of the node's last answer. */
  last_seen?: number | null;

  // ---- Managed-node additions (design F, M). All optional: an older host sends none of them. ----
  /** Design A/F. Absent => "standalone" (today's behaviour). */
  mode?: Mode;
  /** Design K. Absent => treated as done (no wizard). The wizard needs `setup_done === false` AND `mode` present. */
  setup_done?: boolean;
  /** Design F. Null or absent in standalone mode. */
  managed?: Managed | null;
  /** Design L. Page ids that installed + enabled modules add to the sidebar (e.g. ["mining"]).
   *  Absent => the sidebar stays as before (static list, Mining included). */
  modules_pages?: string[];
  /** Design M.1. Last version the node itself reported, shown only as muted "last seen". */
  blvm_version_seen?: VersionSeen | null;
  /** Design K step 2 / G: a node binary shipped next to the console exists ("Run a node for me" is possible).
   *  Not listed in F; K and G both refer to it as `bundle_available`. */
  bundle_available?: boolean;
};

// ---------------------------------------------------------------------------------------------
// Managed-node API types (design section F). Frozen: the Rust host (U1+) and the mock follow these.
// ---------------------------------------------------------------------------------------------

/** Design A. */
export type Mode = "standalone" | "managed";

/** Design C name table ids. */
export type NetworkId = "mainnet" | "testnet4" | "testnet" | "signet" | "regtest";

/** Design M.1: `{version, at}`; `at` is unix seconds. */
export type VersionSeen = { version: string; at: number };

/** Design B process states. "Syncing" is not one of them (derived from `ibd`/`syncing`). */
export type ProcessState = "stopped" | "starting" | "running" | "stopping" | "crashed";

/** Design F `managed.process`. */
export type ManagedProcess = {
  state: ProcessState;
  pid: number | null;
  /** Unix seconds the current state began (uptime base while running). */
  since: number | null;
  exit_code: number | null;
  /** Host only (absent in the mock): the signal that ended the node, e.g. 9 for SIGKILL. */
  exit_signal?: number | null;
  restarts: number;
  /** Always present (null when there is nothing to say). */
  last_error: string | null;
  /** Host only: the start was refused (or the node stopped) because another program holds the node's RPC or
   *  P2P port; `last_error` names the port and the program. */
  conflict?: boolean;
};

/** Design F `managed.networks[]`. */
export type ManagedNetwork = {
  id: NetworkId;
  label: string;
  /** Default RPC address of this network, e.g. "127.0.0.1:38332". */
  rpc: string;
  /** Allocated bytes under networks/<id>/. */
  bytes: number;
  last_height: number | null;
  /** Unix seconds. */
  last_used: number | null;
  has_data: boolean;
};

/**
 * Install / update job (design D, plus `/api/node/install`). One shape for both routes and for `managed.update`
 * while a job runs. Phases in order: download (bytes/total), verify (SHA-256 against the tag's checksums.sha256),
 * test (the new program is run once), install (files moved, `bin/current` swapped), restart (updates only: the
 * node is restarted on the new program), then `done` or `failed`. `idle` = no job. A host may still send the
 * older names (downloading, verifying, installing, restarting, rolled_back); `normalizeJob` maps them.
 */
export type JobPhase = "idle" | "download" | "verify" | "test" | "install" | "restart" | "done" | "failed";
export type UpdatePhase = JobPhase;
export type InstallJob = {
  phase: JobPhase;
  /** Release tag being installed, e.g. "v0.1.60". */
  tag: string | null;
  /** Downloaded so far / expected total, in bytes (total 0 = unknown). */
  bytes: number;
  total: number;
  /** `failed` only: plain words. */
  error: string | null;
  /** `failed` only: the previous program is back in place (an update that did not start). */
  rolled_back: boolean;
};
export type UpdateChannel = "self" | "store" | "off";
/** `managed.update`: the job fields are optional (an idle host may send only the first four). */
export type ManagedUpdate = {
  available: boolean;
  latest: string | null;
  channel: UpdateChannel;
  phase: JobPhase | LegacyPhase;
  error: string | null;
} & Partial<Pick<InstallJob, "tag" | "bytes" | "total" | "rolled_back">>;
export type LegacyPhase = "downloading" | "verifying" | "installing" | "restarting" | "rolled_back";

/**
 * Design C step 4 / F `managed.switch`. `done` and `failed` stay in the status for 30 s after the switch
 * ends so the page can say how it went; `stopping` and `starting` mean the switch still owns the node.
 */
export type SwitchPhase = "stopping" | "starting" | "done" | "failed";
export type ManagedSwitch = {
  id: string;
  from: NetworkId;
  to: NetworkId;
  phase: SwitchPhase;
  /** Unix seconds. */
  started: number;
  /** `failed` only. */
  error?: string;
  /** `failed` only: the last 50 log lines. */
  log?: string[];
};

/** Design M.2 `managed.installed`. */
export type InstalledBinary = {
  label: string;
  tag: string | null;
  self_report: string | null;
  sha256: string | null;
  source: "release" | "bundle" | "custom";
};

/** Design F. */
export type Managed = {
  process: ManagedProcess;
  /** The network the user chose; null until they have (there is no default network). */
  network: NetworkId | null;
  data_root: string;
  free_bytes: number;
  networks: ManagedNetwork[];
  /** Design M.3: the ONE user-facing version of the binary on disk, the same text on every card: the release
   *  tag with a "v" ("v0.1.60"), or the binary's own report ("blvm 0.1.0") when `custom_build`. Null when
   *  unknown. The node's RPC subversion is `Snapshot.blvm_version` and only ever a secondary line. */
  node_version: string | null;
  custom_build: boolean;
  update: ManagedUpdate;
  switch: ManagedSwitch | null;
  /** `external`: the agent runs in another container (Umbrel). */
  agent: { external: boolean; alive: boolean };
  /** Design M.2, optional extra detail. */
  installed?: InstalledBinary | null;
  /** Host only: `X.Y.Z` from `<bin> version`, null until read. */
  installed_version?: string | null;
  /** Host only: the last 50 log lines of a crashed node (the page also fetches /api/node/logs). */
  crash_log?: string[];
};

/** Design F: POST /api/node in managed mode. Standalone keeps the existing `toggle` action. */
export type NodeAction = "start" | "stop" | "restart" | "toggle";
export type NodeActionRequest = { action: NodeAction };
export type NodeActionResponse = {
  ok: boolean;
  message: string;
  /** The host always sends it (null on success); the mock may leave it out. */
  error?: string | null;
  status?: Snapshot;
};

/**
 * Design F: GET /api/network/preview?to= (POST with `{to}` or `{network}` answers the same). The host decides
 * `allowed`; when it is false `reason` says why in plain words (same network, a switch already running, less than
 * 5 GB free, no network chosen yet).
 */
export type NetworkPreview = {
  /** The network the node is on now; null when none has been chosen. */
  from: NetworkId | null;
  to: NetworkId;
  /** Bytes the current network keeps on disk. */
  from_bytes: number;
  /** Bytes already on disk for the target (0 when none). */
  to_bytes: number;
  /** Where the current network stands (live height, else the last one saved); null when unknown. */
  from_height: number | null;
  /** Last known height of the target, null when it has no data. */
  to_height: number | null;
  /** Unix seconds the target's node last ran, null when never. */
  to_last_used: number | null;
  /** Percent (0-100) when the current network is still in initial block download, else null. */
  from_ibd_pct: number | null;
  free_bytes: number;
  /** Rough size the target needs; null when not measured (design C "measure"). */
  estimate_bytes: number | null;
  /** True when the only thing in the way is a nearly full disk (free space below 5 GB). */
  blocked: boolean;
  warnings: string[];
  allowed: boolean;
  reason: string | null;
};

/** Design F: POST /api/network/switch `{to}` (`{network}` also works). 202 on accept; 409 `{error:"busy"}` when one is running; 400 when refused. */
export type NetworkSwitchRequest = { to: NetworkId };
export type NetworkSwitchResponse = {
  ok: boolean;
  id?: string;
  switch?: ManagedSwitch;
  error?: string;
  /** Host only, with a 409 "busy": why. */
  reason?: string;
  status?: Snapshot;
};

/** Design F: GET /api/node/logs?lines=200&after=<seq>. Standalone hosts answer 404. */
export type LogLine = { seq: number; text: string };
export type LogsResponse = {
  lines: LogLine[];
  /** Null when no network is chosen yet (there is no log file to name). */
  file: string | null;
};

/**
 * GET /api/node/releases. Every key may be missing; `normalizeReleases` fills defaults.
 * `platform` is the asset platform the host can install ("linux-x86_64", "linux-aarch64") or "unsupported".
 */
export type ReleaseInfo = {
  /** What is installed from a release; null when nothing is, or the program is a custom build. */
  installed: { tag: string; installed_at: number | null } | null;
  /** Newest published release; null when it could not be checked. */
  latest: { tag: string; published_at: number | null; notes_url: string | null } | null;
  /** The file this host would download; null when there is none for this platform. */
  asset: { name: string; size: number; platform: string } | null;
  platform: string;
  /** The host will accept /api/node/install and /api/node/update. */
  can_install: boolean;
  /** Plain words for `can_install === false` (updates off, store channel, no build for this platform). */
  reason: string | null;
  /** A newer release than the installed one exists. */
  available: boolean;
  /** GitHub could not be reached (the host has no list, or an old one): "Try again" is worth offering. */
  offline?: boolean;
  /** Optional: same value as `managed.update.channel`. */
  channel?: UpdateChannel;
};

/** POST /api/node/install and /api/node/update: 202 with the job when it started, else `{ok:false,error}`. */
export type JobRequest = { tag?: string; version?: string };
export type JobResponse = { ok?: boolean; error?: string | null; job?: Partial<InstallJob> };
export type OkResponse = { ok: boolean; error?: string | null };

/** Design E: one entry of the module index. Extra optional fields beyond the node's own RegistryEntry. */
export type ModuleIndexEntry = {
  name: string;
  repo?: string;
  description: string;
  title?: string;
  category?: string;
  pages?: string[];
  ports?: number[];
  platforms?: string[];
  needs?: string[];
  /** Not in E: set by the host for modules without a release yet (design K "GridPool: coming soon"). */
  coming_soon?: boolean;
};
export type InstalledModule = {
  name: string;
  version: string;
  enabled: boolean;
  loaded: boolean;
  pages: string[];
};
/** Design F: GET /api/modules. */
export type ModulesResponse = {
  index: ModuleIndexEntry[];
  installed: InstalledModule[];
  can_install: boolean;
  /** Not in F: host platform such as "x86_64-linux", so the UI can grey out entries with no build (design K step 5). */
  platform?: string;
  /** Not in F: plain reason shown when `can_install` is false. */
  reason?: string;
};
export type ModuleAction = "install" | "enable" | "disable" | "remove";
export type ModuleActionRequest = { name: string };

/**
 * GET/POST(PUT) /api/settings: the persisted `console.json` fields, as the host reads them. Every key is
 * optional (the mock sends a few). `rpc` and `network` are null while unset; `data_root` and `node_bin` are
 * the effective values; `env_locked` lists keys an environment variable pins (a change answers 409).
 * A POST/PUT patches only the keys it sends; keys it does not know are ignored.
 */
export type ConsoleSettings = {
  version?: number;
  setup_done?: boolean;
  mode?: Mode;
  /** Standalone target. */
  rpc?: string | null;
  network?: NetworkId | null;
  data_root?: string;
  node_bin?: string | null;
  start_on_launch?: boolean;
  /** Host name for "start the node when the console opens". Read as `start_on_boot ?? start_on_launch ?? true`. */
  start_on_boot?: boolean;
  stop_node_on_exit?: boolean;
  /** Host: same fact as `stop_node_on_exit`, inverted (accepted on write, sent on read). */
  keep_node_running_on_exit?: boolean;
  modules_disabled?: string[];
  updates?: UpdateChannel;
  /** Host only. Network id to the node's RPC `host:port` when not the network's default. */
  rpc_overrides?: Partial<Record<NetworkId, string>>;
  /** Host only. `"default" | "off" | <height> | <64 hex>`; null = the per-network default. */
  assume_valid?: string | null;
  /** Host only. Extra environment for the node; an empty value unsets the variable. */
  child_env?: Record<string, string>;
  /** Host only. Setting names pinned by environment variables. */
  env_locked?: string[];
  /** Host only. Where each pinned-able setting came from: "env" | "file" | "bundle" | "default" | "none". */
  sources?: { mode: string; node_bin: string; data_root: string };
};

/** Design K: POST /api/setup. */
export type SetupRequest = {
  mode: Mode;
  /** standalone: the node's RPC address. */
  rpc?: string;
  network?: NetworkId;
  data_root?: string;
  /** Host only: path of a node program to use instead of the bundled one. */
  node_bin?: string;
  start: boolean;
  /** Host only: alias of `start`. */
  start_now?: boolean;
  stop_on_exit: boolean;
  /** Not used by the host yet (modules arrive with U5); sent so the wizard's choice is not lost. */
  modules: string[];
};
export type SetupResponse = { ok: boolean; error?: string | null; status?: Snapshot };

/** Design F/K: POST /api/setup/test. */
export type SetupTestRequest = { rpc: string; user?: string; pass?: string };
export type SetupTestResponse = {
  ok: boolean;
  chain?: string | null;
  height?: number | null;
  version?: string | null;
  error?: string | null;
};

export const emptySnapshot = (): Snapshot => ({
  connected: false,
  rpc_addr: "127.0.0.1:48332",
  health: "dead",
  health_label: "Looking",
  node_status: "Down",
  uptime: "—",
  blvm_version: "—",
  network: "—",
  sync_pct: "—",
  sync_pct_num: "—",
  syncing: false,
  behind: 0,
  local_height: 0,
  network_height: 0,
  ibd: false,
  ibd_label: "idle",
  peers: 0,
  inbound: 0,
  outbound: 0,
  accepting_inbound: false,
  network_active: false,
  peer_rows: [],
  banned: [],
  disk_used_num: "—",
  disk_used_unit: "",
  disk_used_label: "—",
  disk_used_bytes: 0,
  disk_free_num: "—",
  disk_free_unit: "",
  disk_free_label: "—",
  disk_free_bytes: 0,
  disk_total_bytes: 0,
  disk_vol_used_label: "—",
  feed: [],
  feed_waiting: true,
  arrival: Array(12).fill(0),
  last_check: "just now",
  ui_version: "v0.2.0",
  ui_uptime: "—",
  rpc_failures: 0,
  show_manual: false,
  error: null,
  frozen: false,
  node_running: false,
  node_busy: false,
  node_power_label: "Turn Node On",
});
