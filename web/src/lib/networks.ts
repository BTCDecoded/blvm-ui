import type { NetworkId } from "./types";

/**
 * One row per network (design C name table, mirrored by `src/networks.rs` once U1 lands).
 * Used by the setup wizard and the managed Network card. The standalone presets in
 * SettingsPanel.svelte keep their own list until the host owns this table.
 */
export type NetworkInfo = {
  id: NetworkId;
  /** `--network` value for the node CLI. */
  cli: string;
  /** `chain` as the node's RPC reports it (and as `/api/status` `network` shows). */
  chain: string;
  label: string;
  /** Plain-language sentence (design C table, "plain words"). */
  plain: string;
  /** Rough time to sync, in words. Only says what the design supports; empty = nothing known. */
  sync: string;
  rpcPort: number;
  p2pPort: number;
  /** Expected disk use in bytes; null where no figure is published (the card then says the size varies). */
  diskBytes: number | null;
  /** How `diskBytes` relates to the real figure. */
  diskKind: "about" | "over" | "under" | "unmeasured";
  /** Shown under "Developer options" in the wizard. */
  developer?: boolean;
};

const GB = 1_000_000_000;

export const NETWORKS: NetworkInfo[] = [
  {
    id: "mainnet",
    cli: "mainnet",
    chain: "main",
    label: "Mainnet",
    plain: "The real Bitcoin network. Days to sync.",
    sync: "Takes days",
    rpcPort: 8332,
    p2pPort: 8333,
    diskBytes: 800 * GB,
    diskKind: "about",
  },
  {
    id: "testnet4",
    cli: "testnet4",
    chain: "testnet4",
    label: "Testnet4",
    plain: "The current public test network. Coins have no value.",
    sync: "",
    rpcPort: 48332,
    p2pPort: 48333,
    diskBytes: null, // no figure published
    diskKind: "unmeasured",
  },
  {
    id: "testnet",
    cli: "testnet",
    chain: "test",
    label: "Testnet3",
    plain: "The older test network. Large and slow.",
    sync: "Slow, it is a large chain",
    rpcPort: 18332,
    p2pPort: 18333,
    diskBytes: 100 * GB,
    diskKind: "over",
  },
  {
    id: "signet",
    cli: "signet",
    chain: "signet",
    label: "Signet",
    plain: "A calm, stable test network. Good first try.",
    sync: "",
    rpcPort: 38332,
    p2pPort: 38333,
    diskBytes: null, // no figure published
    diskKind: "unmeasured",
  },
  {
    id: "regtest",
    cli: "regtest",
    chain: "regtest",
    label: "Regtest",
    plain: "Private test chain for developers.",
    sync: "Starts empty, nothing to download",
    rpcPort: 18443,
    p2pPort: 18444,
    diskBytes: 1 * GB,
    diskKind: "under",
    developer: true,
  },
];

export const networkById = (id: string | null | undefined): NetworkInfo | undefined =>
  NETWORKS.find((n) => n.id === id);

/** Default local RPC address of a network, the key the console history uses. */
export const rpcAddrOf = (n: NetworkInfo): string => `127.0.0.1:${n.rpcPort}`;

/**
 * What to print for the network the host says the node is on: "None chosen" before one is picked, the label
 * for a known id, and "Unknown network" for an id this page does not have (a newer host). Never throws.
 */
export const networkText = (id: string | null | undefined): string =>
  id === null || id === undefined || id === "" ? "None chosen" : (networkById(id)?.label ?? "Unknown network");

/** "Mainnet", or the raw id when unknown. */
export const networkLabel = (id: string | null | undefined): string => networkById(id)?.label ?? String(id ?? "—");

/** Decimal GB/TB like the rest of the console (`1.04 TB`, `412 GB`). */
export function fmtBytes(n: number | null | undefined): string {
  if (n === null || n === undefined || !Number.isFinite(n)) return "—";
  if (n >= 1e12) return `${(n / 1e12).toFixed(2)} TB`;
  if (n >= 1e9) return `${n >= 1e11 ? Math.round(n / 1e9) : (n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${Math.round(n / 1e6)} MB`;
  if (n > 0) return `${Math.max(1, Math.round(n / 1e3))} KB`;
  return "0 B";
}

/** Expected disk use in words: "about 800 GB", "over 100 GB", "under 1 GB", else "varies, check free space". */
export function diskWords(n: NetworkInfo): string {
  const b = fmtBytes(n.diskBytes);
  switch (n.diskKind) {
    case "about":
      return `about ${b}`;
    case "over":
      return `over ${b}`;
    case "under":
      return `under ${b}`;
    default:
      return "size varies, check free space";
  }
}
