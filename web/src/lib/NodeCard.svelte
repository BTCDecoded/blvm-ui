<script lang="ts">
  import { onMount } from "svelte";
  import { fetchLogs, getSettings, nodeAction, patchSettings } from "./api";
  import { durLabel, installedVersion, nodeReports } from "./format";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import { networkText } from "./networks";
  import { pendingText, storageText, storageView } from "./pruning";
  import { switchRunning, ui } from "./status.svelte";
  import type { ConsoleSettings, NodeAction, ProcessState } from "./types";

  // Design G.1: the managed Node card (replaces "Node Process").
  const s = $derived(ui.snap);
  const m = $derived(s.managed!);
  const proc = $derived(m.process);
  const st = $derived<ProcessState>(proc.state);
  const switching = $derived(switchRunning(m));

  const PILL: Record<ProcessState, { label: string; health: string }> = {
    running: { label: "Running", health: "good" },
    starting: { label: "Starting…", health: "heal" },
    stopping: { label: "Stopping…", health: "heal" },
    stopped: { label: "Stopped", health: "dead" },
    crashed: { label: "Crashed", health: "dead" },
  };
  // Umbrel: the node runs under an agent in another container. When that agent stops answering, what it last
  // wrote (for example "Running") is not true any more, so the card says so instead.
  const external = $derived(m.agent.external);
  const agentGone = $derived(external && !m.agent.alive);
  const pill = $derived(agentGone ? { label: "Agent offline", health: "dead" } : (PILL[st] ?? PILL.stopped));

  // The same version text as About (design M.3): what is installed. What the node says over RPC is only a
  // second, muted line while it answers (M.5).
  const installed = $derived(installedVersion(m));
  const reports = $derived(nodeReports(s, installed));
  const noNetwork = $derived(m.network === null);
  const storage = $derived(storageView(s.pruning, true));
  const uptime = $derived(st === "running" && proc.since ? durLabel(Date.now() / 1000 - proc.since) : "—");

  let busy = $state(false);
  let hint = $state("");
  let crashLog = $state<string[]>([]);
  let settings = $state<ConsoleSettings>({});
  let settingsErr = $state("");

  const idle = $derived(!busy && !switching && st !== "stopping" && !agentGone);

  async function act(action: NodeAction) {
    if (busy) return;
    busy = true;
    hint = "";
    try {
      const d = await nodeAction(action);
      if (d.status) ui.snap = d.status;
      hint = d.error || d.message || "";
    } catch (err) {
      hint = err instanceof Error ? err.message : "The request failed.";
    } finally {
      busy = false;
    }
  }

  // A reply from an earlier click is stale once the process changes state.
  $effect(() => {
    void st;
    hint = "";
  });

  // Crash block: the last 50 lines, fetched when the node is (or becomes) crashed.
  let crashFor = "";
  // A plain string (empty unless crashed): `proc` is a new object on every status poll, this changes only with the crash.
  const crashKey = $derived(st === "crashed" ? `${proc.since}:${proc.restarts}` : "");
  $effect(() => {
    const key = crashKey;
    if (key === "") {
      crashFor = "";
      crashLog = [];
      return;
    }
    if (crashFor === key) return;
    crashFor = key;
    fetchLogs(undefined, 50)
      .then((r) => (crashLog = r.lines.slice(-50).map((l) => l.text)))
      .catch(() => (crashLog = []));
  });

  onMount(() => {
    getSettings()
      .then((d) => (settings = d))
      .catch(() => {});
  });

  // "Start the node when the console opens": the host's `start_on_boot` (older hosts: `start_on_launch`).
  // On unless the host says otherwise.
  const autoStart = $derived(settings.start_on_boot ?? settings.start_on_launch ?? true);

  async function flip(key: "auto_start" | "stop_node_on_exit") {
    settingsErr = "";
    try {
      const patch: ConsoleSettings =
        key === "auto_start"
          ? { start_on_boot: !autoStart, start_on_launch: !autoStart }
          : { stop_node_on_exit: !settings.stop_node_on_exit };
      settings = { ...settings, ...(await patchSettings(patch)) };
    } catch (err) {
      settingsErr = err instanceof Error ? err.message : "Could not save that setting.";
    }
  }
</script>

