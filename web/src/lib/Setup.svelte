<script lang="ts">
  import { onMount, tick } from "svelte";
  import {
    ApiError,
    fetchModules,
    fetchStatus,
    getInstallJob,
    getReleases,
    getSettings,
    installNode,
    isMissing,
    jobRunning,
    normalizeJob,
    patchSettings,
    runSetup,
    testConnection,
  } from "./api";
  import { fmt, friendlyErr, installedVersion, withV } from "./format";
  import Icon from "./Icon.svelte";
  import InstallProgress from "./InstallProgress.svelte";
  import { moduleReason, moduleTitle } from "./modules";
  import { diskWords, fmtBytes, NETWORKS, networkById } from "./networks";
  import { applySnap, setPage, ui } from "./status.svelte";
  import type { InstallJob, ModulesResponse, NetworkId, ReleaseInfo, SetupRequest, SetupTestResponse } from "./types";

  // Design K: first-run wizard. Shown instead of the dashboard (see App.svelte), skippable, never deletes data.
  type StepId = "welcome" | "mode" | "network" | "get" | "storage" | "modules" | "summary";
  type Mode = "managed" | "standalone";

  const s = $derived(ui.snap);
  const rerun = $derived(s.setup_done !== false); // opened on purpose, not by a fresh install
  const external = $derived(!!s.managed?.agent.external); // Umbrel bundle: mode and data root are fixed
  const canManage = $derived(!!s.bundle_available || s.mode === "managed");
  const freeBytes = $derived((s.managed?.free_bytes || s.disk_free_bytes) ?? 0);

  const w = $state({
    step: 0,
    mode: null as Mode | null,
    rpc: "",
    user: "",
    pass: "",
    network: null as NetworkId | null,
    dataRoot: "",
    start: true,
    stopOnExit: true,
    modules: [] as string[],
  });

  let test = $state<SetupTestResponse | null>(null);
  let testing = $state(false);
  let mods = $state<ModulesResponse | null>(null);
  let modsErr = $state("");
  let saving = $state(false);
  let err = $state("");

  // Managed, but the host has no node program yet (the Umbrel / app-store case): the wizard gets a "Get the node"
  // step after the network. Decided once when the wizard opens, so it does not vanish when the download ends
  // and the status flips `bundle_available` to true.
  let needGet = $state(false);

  const steps = $derived.by<StepId[]>(() => {
    const get: StepId[] = needGet ? ["get"] : [];
    if (external) return ["welcome", "network", ...get, "summary"];
    return w.mode === "managed"
      ? ["welcome", "mode", "network", ...get, "storage", "modules", "summary"]
      : ["welcome", "mode", "summary"];
  });
  const cur = $derived(steps[Math.min(w.step, steps.length - 1)]);
  const net = $derived(networkById(w.network));
  const tooSmall = $derived(!!net?.diskBytes && freeBytes > 0 && freeBytes < net.diskBytes);

  onMount(() => {
    needGet = s.mode === "managed" && s.bundle_available === false;
    // Defaults, then whatever the host already knows (re-run is prefilled).
    w.mode = s.mode === "managed" || (canManage && !rerun) ? "managed" : "standalone";
    w.rpc = s.rpc_addr || "127.0.0.1:48332";
    // No default network on a fresh install: the user must pick one. A re-run is prefilled with the choice
    // already made (below), which is the user's own pick, not a suggestion.
    if (rerun && s.managed) w.network = s.managed.network ?? null;
    getSettings()
      .then((c) => {
        if (c.mode && rerun) w.mode = c.mode;
        if (c.rpc) w.rpc = c.rpc;
        if (c.network && rerun) w.network = c.network;
        if (c.data_root) w.dataRoot = c.data_root;
        if (c.stop_node_on_exit !== undefined) w.stopOnExit = c.stop_node_on_exit;
      })
      .catch(() => {});
  });

  // Each step moves focus to its heading, so a keyboard or screen-reader user lands on the new step instead
  // of on the page behind it (the buttons that were focused are gone).
  let stepEl: HTMLDivElement | undefined = $state();
  let focusedStep = "";
  $effect(() => {
    const id = cur;
    if (!stepEl || id === focusedStep) return;
    focusedStep = id;
    void tick().then(() => stepEl?.querySelector<HTMLElement>(".step-title")?.focus({ preventScroll: true }));
  });

  // Modules are only fetched when that step is reached.
  $effect(() => {
    if (cur !== "modules" || mods || modsErr) return;
    fetchModules()
      .then((d) => (mods = d))
      .catch(() => (modsErr = "The module list is not available yet. You can add modules later in Settings."));
  });

  const modChoices = $derived((mods?.index ?? []).filter((e) => !mods?.installed.some((i) => i.name === e.name)));

  function toggleMod(name: string) {
    w.modules = w.modules.includes(name) ? w.modules.filter((n) => n !== name) : [...w.modules, name];
  }

  async function runTest() {
    if (!w.rpc.trim() || testing) return;
    testing = true;
    test = null;
    try {
      test = await testConnection({
        rpc: w.rpc.trim(),
        ...(w.user ? { user: w.user } : {}),
        ...(w.pass ? { pass: w.pass } : {}),
      });
    } catch (e) {
      test = { ok: false, error: e instanceof Error ? e.message : "The test failed." };
    } finally {
      testing = false;
    }
  }

  // ---- "Get the node" (download, check, test, install) -----------------------------------------------
  let rel = $state<ReleaseInfo | null>(null);
  let relState = $state<"idle" | "loading" | "ready" | "missing" | "error">("idle");
  let relErr = $state("");
  let job = $state<InstallJob | null>(null);
  let began = $state(false);
  let starting = $state(false);
  let getErr = $state("");
  let pollFails = 0;
  let graceUntil = 0;
  let timer: ReturnType<typeof setInterval> | undefined;

  const running = $derived(!!job && jobRunning(job));
  // Done by the job, or (after we started one) by the host reporting the program present.
  const installed = $derived(job?.phase === "done" || (began && !running && job?.phase !== "failed" && s.bundle_available === true));
  const nodeText = $derived.by(() => {
    const v = installedVersion(s.managed);
    if (v.text !== "—") return v.text;
    return job?.tag ? withV(job.tag) : rel?.installed?.tag ? withV(rel.installed.tag) : "";
  });
  const unsupported = $derived(!!rel && rel.platform === "unsupported");
  const canGet = $derived(!!rel && !unsupported && rel.can_install && !!rel.latest);
  const mbText = (n: number) => `${(n / 1e6).toFixed(n >= 1e8 ? 0 : 1)} MB`;

  function stopPoll() {
    if (timer) clearInterval(timer);
    timer = undefined;
  }
  async function pollOnce() {
    try {
      const j = await getInstallJob();
      pollFails = 0;
      // Right after a retry the host may still answer with the previous job: ignore finished jobs for a moment.
      if (Date.now() < graceUntil && !jobRunning(j)) return;
      job = j;
      if (!jobRunning(j)) stopPoll();
    } catch (e) {
      if (isMissing(e)) {
        stopPoll();
        return;
      }
      if (++pollFails >= 5) getErr = "Lost contact with the console. The download may still be running; this page keeps trying.";
    }
  }
  function startPoll(grace = 0) {
    stopPoll();
    graceUntil = Date.now() + grace;
    void pollOnce();
    timer = setInterval(pollOnce, 700);
  }

  async function loadRelease(force = false) {
    relState = "loading";
    relErr = "";
    try {
      rel = await getReleases(force);
      relState = "ready";
      // A job already running (the page was reloaded mid-download) is picked up again.
      try {
        const j = await getInstallJob();
        if (jobRunning(j)) {
          job = j;
          began = true;
          startPoll();
        }
      } catch {}
    } catch (e) {
      if (isMissing(e)) relState = "missing";
      else {
        relState = "error";
        relErr = e instanceof Error ? e.message : "Could not check the latest release.";
      }
    }
  }

  $effect(() => {
    if (cur === "get" && relState === "idle") void loadRelease();
  });
  $effect(() => () => stopPoll());

  async function download() {
    if (starting || !rel?.latest) return;
    starting = true;
    getErr = "";
    const tag = rel.latest.tag;
    try {
      const d = await installNode(tag);
      if (d.ok === false) throw new Error(d.error || "The download could not start.");
      began = true;
      job = normalizeJob({ phase: "download", tag, total: rel.asset?.size ?? 0, ...(d.job ?? {}) });
      if (job.phase === "idle") job = { ...job, phase: "download" };
      startPoll(1800);
    } catch (e) {
      if (e instanceof ApiError && e.status === 409) {
        // One is already running (another tab, or a retry that raced): follow it.
        began = true;
        job = normalizeJob({ phase: "download", tag, total: rel.asset?.size ?? 0 });
        startPoll();
      } else {
        getErr = e instanceof Error ? e.message : "The download could not start.";
      }
    } finally {
      starting = false;
    }
  }

  const chainLabel = (chain?: string | null) => NETWORKS.find((n) => n.chain === chain)?.label ?? chain ?? "unknown chain";

  const nextOk = $derived.by(() => {
    if (cur === "mode") return w.mode === "managed" ? canManage : !!w.rpc.trim();
    if (cur === "network") return !!w.network;
    if (cur === "get") return installed;
    return true;
  });

  function next() {
    err = "";
    if (w.step < steps.length - 1) w.step++;
  }
  function back() {
    err = "";
    if (w.step > 0) w.step--;
  }
  // Changing mode changes the step list; stay on the same position.
  function pickMode(m: Mode) {
    w.mode = m;
    test = null;
  }

  async function finish(req: SetupRequest) {
    saving = true;
    err = "";
    try {
      const d = await runSetup(req);
      if (d.ok === false) throw new Error(d.error || "Setup could not be saved.");
      applySnap(d.status ?? (await fetchStatus(AbortSignal.timeout(3000))));
      ui.setupOpen = false;
      ui.setupDone = true;
      setPage("home");
    } catch (e) {
      err = e instanceof Error ? e.message : "Setup could not be saved.";
    } finally {
      saving = false;
    }
  }

  function start() {
    const managed = external || w.mode === "managed";
    void finish({
      mode: managed ? "managed" : "standalone",
      ...(managed ? { network: w.network ?? undefined, ...(w.dataRoot.trim() && !external ? { data_root: w.dataRoot.trim() } : {}) } : { rpc: w.rpc.trim() }),
      start: managed ? w.start : false,
      stop_on_exit: w.stopOnExit,
      modules: managed ? w.modules : [],
    });
  }

  // Umbrel (the app always runs its own node): there is no "my own node" to switch to, so skipping only marks
  // setup as done. The mode, the network and the data folder stay as they are.
  async function markDone() {
    saving = true;
    err = "";
    try {
      await patchSettings({ setup_done: true });
      applySnap(await fetchStatus(AbortSignal.timeout(3000)));
      ui.setupOpen = false;
      ui.setupDone = true;
      setPage("home");
    } catch (e) {
      err = e instanceof Error ? e.message : "Setup could not be skipped.";
    } finally {
      saving = false;
    }
  }

  // "Skip, I already have a node": standalone against the current target, setup marked done.
  function skip() {
    if (rerun) {
      ui.setupOpen = false;
      return;
    }
    if (external) {
      void markDone();
      return;
    }
    void finish({ mode: "standalone", start: false, stop_on_exit: true, modules: [] });
  }

  // An error is shown inside the card, which scrolls: bring it into view (a phone shows one step at a time).
  function reveal(node: HTMLElement, _text?: string) {
    node.scrollIntoView({ block: "nearest" });
    return { update: () => node.scrollIntoView({ block: "nearest" }) };
  }

  const skipLabel = $derived(rerun ? "Close" : external ? "Skip for now" : "Skip");

  const regular = NETWORKS.filter((n) => !n.developer);
  const developer = NETWORKS.filter((n) => n.developer);
