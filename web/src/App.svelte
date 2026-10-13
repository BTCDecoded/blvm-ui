<script lang="ts">
  import { onMount } from "svelte";
  import { ApiError, fetchStatus } from "./lib/api";
  import HomePanel from "./lib/HomePanel.svelte";
  import Icon from "./lib/Icon.svelte";
  import InsightsPanel from "./lib/InsightsPanel.svelte";
  import Light from "./lib/Light.svelte";
  import NetworkPanel from "./lib/NetworkPanel.svelte";
  import SettingsPanel from "./lib/SettingsPanel.svelte";
  import LogsPanel from "./lib/LogsPanel.svelte";
  import MiningPanel from "./lib/MiningPanel.svelte";
  import LogsGate from "./lib/LogsGate.svelte";
  import ModuleGate from "./lib/ModuleGate.svelte";
  import { MODULE_PAGES } from "./lib/modules";
  import Setup from "./lib/Setup.svelte";
  import Terrain from "./lib/Terrain.svelte";
  import ViewSwitch from "./lib/ViewSwitch.svelte";
  import { applySnap, modulePages, setPage, ui, wizardActive, type Page } from "./lib/status.svelte";

  type Item = { id: Page; label: string; icon: string };
  const home: Item = { id: "home", label: "Home", icon: "home" };
  const network: Item = { id: "network", label: "Network", icon: "network" };
  const logs: Item = { id: "logs", label: "Logs", icon: "logs" };
  const settings: Item = { id: "settings", label: "Settings", icon: "settings" };

  // Design L: core pages plus the pages installed modules add. A host that does not send `modules_pages`
  // (and the moment before the first /api/status answers) shows the core pages only: a module page is never
  // shown unless the host says its module is installed and on.
  const side = $derived.by<Item[]>(() => {
    const mp = ui.ready ? modulePages(ui.snap) : null;
    // Logs is off until the user turns it on in Settings (`show_logs`).
    const showLogs = ui.ready && ui.snap.show_logs === true;
    const extra = (mp ?? []).filter((id) => id in MODULE_PAGES).map((id) => ({ id: id as Page, ...MODULE_PAGES[id] }));
    return [home, ...extra, network, ...(showLogs ? [logs] : []), settings];
  });

  // A module page whose module is not there (typed URL, or the module was just removed).
  const gated = $derived.by(() => {
    if (!ui.ready || !(ui.page in MODULE_PAGES)) return false;
    return !(modulePages(ui.snap) ?? []).includes(ui.page);
  });
  const logsOff = $derived(ui.ready && ui.page === "logs" && ui.snap.show_logs !== true);
  const wizard = $derived(wizardActive());
  // Umbrel: the node runs under an agent in another container; when its heartbeat is gone the page says so
  // next to the "Console Live" light (the console itself is fine, the node is not being looked after).
  const agentDown = $derived(ui.live && ui.snap.managed?.agent.external === true && ui.snap.managed.agent.alive === false);

  // False until the first /api/status has answered (or failed): the dashboard and the wizard both depend on
  // `setup_done`, so neither is drawn before that, and a finished setup never flashes the wizard.
  let settled = $state(false);

  async function tick() {
    try {
      applySnap(await fetchStatus(AbortSignal.timeout(3000)));
    } catch (e) {
      ui.live = false;
      ui.hostError = e instanceof ApiError && e.status === 403 ? e.message : "";
    } finally {
      settled = true;
    }
  }

  onMount(() => {
    if (ui.page === "node") setPage("insights");
    tick();
    const id = setInterval(tick, 1000);
    return () => clearInterval(id);
  });
</script>

<div class="relative flex h-dvh flex-col overflow-hidden">
  <Terrain />

  <header class="relative z-10 grid grid-cols-[1fr_auto_1fr] items-center gap-3 border-b border-line px-4 py-3 sm:px-6">
    <div class="flex items-center gap-3">
      <img src="/logo.png" alt="Commons" class="h-7 w-auto" />
      <span class="text-faint">/</span>
      <span class="text-sm text-muted">Node View</span>
    </div>
    <div></div>
    <div class="flex items-center justify-end gap-2 text-sm text-muted">
      {#if agentDown}
        <Light health="dead" />
        <span role="status">Node agent offline</span>
      {:else}
        <Light health={ui.live ? "good" : "dead"} />
        Console Live
      {/if}
    </div>
  </header>

  <div class="relative z-10 flex min-h-0 flex-1">
    {#if settled && !wizard}
    <aside class="flex w-[4.5rem] shrink-0 flex-col border-r border-line py-4 sm:w-48">
      <nav class="flex flex-col gap-0.5 px-2" aria-label="Primary">
        {#each side as item}
          <button
            type="button"
            class="nav-side flex w-full items-center gap-3 rounded-r-lg px-3 py-2.5 text-left text-sm {ui.page ===
            item.id
              ? 'is-on'
              : 'text-muted hover:text-cream'}"
            onclick={() => setPage(item.id)}
          >
            <Icon name={item.icon} class="h-4 w-4 shrink-0" />
            <span class="hidden sm:inline">{item.label}</span>
          </button>
        {/each}
      </nav>
    </aside>
    {/if}

    <main class="flex min-h-0 min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto px-4 py-5 sm:px-6 sm:py-6">
      {#if ui.hostError}
        <p class="ui-note ui-bad mb-4 max-w-3xl" role="alert">{ui.hostError}</p>
      {/if}
      {#if !settled}
        <div class="boot" role="status" aria-live="polite">
          <span class="boot-ring" aria-hidden="true"></span>
          Connecting to the console…
        </div>
      {:else if wizard}
        <Setup />
      {:else}
      {#if ui.page === "home" || ui.page === "insights"}
        <ViewSwitch />
      {/if}
      {#if gated}
        <ModuleGate page={ui.page} />
      {:else if logsOff}
        <LogsGate />
      {:else if ui.page === "home"}
        <HomePanel />
      {:else if ui.page === "insights"}
        <InsightsPanel />
      {:else if ui.page === "settings"}
        <SettingsPanel />
      {:else if ui.page === "network"}
        <NetworkPanel />
      {:else if ui.page === "mining"}
        <MiningPanel />
      {:else}
        <LogsPanel />
      {/if}
      {/if}
    </main>
  </div>
</div>

<style>
  .boot {
    margin: auto;
    display: flex;
    align-items: center;
    gap: 0.75rem;
    color: var(--color-muted);
    font-size: 0.95rem;
  }
  .boot-ring {
    width: 1.1rem;
    height: 1.1rem;
    border-radius: 999px;
    border: 2px solid rgb(240 230 208 / 0.18);
    border-top-color: var(--color-orange);
    animation: boot-spin 0.9s linear infinite;
  }
  @keyframes boot-spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .boot-ring {
      animation-duration: 3s;
    }
  }
</style>
