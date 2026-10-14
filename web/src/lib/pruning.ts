import { fmt } from "./format";
import { diskWords, fmtBytes, type NetworkInfo } from "./networks";
import type { Pruning, Snapshot } from "./types";
import { knownMax } from "./validation";

// Block-pruning helpers shared by the setup wizard (step "Blocks to keep"), the Settings Pruning card and the
// dashboard. A pruned node keeps the newest N blocks and deletes older ones after it has verified them. The setting
// is a number of blocks, or null for a full node (every block).

/** The fewest blocks a pruned node may keep (BIP159 promises 288 to peers, and a reorg cannot be deeper). */
export const PRUNE_MIN = 288;
/** The largest number the host accepts. */
export const PRUNE_MAX = 100_000_000;
/** What the slider starts at when a pruned node is chosen. */
export const PRUNE_DEFAULT = 10_000;
/** Slider step (the number box takes any whole number from the minimum). */
export const PRUNE_STEP = 1000;

export type StorageChoice = "full" | "pruned";

/** A usable number of blocks to keep. */
export const validKeep = (n: number | null | undefined): n is number =>
  typeof n === "number" && Number.isInteger(n) && n >= PRUNE_MIN && n <= PRUNE_MAX;

/**
 * The value sent to the host: the number for a pruned node, `null` for a full one, and `undefined` for a pruned
 * choice whose number is not usable. `null` means "back to a full node", so an invalid number must never turn
 * into it: callers disable Save/Next (and send nothing) while this is `undefined`.
 */
export const toKeep = (choice: StorageChoice, n: number | null): number | null | undefined =>
  choice !== "pruned" ? null : validKeep(n) ? n : undefined;

/** Slider maximum: the chain height the page knows for the network (live for the running one), never below the minimum. */
export function keepMax(net: NetworkInfo, snap: Snapshot): { max: number; live: boolean } {
  const k = knownMax(net, snap, undefined);
  return { max: Math.max(PRUNE_MIN, k.max), live: k.live };
}

/** Rough disk use of the newest `n` blocks: `n` times the average block size of a full chain (full size / height).
 *  `null` when the network has no size or height figure. Approximate by nature (recent blocks are larger). */
export function keepBytes(net: NetworkInfo, n: number): number | null {
  if (!net.diskBytes || net.knownHeight <= 0 || !Number.isFinite(n) || n <= 0) return null;
  return Math.min(net.diskBytes, Math.round((n * net.diskBytes) / net.knownHeight));
}

/** What a pruned node needs besides the blocks it keeps: the UTXO set, the block index and the other indexes.
 *  A fixed allowance (the UTXO set of mainnet is far larger than that of the test networks), approximate. */
export const pruneOverheadBytes = (net: NetworkInfo): number => (net.id === "mainnet" ? 12e9 : 2e9);

/** Disk a node needs for this storage choice: the full chain's size, or for a pruned node the newest `n` blocks
 *  (`keepBytes`) plus [`pruneOverheadBytes`], never more than the full chain. 0 when the network has no figure. */
export function neededBytes(net: NetworkInfo, choice: StorageChoice, n: number | null): number {
  if (!net.diskBytes) return 0;
  if (choice !== "pruned" || !validKeep(n)) return net.diskBytes;
  const blocks = keepBytes(net, n);
  return blocks === null ? net.diskBytes : Math.min(net.diskBytes, blocks + pruneOverheadBytes(net));
}

/** The same in words, for the wizard's summary and its low-disk warnings. */
export function neededWords(net: NetworkInfo, choice: StorageChoice, n: number | null): string {
  if (choice !== "pruned" || !validKeep(n)) return diskWords(net);
  const b = neededBytes(net, choice, n);
  return b ? `about ${fmtBytes(b)} (pruned, approximate)` : diskWords(net);
}

/** "about 8.7 GB", or null when unknown. */
export function keepSizeWords(net: NetworkInfo, n: number): string | null {
  const b = keepBytes(net, n);
  return b === null ? null : `about ${fmtBytes(b)}`;
}