</script>

{#snippet netCard(n: (typeof NETWORKS)[number])}
  <button
    type="button"
    class="choice {w.network === n.id ? 'is-on' : ''}"
    aria-pressed={w.network === n.id}
    onclick={() => (w.network = n.id)}
  >
    <span class="choice-name">{n.label}</span>
    <span class="choice-text">{n.plain}</span>
    <span class="choice-meta">{[diskWords(n), n.sync].filter(Boolean).join(" · ")}</span>
  </button>
{/snippet}

<section class="ui-page setup-page">
  <article class="panel ui-card setup" aria-labelledby="setup-title">
    <header class="wiz-head">
      <div class="ui-head">
        <h2 id="setup-title" class="ui-title"><Icon name="settings" class="h-5 w-5 text-muted" /> Set up your node</h2>
        <div class="flex items-center gap-3">
          <span class="ui-chip">Step {Math.min(w.step, steps.length - 1) + 1} of {steps.length}</span>
          {#if cur !== "welcome"}
            <button type="button" class="ui-btn-ghost" disabled={running || saving} onclick={skip}>{skipLabel}</button>
          {/if}
        </div>
      </div>

      <div class="dots" aria-hidden="true">
        {#each steps as _, i}<span class={i === w.step ? "on" : i < w.step ? "past" : ""}></span>{/each}
      </div>
    </header>

    <div class="step" bind:this={stepEl}>
      {#if cur === "welcome"}
        <h3 class="step-title" tabindex="-1">{rerun ? "Run setup again" : "Welcome"}</h3>
        {#if external}
          <p class="ui-lead max-w-2xl">
            Commons Node View runs a Bitcoin node on your Umbrel and shows what it is doing. Setup takes a minute. You
            can change everything later in Settings, and running setup again never deletes data.
          </p>
        {:else}
          <p class="ui-lead max-w-2xl">
            Commons Node View shows what your Bitcoin node is doing. It can also run a node for you, or watch one you
            already run. Setup takes a minute and you can change everything later in Settings. Running it again never
            deletes any data.
          </p>
        {/if}
      {:else if cur === "mode"}
        <h3 class="step-title" tabindex="-1">How do you run your node?</h3>
        <div class="choices two">
          <button
            type="button"
            class="choice {w.mode === 'managed' ? 'is-on' : ''}"
            aria-pressed={w.mode === "managed"}
            disabled={!canManage}
            onclick={() => pickMode("managed")}
          >
            <span class="choice-name">Run a node for me</span>
            <span class="choice-text">
              {canManage
                ? "The console starts, stops and updates a node on this computer."
                : "No node program came with this console, so this is not available here."}
            </span>
          </button>
          <button
            type="button"
            class="choice {w.mode === 'standalone' ? 'is-on' : ''}"
            aria-pressed={w.mode === "standalone"}
            onclick={() => pickMode("standalone")}
          >
            <span class="choice-name">Connect to my existing node</span>
            <span class="choice-text">You run the node yourself. The console watches it over RPC.</span>
          </button>
        </div>

        {#if w.mode === "standalone"}
          <div class="rpc">
            <label class="ui-label" for="setup-rpc">Node RPC address</label>
            <div class="flex flex-wrap gap-2">
              <input
                id="setup-rpc"
                class="ui-input font-mono"
                bind:value={w.rpc}
                spellcheck="false"
                autocomplete="off"
                oninput={() => (test = null)}
              />
              <button type="button" class="ui-btn" disabled={testing || !w.rpc.trim()} onclick={runTest}>
                {testing ? "Testing…" : "Test connection"}
              </button>
            </div>
            <div class="mt-3 grid gap-2 sm:grid-cols-2">
              <div>
                <label class="ui-label" for="setup-user">User (optional)</label>
                <input id="setup-user" class="ui-input w-full" bind:value={w.user} autocomplete="off" spellcheck="false" />
              </div>
              <div>
                <label class="ui-label" for="setup-pass">Password (optional)</label>
                <input id="setup-pass" class="ui-input w-full" type="password" bind:value={w.pass} autocomplete="off" />
              </div>
            </div>
            <p class="ui-note mt-2">
              User and password are only used for this test. To make the console use them, start it with
              <code>BLVM_UI_RPC_USER</code> and <code>BLVM_UI_RPC_PASS</code> set.
            </p>
            {#if test}
              <p class="mt-3 text-[0.95rem] {test.ok ? 'text-good' : 'ui-bad'}" role="status">
                {#if test.ok}
                  Connected. {chainLabel(test.chain)}{test.height !== undefined ? `, block ${fmt(test.height)}` : ""}{test.version
                    ? `, node reports BitcoinCommons ${test.version.replace(/^v/, "")}`
                    : ""}.
                {:else}
                  {friendlyErr(test.error) || "Could not reach the node."}
                {/if}
              </p>
            {/if}
          </div>
        {/if}
      {:else if cur === "network"}
        <h3 class="step-title" tabindex="-1">Which network?</h3>
        <p class="ui-lead max-w-2xl">
          Pick one to continue. Each network keeps its own data, so you can switch later and nothing is lost.
          {#if freeBytes > 0}This computer has {fmtBytes(freeBytes)} free.{/if}
        </p>
        <div class="choices">
          {#each regular as n (n.id)}{@render netCard(n)}{/each}
        </div>
        <details class="dev" open={networkById(w.network)?.developer}>
          <summary>Developer options</summary>
          <div class="choices mt-3">
            {#each developer as n (n.id)}{@render netCard(n)}{/each}
          </div>
        </details>
        {#if tooSmall}
          <p class="ui-warn mt-4 text-[0.95rem]">
            {net?.label} needs {diskWords(net!)} and this disk has {fmtBytes(freeBytes)} free. Pick a smaller network or
            free up space.
          </p>
        {/if}
      {:else if cur === "get"}
        <h3 class="step-title" tabindex="-1">Get the node</h3>
        {#if relState === "idle" || relState === "loading"}
          <p class="ui-note">Checking the latest release…</p>
        {:else if relState === "missing"}
          <p class="ui-lead max-w-2xl">This {external ? "app" : "console"} cannot download the node program here.</p>
          {#if external}
            <p class="ui-note mt-3">You can skip setup for now. The Node page shows what is missing.</p>
            <button type="button" class="ui-btn mt-4" disabled={saving} onclick={skip}>Skip for now</button>
          {:else}
            <p class="ui-note mt-3">
              You can finish setup without one and connect to a node you run yourself. After setup, open Settings and
              use Console Target to point the console at it.
            </p>
            <button type="button" class="ui-btn mt-4" onclick={skip}>Finish without a node</button>
          {/if}
        {:else if relState === "error"}
          <p class="ui-bad text-[0.95rem]" role="alert" use:reveal={relErr}>{relErr}</p>
          <button type="button" class="ui-btn mt-4" onclick={() => loadRelease(true)}>Try again</button>
        {:else if unsupported}
          <p class="ui-lead max-w-2xl">
            There is no official node program for this computer yet. The console can install one on Linux (x86_64 and
            aarch64).
          </p>
          <p class="ui-note mt-3">
            You can finish setup without a node and connect to one you run yourself. After setup, open Settings and
            use Console Target to point the console at it.
          </p>
          <button type="button" class="ui-btn mt-4" onclick={skip}>Finish without a node</button>
        {:else if !canGet}
          {#if rel?.offline}
            <!-- GitHub could not be reached: the reason is the host's own sentence, and trying again is the way on. -->
            <p class="ui-lead max-w-2xl">
              The node could not be downloaded right now{rel.reason ? `: ${rel.reason.split(/\.\s/)[0].replace(/\.$/, "")}` : ""}. Check that
              {external ? "your Umbrel" : "this computer"} is online, then try again.
            </p>
          {:else}
            <p class="ui-lead max-w-2xl">{rel?.reason || "The node cannot be downloaded here right now."}</p>
          {/if}
          <div class="mt-4 flex flex-wrap gap-2">
            <button type="button" class="ui-btn" onclick={() => loadRelease(true)}>Try again</button>
            {#if external}
              <button type="button" class="ui-btn-ghost" disabled={saving} onclick={skip}>Skip for now</button>
            {:else}
              <button type="button" class="ui-btn-ghost" onclick={skip}>Finish without a node</button>
            {/if}
          </div>
          {#if !external}
            <p class="ui-note mt-3">
              Without a node you can connect to one you run yourself, in Settings under Console Target.
            </p>
          {/if}
        {:else}
          {#if running || installed}
            <!-- Compact while a job runs: the card is a fixed square, so the progress must stay in view. -->
            <p class="ui-note">
              {withV(rel!.latest!.tag)}{rel!.asset ? `, ${mbText(rel!.asset.size)}` : ""}. Checked against the release's
              published checksum before it is installed.
            </p>
          {:else}
            <p class="ui-lead max-w-2xl">
              This {external ? "app" : "computer"} has no node program yet. The console downloads the official release for
              you and checks it before using it.
            </p>
            <dl class="kv sum">
              <div><dt>Latest release</dt><dd>{withV(rel!.latest!.tag)}</dd></div>
              {#if rel!.asset}<div><dt>Download size</dt><dd>{mbText(rel!.asset.size)}</dd></div>{/if}
            </dl>
            <p class="ui-note mt-3">
              Checked against the release's published checksum before it is installed, so a damaged download is never
              used. Releases are not signed yet.
            </p>
          {/if}
          {#if installed}
            <p class="mt-4 text-[0.95rem] text-good" role="status">Node {nodeText || withV(rel!.latest!.tag)} installed.</p>
          {:else if running && job}
            <InstallProgress {job} />
          {:else}
            {#if job?.phase === "failed"}
              <p class="ui-bad mt-4 text-[0.95rem]" role="alert" use:reveal={job.error ?? ""}>
                {job.error || "The download failed."}
                {job.rolled_back ? " The previous version is back." : " Nothing was installed."}
              </p>
            {/if}
            <button type="button" class="ui-btn mt-4" disabled={starting} onclick={download}>
              {starting ? "Starting…" : job?.phase === "failed" ? "Retry" : `Download ${withV(rel!.latest!.tag)}`}
            </button>
          {/if}
          {#if getErr}<p class="ui-bad mt-3 text-[0.95rem]" role="alert" use:reveal={getErr}>{getErr}</p>{/if}
        {/if}
      {:else if cur === "storage"}
        <h3 class="step-title" tabindex="-1">Storage and options</h3>
        <div class="rpc">
          <label class="ui-label" for="setup-root">Data location</label>
          <input
            id="setup-root"
            class="ui-input w-full font-mono"
            bind:value={w.dataRoot}
            placeholder={/Mac/i.test(navigator.platform) ? "~/Library/Application Support/blvm" : "~/.local/share/blvm"}
            spellcheck="false"
            autocomplete="off"
          />
          <p class="ui-note mt-2">
            Leave empty for the default. {freeBytes > 0 ? `${fmtBytes(freeBytes)} free on the disk the console runs from.` : ""}
          </p>
          {#if tooSmall}
            <p class="ui-warn mt-2 text-[0.95rem]">
              {net?.label} needs {diskWords(net!)}. That may not fit here.
            </p>
          {/if}
        </div>
        <div class="toggles">
          <div class="toggle">
            <span>Start the node now</span>
            <button
              type="button"
              role="switch"
              aria-checked={w.start}
              aria-label="Start the node now"
              class="ui-switch"
              onclick={() => (w.start = !w.start)}
            ></button>
          </div>
          <div class="toggle">
            <span>Stop the node when the console quits</span>
            <button
              type="button"
              role="switch"
              aria-checked={w.stopOnExit}
              aria-label="Stop the node when the console quits"
              class="ui-switch"
              onclick={() => (w.stopOnExit = !w.stopOnExit)}
            ></button>
          </div>
        </div>
        <p class="ui-note mt-4">The console collects no usage data.</p>
      {:else if cur === "modules"}
        <h3 class="step-title" tabindex="-1">Modules <span class="opt-tag">optional</span></h3>
        <p class="ui-lead max-w-2xl">Add-ons that run next to the node. You can add or remove them any time in Settings.</p>
        {#if modsErr}
          <p class="ui-note mt-4">{modsErr}</p>
        {:else if !mods}
          <p class="ui-note mt-4">Loading…</p>
        {:else if modChoices.length === 0}
          <p class="ui-note mt-4">There are no modules to add right now.</p>
        {:else}
          <ul class="mods">
            {#each modChoices as e (e.name)}
              {@const why = moduleReason(e, mods)}
              <li>
                <label class="mod {why ? 'is-off' : ''}">
                  <input
                    type="checkbox"
                    checked={w.modules.includes(e.name)}
                    disabled={!!why}
                    onchange={() => toggleMod(e.name)}
                  />
                  <span class="min-w-0">
                    <span class="choice-name">{moduleTitle(e.name, e)}</span>
                    <span class="choice-text block">{e.description}</span>
                    {#if why}<span class="ui-note ui-warn block">{why}</span>{/if}
                  </span>
                </label>
              </li>
            {/each}
          </ul>
        {/if}
      {:else}
        <h3 class="step-title" tabindex="-1">Ready</h3>
        <dl class="kv sum">
          {#if external || w.mode === "managed"}
            <div><dt>Node</dt><dd>{needGet && nodeText ? `${nodeText} installed` : "Run by this console"}</dd></div>
            <div><dt>Network</dt><dd>{net?.label ?? "Not chosen"}</dd></div>
            <div><dt>Disk needed</dt><dd>{net ? diskWords(net) : "—"}{freeBytes > 0 ? ` · ${fmtBytes(freeBytes)} free` : ""}</dd></div>
            {#if !external}
              <div><dt>Data location</dt><dd class="font-mono text-[0.9rem]">{w.dataRoot.trim() || "Default"}</dd></div>
              <div><dt>Start the node on {net?.label ?? "…"}</dt><dd>{w.start ? "Yes, right away" : "No, later from Settings"}</dd></div>
              <div><dt>Stop when the console quits</dt><dd>{w.stopOnExit ? "Yes" : "No"}</dd></div>
              <div>
                <dt>Modules</dt>
                <dd>{w.modules.length ? w.modules.map((n) => moduleTitle(n, mods?.index.find((e) => e.name === n))).join(", ") : "None"}</dd>
              </div>
            {/if}
          {:else}
            <div><dt>Node</dt><dd>Your own node</dd></div>
            <div><dt>RPC address</dt><dd class="font-mono text-[0.9rem]">{w.rpc}</dd></div>
          {/if}
        </dl>
        {#if err}<p class="ui-bad mt-4 text-[0.95rem]" role="alert" use:reveal={err}>{err}</p>{/if}
      {/if}
      {#if err && cur !== "summary"}<p class="ui-bad mt-3 text-[0.95rem]" role="alert" use:reveal={err}>{err}</p>{/if}
    </div>
    <div class="foot">
      {#if cur === "welcome"}
        <button type="button" class="ui-btn-ghost" disabled={saving} onclick={skip}>
          {rerun ? "Close" : external ? "Skip for now" : "Skip, I already have a node"}
        </button>
        <button type="button" class="ui-btn" onclick={next}>Set up</button>
      {:else if cur === "mode"}
        <button type="button" class="ui-btn-ghost" onclick={back}>Back</button>
        <button type="button" class="ui-btn" disabled={!nextOk} onclick={next}>Next</button>
      {:else if cur === "network"}
        <button type="button" class="ui-btn-ghost" onclick={back}>Back</button>
        <button type="button" class="ui-btn" disabled={!nextOk} onclick={next}>Next</button>
      {:else if cur === "get"}
        <button type="button" class="ui-btn-ghost" disabled={running} onclick={back}>Back</button>
        <button type="button" class="ui-btn" disabled={!nextOk} onclick={next}>Next</button>
      {:else if cur === "storage"}
        <button type="button" class="ui-btn-ghost" onclick={back}>Back</button>
        <button type="button" class="ui-btn" onclick={next}>Next</button>
      {:else if cur === "modules"}
        <button type="button" class="ui-btn-ghost" onclick={back}>Back</button>
        <button type="button" class="ui-btn" onclick={next}>Next</button>
      {:else}
        <button type="button" class="ui-btn-ghost" disabled={saving} onclick={back}>Back</button>
        <button type="button" class="ui-btn" disabled={saving || ((external || w.mode === "managed") && (!w.network || (needGet && !installed)))} onclick={start}>
          {saving ? "Saving…" : (external || w.mode === "managed") && w.start ? `Start the node on ${net?.label ?? "…"}` : "Finish"}
        </button>
      {/if}
    </div>
  </article>
</section>

<style>
  /* Wizard shell: the viewport below the top bar (about 48px) minus the main padding, centred both ways. The
     card is a fixed square (--side) that is the same on every step; the body scrolls inside it. */
  .setup-page {
    --wiz-chrome: calc(48px + 2.5rem); /* top bar + main vertical padding */
    --side: min(640px, calc(100vw - 32px), calc(100dvh - var(--wiz-chrome)));
    min-height: calc(100vh - var(--wiz-chrome));
    min-height: calc(100dvh - var(--wiz-chrome));
    align-items: center;
    justify-content: center;
  }
  @media (min-width: 640px) {
    .setup-page {
      --wiz-chrome: calc(48px + 3rem);
    }
  }
  .setup {
    box-sizing: border-box;
    width: var(--side);
    height: var(--side); /* explicit height as well as the ratio, so the body row has a definite size */
    aspect-ratio: 1 / 1;
    min-height: min(20rem, 100%);
    flex: none;
    display: grid;
    grid-template-rows: auto minmax(0, 1fr) auto;
    padding: 1.4rem 1.6rem;
    overflow: hidden;
  }
  @media (max-width: 420px) {
    .setup {
      padding: 1.1rem 1.1rem;
    }
  }
  .wiz-head {
    flex: none;
  }
  .wiz-head :global(.ui-head) {
    min-height: 2.5rem; /* same height with or without the Skip button */
    flex-wrap: nowrap;
  }
  .wiz-head :global(.ui-title) {
    min-width: 0;
    white-space: nowrap;
  }
  .step {
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding-right: 0.25rem;
  }
  .foot {
    margin-top: 0.9rem;
    padding-top: 0.9rem;
    border-top: 1px solid var(--color-line);
    min-height: 3.1rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
  }
  .foot :global(.ui-btn-ghost) {
    padding: 0.6rem 1rem;
    font-size: 0.95rem;
  }
  .dots {
    margin-bottom: 1rem;
    display: flex;
    gap: 0.4rem;
  }
  .dots span {
    width: 0.4rem;
    height: 0.4rem;
    border-radius: 999px;
    background: rgb(240 230 208 / 0.18);
    transition: width 0.3s, background 0.3s;
  }
  .dots span.past {
    background: rgb(247 147 26 / 0.45);
  }
  .dots span.on {
    width: 1.1rem;
    background: var(--color-orange);
  }
  .step-title:focus {
    outline: none;
  }
  .step-title {
    margin-bottom: 0.7rem;
    font-size: 1.3rem;
    font-weight: 500;
    color: var(--color-cream);
  }
  .opt-tag {
    margin-left: 0.5rem;
    font-size: 0.78rem;
    font-weight: 400;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--color-faint);
  }
  .choices {
    margin-top: 1rem;
    display: grid;
    gap: 0.6rem;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 14rem), 1fr));
  }
  .choice {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0.3rem;
    border: 1px solid var(--color-line);
    border-radius: 1rem;
    background: #0e0d0b;
    padding: 1rem 1.1rem;
    text-align: left;
    color: var(--color-muted);
    transition: border-color 0.2s, background 0.2s;
  }
  .choice:hover:not(:disabled) {
    border-color: rgb(240 230 208 / 0.25);
  }
  .choice:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .choice.is-on {
    border-color: rgb(247 147 26 / 0.55);
    background: linear-gradient(180deg, #17130d, #0e0d0b);
    color: var(--color-cream);
    box-shadow: 0 0 0 1px rgb(247 147 26 / 0.12);
  }
  .choice-name {
    font-size: 1.05rem;
    font-weight: 500;
    color: var(--color-cream);
  }
  .choice-text {
    font-size: 0.9rem;
    line-height: 1.45;
    color: var(--color-muted);
  }
  .choice-meta {
    margin-top: 0.15rem;
    font-size: 0.78rem;
    color: var(--color-faint);
  }
  .dev {
    margin-top: 1rem;
    font-size: 0.9rem;
    color: var(--color-muted);
  }
  .dev summary {
    cursor: pointer;
    width: fit-content;
  }
  .dev summary:hover {
    color: var(--color-cream);
  }
  .rpc {
    margin-top: 1.4rem;
  }
  .toggles {
    margin-top: 1.4rem;
    border-top: 1px solid var(--color-line);
  }
  .toggle {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.8rem 0;
    border-bottom: 1px solid var(--color-line);
    color: var(--color-muted);
  }
  .mods {
    margin-top: 1rem;
    display: grid;
    gap: 0.5rem;
  }
  .mod {
    display: flex;
    align-items: flex-start;
    gap: 0.85rem;
    border: 1px solid var(--color-line);
    border-radius: 0.85rem;
    padding: 0.85rem 1rem;
    cursor: pointer;
  }
  .mod.is-off {
    opacity: 0.55;
    cursor: not-allowed;
  }
  .mod input {
    margin-top: 0.3rem;
    accent-color: var(--color-orange);
  }
  .sum {
    margin-top: 0.5rem;
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.88em;
    color: var(--color-orange);
  }
</style>
