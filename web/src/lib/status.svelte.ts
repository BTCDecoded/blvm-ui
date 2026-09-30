import { emptySnapshot, type Snapshot } from "./types";

export type Page =
  | "home"
  | "node"
  | "mining"
  | "network"
  | "logs"
  | "insights"
  | "settings";
export type Sub = "connection" | "peers" | "network";

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
  const sub: Sub = ["connection", "peers", "network"].includes(subName)
    ? (subName as Sub)
    : "connection";
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
});

export function setPage(page: Page, sub?: Sub) {
  ui.page = page;
  if (sub) ui.sub = sub;
  const hash = `#${page}`;
  if (location.hash !== hash) history.replaceState(null, "", hash);
}

export function setSub(sub: Sub) {
  ui.sub = sub;
  if (ui.page === "settings") {
    const hash = `#settings/${sub}`;
    if (location.hash !== hash) history.replaceState(null, "", hash);
  }
}

export function applySnap(s: Snapshot) {
  ui.snap = s;
  ui.live = true;
}
