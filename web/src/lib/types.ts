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
  ui_version: "v0.1.0",
  ui_uptime: "—",
  rpc_failures: 0,
  show_manual: false,
  error: null,
  frozen: false,
  node_running: false,
  node_busy: false,
  node_power_label: "Turn Node On",
});
