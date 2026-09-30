// Product screenshots of the console, taken against the mock host (mock/server.mjs).
//
//   npm run build && npm run screenshots     # writes ../screenshots/*.png
//
// Uses a Chromium-family browser already on this machine (no browser download).
// Set BROWSER_PATH to pick one explicitly. SCREENSHOT_DIR overrides the output folder.

import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright-core";

const HERE = dirname(fileURLToPath(import.meta.url));
const OUT = resolve(process.env.SCREENSHOT_DIR || join(HERE, "..", "..", "screenshots"));
const PORT = Number(process.env.MOCK_PORT || 3851);
const BASE = `http://127.0.0.1:${PORT}`;

function findBrowser() {
  if (process.env.BROWSER_PATH) return process.env.BROWSER_PATH;
  const candidates = [
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
    "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
  ];
  const pw = join(homedir(), "Library/Caches/ms-playwright");
  if (existsSync(pw)) {
    for (const d of readdirSync(pw).filter((d) => d.startsWith("chromium-"))) {
      candidates.push(join(pw, d, "chrome-mac/Chromium.app/Contents/MacOS/Chromium"));
    }
  }
  const hit = candidates.find((p) => existsSync(p));
  if (!hit) throw new Error("No Chromium-family browser found. Set BROWSER_PATH.");
  return hit;
}

async function startMock() {
  const child = spawn(process.execPath, [join(HERE, "server.mjs")], {
    env: { ...process.env, MOCK_PORT: String(PORT), MOCK_BLOCK_SECS: "0" },
    stdio: ["ignore", "pipe", "inherit"],
  });
  await new Promise((ok, fail) => {
    child.stdout.on("data", (b) => String(b).includes("mock console on") && ok());
    child.on("exit", (code) => fail(new Error(`mock server exited (${code})`)));
  });
  return child;
}

const mock = (q) => fetch(`${BASE}/__mock?${q}`).then((r) => r.json());
const card = (title) => `article.panel:has(h2:text-is("${title}"))`;

// name → [page hash, what to capture]. `null` target = the whole window.
const SYNCED = [
  ["home", "home", null],
  ["insights", "insights", null],
  ["network", "network", null],
  ["settings", "settings", null],
  ["mining", "mining", null],
  ["logs", "logs", null],

  ["home-network-sync", "home", ".sync-card"],
  ["home-p2p-connections", "home", ".p2p-float"],
  ["home-latest-blocks", "home", ".blocks-card"],
  ["insights-system-health", "insights", card("System Health")],
  ["insights-connected-peers", "insights", card("Connected Peers")],
  ["insights-disk-footprint", "insights", card("Disk Footprint")],
  ["insights-top-row", "insights", ".insights-top"],
  ["insights-block-arrival", "insights", ".arrival"],
  ["network-p2p-overview", "network", card("P2P Overview")],
  ["network-connected-peers", "network", card("Connected Peers")],
  ["network-blocked-peers", "network", card("Blocked Peers")],
  ["settings-console-target", "settings", card("Console Target")],
  ["settings-node-process", "settings", card("Node Process")],
  ["settings-about", "settings", card("About")],
  ["mining-stratum-v2", "mining", card("Stratum V2")],
];

const SYNCING = [
  ["ibd-home", "home", null],
  ["ibd-insights", "insights", null],
  ["ibd-home-network-sync", "home", ".sync-card"],
  ["ibd-home-latest-blocks", "home", ".blocks-card"],
  ["ibd-insights-block-arrival", "insights", ".arrival"],
];

const VIEWPORTS = [{ tag: "1920", width: 1920, height: 1080 }];

async function shoot(browser, vp, shots) {
  const ctx = await browser.newContext({
    viewport: { width: vp.width, height: vp.height },
    deviceScaleFactor: 2,
    colorScheme: "dark",
    reducedMotion: "no-preference",
  });
  const page = await ctx.newPage();
  let at = "";
  for (const [name, hash, target] of shots) {
    if (at !== hash) {
      await page.goto(`${BASE}/#${hash}`);
      await page.reload();
      await page.evaluate(() => document.fonts.ready);
      // Globe texture, block tiles and entry animations settle.
      await page.waitForTimeout(hash === "home" ? 3500 : 1500);
      at = hash;
    }
    await page.mouse.move(0, vp.height - 1);
    const file = join(OUT, `${name}-${vp.tag}.png`);
    if (target) {
      const el = page.locator(target).first();
      await el.scrollIntoViewIfNeeded();
      await el.screenshot({ path: file, animations: "allow" });
    } else {
      await page.screenshot({ path: file });
    }
    console.log(`  ${file.replace(OUT + "/", "")}`);
  }
  await ctx.close();
}

const server = await startMock();
let browser;
try {
  rmSync(OUT, { recursive: true, force: true });
  mkdirSync(OUT, { recursive: true });
  browser = await chromium.launch({
    executablePath: findBrowser(),
    headless: true,
    args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--hide-scrollbars"],
  });
  for (const [scenario, shots] of [
    ["synced", SYNCED],
    ["ibd", SYNCING],
  ]) {
    const st = await mock(`scenario=${scenario}&live=0`);
    console.log(`${scenario}: height ${st.height.toLocaleString("en-US")} / tip ${st.tip.toLocaleString("en-US")}`);
    for (const vp of VIEWPORTS) await shoot(browser, vp, shots);
  }
  console.log(`\nSaved to ${OUT}`);
} finally {
  await browser?.close();
  server.kill();
}
