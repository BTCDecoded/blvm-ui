import { emptySnapshot, type Managed, type Snapshot } from "./types";

export type Page =
  | "home"
  | "node"
  | "mining"
  | "network"
  | "logs"
  | "insights"
  | "settings";
/** The Settings sub-pages (tabs); `#settings/chain` opens one. */
export type Sub = "node" | "network" | "chain" | "modules" | "console";
export const SUBS: Sub[] = ["node", "network", "chain", "modules", "console"];
const SUB_KEY = "blvm-ui-settings-tab";

/** Last tab the viewer used: a per-viewer convenience only, so every access is guarded. */
function savedSub(): Sub | null {
  try {
    const v = localStorage.getItem(SUB_KEY);
    return SUBS.includes(v as Sub) ? (v as Sub) : null;
  } catch {
    return null;
  }
}

const PAGES: Page[] = [
  "home",
  "node",
  "mining",
  "network",
  "logs",
  "insights",
  "settings",
];

function parseHash(): { page: Page; sub: Sub } {
  const raw = (location.hash || "#home").replace("#", "");
  const [pageName, subName] = raw.split("/");
  let page: Page = PAGES.includes(pageName as Page) ? (pageName as Page) : "home";
  // Peer controls moved from Settings to the Network page.
  if (page === "settings" && subName === "peers") page = "network";
  const sub: Sub = SUBS.includes(subName as Sub) ? (subName as Sub) : (savedSub() ?? "node");
  return { page, sub };
}

const start = parseHash();

export const ui = $state({
  snap: emptySnapshot() as Snapshot,
  live: true,
  page: start.page as Page,
  sub: start.sub as Sub,
  peersMsg: "",
  networkMsg: "",
  /** True after the first status answer; the sidebar stays as before until then. */
  ready: false,
  /** The setup wizard was opened on purpose ("Run setup again", "Use a managed node"). */
  setupOpen: false,
  /** The first-run wizard was skipped or finished in this tab. */
  setupDone: false,
  /** The host refused this address (403) and said why; shown instead of a silently dead page. */
  hostError: "",
});

/** Design A: the page learns the mode from /api/status. Absent => standalone. */
export const isManaged = (s: Snapshot) => s.mode === "managed" && !!s.managed;

/**
 * Design K: the wizard shows instead of the dashboard when the host says setup is not done AND it reports a
 * mode at all. A host without the new fields never triggers it.
 */
export const wizardActive = () =>
  ui.setupOpen || (ui.ready && ui.snap.setup_done === false && ui.snap.mode !== undefined && !ui.setupDone);

/** A network switch owns the node right now (stopping the old one or starting the new one). */
export const switchRunning = (m: Managed | null | undefined): boolean =>
  !!m?.switch && (m.switch.phase === "stopping" || m.switch.phase === "starting");

/** Design L: module-added sidebar pages. Null when the host does not say (sidebar unchanged). */
export const modulePages = (s: Snapshot): string[] | null => (Array.isArray(s.modules_pages) ? s.modules_pages : null);

export function setPage(page: Page, sub?: Sub) {
  ui.page = page;
  if (sub) ui.sub = sub;
  const hash = page === "settings" ? `#settings/${ui.sub}` : `#${page}`;
  if (location.hash !== hash) history.replaceState(null, "", hash);
}

export function setSub(sub: Sub) {
  ui.sub = sub;
  try {
    localStorage.setItem(SUB_KEY, sub);
  } catch {
    // private window or blocked storage: the tab still works, it is just not remembered
  }
  if (ui.page === "settings") {
    const hash = `#settings/${sub}`;
    if (location.hash !== hash) history.replaceState(null, "", hash);
  }
}

// A link or a typed hash (`#settings/chain`) that changes while the page is open. replaceState (our own
// updates) does not fire this.
window.addEventListener("hashchange", () => {
  const h = parseHash();
  ui.page = h.page;
  ui.sub = h.sub;
});

export function applySnap(s: Snapshot) {
  ui.snap = s;
  ui.live = true;
  ui.ready = true;
  ui.hostError = "";
}