/** The summary row text of the wizard. */
export function keepSummary(net: NetworkInfo | undefined, choice: StorageChoice, n: number | null): string {
  if (choice !== "pruned" || !validKeep(n)) return "All blocks (full node)";
  const size = net ? keepSizeWords(net, n) : null;
  return `Newest ${fmt(n)}${size ? ` (${size})` : ""}`;
}

/**
 * What the dashboard says about storage. The node's own answer wins when there is one (a setting changed after
 * the node started only applies at its next start); without an answer the console's saved setting is shown, and
 * labelled as such.
 */
export type StorageView = {
  /** The node keeps only recent blocks. */
  pruned: boolean;
  /** Newest blocks kept: the running node's number when it answers, else the saved setting. */
  keep: number | null;
  /** Where `keep` comes from: the running node, the saved setting, or neither. */
  keepFrom: "node" | "saved" | null;
  /** The console has a setting to compare with (a managed console). Standalone shows the node's figures plain. */
  managed: boolean;
  /** The saved setting differs from what the running node does: it applies at the next start. */
  pending: boolean;
  /** The saved setting, for "restart pending: saved 400" (null: saved as a full node). */
  savedKeep: number | null;
  fromHeight: number | null;
  stored: number | null;
  source: Pruning["source"];
};

export function storageView(p: Pruning | undefined, managed: boolean): StorageView {
  if (!p) {
    return { pruned: false, keep: null, keepFrom: null, managed, pending: false, savedKeep: null, fromHeight: null, stored: null, source: "unknown" };
  }
  // The node answered: `getpruneinfo` (on/off and the number), else `getblockchaininfo.pruned` (on/off only).
  const nodeOn = p.node_enabled ?? p.node_reports_pruned ?? null;
  const saved = p.enabled ? p.keep_blocks : null;
  const pruned = nodeOn ?? p.enabled;
  let keep: number | null = null;
  let keepFrom: StorageView["keepFrom"] = null;
  if (nodeOn === true && p.node_keep_blocks != null) {
    keep = p.node_keep_blocks;
    keepFrom = "node";
  } else if (nodeOn === null || (nodeOn === true && saved !== null)) {
    // The node does not answer (or does not say the number): what is saved.
    keep = saved;
    keepFrom = saved === null ? null : "saved";
  }
  return {
    pruned,
    keep,
    keepFrom,
    managed,
    // Only the managed console has a setting to compare with; the host decides (on/off and the number).
    pending: managed && !!p.pending_restart,
    savedKeep: saved,
    fromHeight: pruned ? p.stored_from_height : null,
    stored: pruned ? p.stored_blocks : null,
    source: p.source,
  };
}

/** "restart pending: saved 400" / "restart pending: saved as a full node". */
export function pendingText(v: StorageView): string {
  return `restart pending: saved ${v.savedKeep ? fmt(v.savedKeep) : "as a full node"}`;
}

/** "Full node", "Pruned · newest 300 blocks (node)" or, while the node does not answer, "(saved)". */
export function storageText(v: StorageView): string {
  if (!v.pruned) return "Full node";
  if (!v.keep) return "Pruned";
  const label = v.managed && v.keepFrom ? ` (${v.keepFrom})` : "";
  return `Pruned · newest ${fmt(v.keep)} blocks${label}`;
}

/**
 * What "Blocks stored" says. The genesis block counts as stored, so a full store with tip `localHeight` holds
 * `localHeight + 1` blocks (a pruned one never more): "All 1,001 blocks", or "2,016 of 110,001 · from height
 * 107,985" (K = blocks stored, H + 1 = every block of the chain so far). Never "1,001 of 1,000".
 */
export function blocksStoredText(v: StorageView, localHeight: number): string {
  const total = Math.max(localHeight, 0) + 1;
  if (!v.pruned) return total > 1 ? `All ${fmt(total)} blocks` : "All blocks";
  if (v.stored === null || v.fromHeight === null) return v.keep ? `Newest ${fmt(v.keep)}` : "Newest blocks only";
  // Still catching up: the node keeps the newest blocks of the chain, and has not reached them yet.
  if (v.stored === 0) return v.keep ? `None yet · keeps the newest ${fmt(v.keep)}` : "None yet";
  return `${fmt(Math.min(v.stored, total))} of ${fmt(total)} · from height ${fmt(v.fromHeight)}`;
}
