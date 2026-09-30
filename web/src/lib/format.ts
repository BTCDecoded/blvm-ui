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