<article class="panel ui-card">
  <div class="ui-head">
    <h2 class="ui-title"><Icon name="power" class="h-5 w-5 text-muted" /> Node</h2>
    <span class="ui-chip"><Light health={pill.health} /> {pill.label}</span>
  </div>

  <dl class="kv">
    <div><dt>Network</dt><dd>{networkText(m.network)}</dd></div>
    {#if !noNetwork}
      <div>
        <dt>Storage</dt>
        <dd>
          {storageText(storage)}
          {#if storage.pending}<span class="ui-chip ml-2">{pendingText(storage)}</span>{/if}
        </dd>
      </div>
    {/if}
    <div>
      <dt>Installed</dt>
      <dd>
        {installed.text}
        {#if installed.custom}<span class="ui-chip">Custom build</span>{/if}
        {#if reports}<span class="ui-seen ml-2">{reports}</span>{/if}
      </dd>
    </div>
    <div><dt>Uptime</dt><dd>{uptime}</dd></div>
  </dl>

  {#if agentGone}
    <p class="ui-lead ui-bad mt-4" role="alert">The node agent is not running. Restart the app.</p>
  {/if}

  {#if noNetwork}
    <p class="ui-note mt-4">
      No network has been chosen yet, so there is nothing to start.
      <button type="button" class="ui-link" onclick={() => (ui.setupOpen = true)}>Run setup</button>
    </p>
  {/if}

  {#if proc.notice && (st === "running" || st === "starting") && !agentGone}
    <p class="ui-note mt-4">{proc.notice}</p>
  {/if}

  {#if st === "stopped" && proc.last_error && !agentGone}
    <p class="ui-note ui-warn mt-4">{proc.last_error}</p>
  {/if}

  {#if agentGone}
    <!-- the banner above says it all -->
  {:else if st === "stopping"}
    <p class="ui-note mt-4">Saving chain state. A node that is syncing can take a few minutes to stop cleanly.</p>
  {:else if st === "starting"}
    <p class="ui-note mt-4">Starting. The node can take a minute or two before it answers.</p>
  {/if}

  {#if st === "crashed" && !agentGone}
    <div class="mt-4">
      {#if proc.conflict}
        <p class="ui-lead ui-bad">
          The node was not started: another program is using its port.
          {#if proc.last_error}<span class="block pt-1 text-muted">{proc.last_error}</span>{/if}
        </p>
      {:else}
        <p class="ui-lead ui-bad">
          The node stopped unexpectedly{proc.exit_code !== null ? ` (exit code ${proc.exit_code})` : ""}.
          {#if proc.last_error}<span class="block pt-1 text-muted">{proc.last_error}</span>{/if}
        </p>
        <p class="ui-sub mb-2 mt-4">Last log lines</p>
        <code class="ui-code ui-log" aria-label="Last log lines">{crashLog.length ? crashLog.join("\n") : "No log lines available."}</code>
      {/if}
    </div>
  {/if}

  <div class="power-row">
    <p class="ui-note min-w-0">{hint || (switching ? "A network switch is running." : external ? "This page keeps running either way." : "This console keeps running either way.")}</p>
    <div class="flex flex-wrap gap-2">
      {#if st === "running"}
        <button type="button" class="power-btn is-neutral" disabled={!idle} onclick={() => act("restart")}>
          <Icon name="wave" class="h-4 w-4" /> Restart
        </button>
      {/if}
      {#if st === "stopped" || st === "crashed"}
        <button type="button" class="power-btn is-start" disabled={!idle || noNetwork} onclick={() => act("start")}>
          <Icon name="power" class="h-4 w-4" /> {st === "crashed" ? "Try again" : "Start"}
        </button>
      {:else}
        <button type="button" class="power-btn is-stop" disabled={!idle} onclick={() => act("stop")}>
          <Icon name="power" class="h-4 w-4" /> Stop
        </button>
      {/if}
    </div>
  </div>

  <div class="opts">
    {#if external}
      <!-- On Umbrel the node agent restores what you asked for last by itself, whatever this app is set to, so
           there is nothing to switch here. -->
      <p class="ui-note opt-help">
        The node comes back after an app update or a restart of your Umbrel if you left it running. A node you
        stopped stays stopped.
      </p>
    {:else}
      <div class="opt">
        <span>Start the node when this app starts</span>
        <button
          type="button"
          role="switch"
          aria-checked={autoStart}
          aria-label="Start the node when this app starts"
          class="ui-switch"
          onclick={() => flip("auto_start")}
        ></button>
      </div>
      <p class="ui-note opt-help">Applies when you left the node running. A node you stopped stays stopped until you start it.</p>
    {/if}
    {#if !external}
      <!-- On Umbrel the node belongs to its own container, which the app never stops on its way out. -->
      <div class="opt">
        <span>Stop the node when this console quits</span>
        <button
          type="button"
          role="switch"
          aria-checked={!!settings.stop_node_on_exit}
          aria-label="Stop the node when this console quits"
          class="ui-switch"
          onclick={() => flip("stop_node_on_exit")}
        ></button>
      </div>
    {/if}
    {#if settingsErr}<p class="ui-note ui-bad">{settingsErr}</p>{/if}
  </div>
</article>

<style>
  .power-row {
    margin-top: 1.25rem;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .opts {
    margin-top: 1.25rem;
    border-top: 1px solid var(--color-line);
    padding-top: 0.5rem;
  }
  @media (max-width: 767px) {
    .power-row > :global(.flex) {
      width: 100%;
    }
    .opt > span {
      min-width: 0;
      overflow-wrap: anywhere;
    }
  }
  .opt-help {
    margin: 0 0 0.35rem;
    font-size: 0.85rem;
  }
  .opt {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.55rem 0;
    font-size: 0.95rem;
    color: var(--color-muted);
  }
</style>
