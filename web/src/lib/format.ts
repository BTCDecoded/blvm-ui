export function fmt(n: number | string | undefined | null): string {
  if (n === undefined || n === null || n === "—") return "—";
  const v = typeof n === "number" ? n : Number(n);
  if (!Number.isFinite(v)) return String(n);
  return v.toLocaleString("en-US");
}

export function simpleLabel(s: {
  frozen: boolean;
  connected: boolean;
  node_running?: boolean;
  health: string;
  health_label: string;
}): string {
  if (s.frozen) return "Frozen";
  const up = s.connected || !!s.node_running;
  if (!up) return "Down";
  if (s.health === "good") return "Well";
  return s.health_label || "Healing";
}

export function behindLine(s: {
  connected: boolean;
  node_running?: boolean;
  frozen: boolean;
  ibd: boolean;
  behind: number;
  health: string;
  health_label: string;
  show_manual: boolean;
  last_seen?: number | null;
}): string {
  if (s.frozen) {
    const when = s.last_seen ? ` from ${seenAt(s.last_seen)}` : "";
    return s.ibd
      ? `Node frozen during sync — last known status${when}`
      : `Node is not responding — last known status${when}`;
  }
  if (!s.connected && s.node_running) return "Node is running but not answering yet";
  const up = s.connected || !!s.node_running;
  if (up) {
    if (s.behind === 0) return "At network tip";
    if (s.behind === 1) return "1 block behind network tip";
    return `${fmt(s.behind)} blocks behind network tip`;
  }
  if (s.frozen) {
    return s.ibd
      ? "Node frozen during sync — last known status below"
      : "Node RPC frozen — last known status below";
  }
  if (s.health_label === "Node down" || s.health === "dead") {
    return "Node is down — console is still here";
  }
  if (s.show_manual) return "Can't reach the node";
  return "Looking for the node";
}

export function healthClass(h: string): "good" | "heal" | "dead" {
  return h === "good" || h === "heal" || h === "dead" ? h : "dead";
}

export function friendlyErr(msg: unknown): string {
  const s = String(msg || "");
  if (/^connect /i.test(s) || /connection refused/i.test(s)) {
    return "Can't reach the node.";
  }
  return s || "Could not apply that setting.";
}

/** "8:04 PM" today, else "Sep 21, 8:04 PM". */
export function seenAt(unixSecs: number): string {
  const d = new Date(unixSecs * 1000);
  const today = new Date().toDateString() === d.toDateString();
  return today
    ? d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
    : d.toLocaleString([], { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
}

/** "3 h ago", "2 d ago", "just now". `at` is unix seconds. */
export function agoLabel(at: number): string {
  const secs = Math.max(0, Math.floor(Date.now() / 1000 - at));
  if (secs < 60) return "just now";
  if (secs < 3600) return `${Math.floor(secs / 60)} min ago`;
  if (secs < 86400) return `${Math.floor(secs / 3600)} h ago`;
  return `${Math.floor(secs / 86400)} d ago`;
}

/** "v0.1.13" for "0.1.13" or "v0.1.13"; anything that is not a bare number stays as given. */
export function withV(version: string): string {
  return /^\d/.test(version) ? `v${version}` : version;
}

/**
 * Design M.1: the node's own version, live only. When the node is not answering this is "—" (even if the
 * host still carries an old value over), and the last answer, if the host sends one, goes in `seen`.
 */
export function nodeVersion(s: {
  connected: boolean;
  blvm_version: string;
  blvm_version_seen?: { version: string; at: number } | null;
}): { text: string; seen: string | null } {
  if (s.connected) return { text: s.blvm_version || "—", seen: null };
  const v = s.blvm_version_seen;
  return { text: "—", seen: v ? `last seen ${withV(v.version)}, ${agoLabel(v.at)}` : null };
}

/**
 * The one version a managed console shows for the node program (design M.3), on every card: the release tag
 * ("v0.1.60") or, for a build the console did not install, the binary's own report ("blvm 0.1.0") with the
 * "Custom build" chip next to it. "—" until the host has read it.
 */
export function installedVersion(m: { node_version: string | null; custom_build: boolean } | null | undefined): {
  text: string;
  custom: boolean;
} {
  if (!m?.node_version) return { text: "—", custom: false };
  return m.custom_build
    ? { text: `blvm ${m.node_version.replace(/^blvm\s+/, "")}`, custom: true }
    : { text: withV(m.node_version), custom: false };
}

/**
 * Design M.5: what the running node says about itself over RPC, as a secondary line next to the installed
 * version ("Node reports BitcoinCommons 0.1.13"), live only. Null when the node is not answering or says the
 * same as the installed version.
 */
export function nodeReports(
  s: { connected: boolean; blvm_version: string },
  installed: { text: string },
): string | null {
  if (!s.connected || !s.blvm_version || s.blvm_version === "—") return null;
  if (withV(s.blvm_version) === installed.text) return null;
  return `Node reports BitcoinCommons ${s.blvm_version.replace(/^v/, "")}`;
}

/** "12d 07h 41m" / "07h 41m" (same shape the host uses for node uptime). */
export function durLabel(secs: number): string {
  const t = Math.max(0, Math.floor(secs));
  const d = Math.floor(t / 86400);
  const h = String(Math.floor((t % 86400) / 3600)).padStart(2, "0");
  const m = String(Math.floor((t % 3600) / 60)).padStart(2, "0");
  return d > 0 ? `${d}d ${h}h ${m}m` : `${h}h ${m}m`;
}
