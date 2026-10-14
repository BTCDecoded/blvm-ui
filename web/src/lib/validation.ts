import { fmt } from "./format";
import type { NetworkInfo } from "./networks";
import type { AssumeValidDefault, Snapshot } from "./types";

// Assume-valid helpers shared by the setup wizard (step "Validation") and the Settings Validation card.
// The setting travels as text: "default" (the node's own choice for the network), "off" (verify everything) or a
// block height. The host reads a 64-hex block hash from older files and from the older global key only (it refuses
// one in the per-network keys), because the node's hash form does not work yet; this page never offers it.

/** Shown by the wizard step and the Validation card when the stored value for the network is a block hash. */
export const HASH_NOTE = "An older setting uses a block hash, which this page does not offer. Pick an option to replace it.";

export type ValidationChoice = "default" | "off" | "custom";
/** What a stored value means for the page. `hash`: an older value this page does not offer. */
export type ParsedSetting = { choice: ValidationChoice | "hash"; height: number };

/** The largest height the host accepts (nine digits). */
export const MAX_HEIGHT = 999_999_999;
/** Slider step; the number box takes any height. */
export const STEP = 1000;

export function parseSetting(v: string | null | undefined, fallbackHeight: number): ParsedSetting {
  const t = (v ?? "").trim();
  if (t === "" || t === "default") return { choice: "default", height: fallbackHeight };
  if (t === "off" || t === "0") return { choice: "off", height: fallbackHeight };
  if (/^\d{1,9}$/.test(t)) return { choice: "custom", height: Number(t) };
  return { choice: "hash", height: fallbackHeight };
}

/** A usable custom height: a whole number from 0 to nine digits. */
export const validHeight = (h: number | null | undefined): h is number =>
  typeof h === "number" && Number.isInteger(h) && h >= 0 && h <= MAX_HEIGHT;

/** The text sent to the host. A custom height of 0 means "verify everything". */
export function toSetting(choice: ValidationChoice, height: number | null): string {
  if (choice === "default") return "default";
  if (choice === "off") return "off";
  return validHeight(height) && height > 0 ? String(height) : "off";
}

/** Slider maximum: the highest of the live height (current network only), the last height the console saw on this
 *  network, the approximate known height and the node's default height. */
export function knownMax(net: NetworkInfo, snap: Snapshot, def: AssumeValidDefault | undefined): { max: number; live: boolean } {
  const isCurrent = snap.managed?.network === net.id;
  const live = isCurrent && snap.network_height > 0 ? snap.network_height : 0;
  const last = snap.managed?.networks.find((n) => n.id === net.id)?.last_height ?? 0;
  const max = Math.max(live, last || 0, net.knownHeight, def?.height ?? 0);
  return { max, live: live > 0 && live >= max };
}

/** One line for the "Node default" choice. */
export function defaultText(net: NetworkInfo, def: AssumeValidDefault | undefined): string {
  if (def?.forced_off) return `On ${net.label} the console verifies every block (full validation).`;
  if (def && def.height > 0) return `${net.label}: blocks below ${fmt(def.height)} are assumed valid.`;
  return `The node's own choice for ${net.label}.`;
}

/** The summary row text. */
export function summaryText(net: NetworkInfo | undefined, choice: ValidationChoice, height: number | null, def: AssumeValidDefault | undefined): string {
  if (choice === "off") return "Verify everything";
  if (choice === "custom") return validHeight(height) && height > 0 ? `Custom: ${fmt(height)}` : "Verify everything";
  if (def?.forced_off) return "Node default (verify everything)";
  return def && def.height > 0 ? `Node default (${fmt(def.height)})` : net ? "Node default" : "—";
}
