<script lang="ts">
  import { onMount } from "svelte";
  import { fetchStatus } from "./lib/api";
  import HomePanel from "./lib/HomePanel.svelte";
  import Icon from "./lib/Icon.svelte";
  import InsightsPanel from "./lib/InsightsPanel.svelte";
  import Light from "./lib/Light.svelte";
  import NetworkPanel from "./lib/NetworkPanel.svelte";
  import SettingsPanel from "./lib/SettingsPanel.svelte";
  import LogsPanel from "./lib/LogsPanel.svelte";
  import MiningPanel from "./lib/MiningPanel.svelte";
  import Terrain from "./lib/Terrain.svelte";
  import ViewSwitch from "./lib/ViewSwitch.svelte";
  import { applySnap, setPage, ui, type Page } from "./lib/status.svelte";

  const side: { id: Page; label: string; icon: string }[] = [
    { id: "home", label: "Home", icon: "home" },
    { id: "mining", label: "Mining", icon: "mining" },
    { id: "network", label: "Network", icon: "network" },
    { id: "logs", label: "Logs", icon: "logs" },
    { id: "settings", label: "Settings", icon: "settings" },
  ];

  async function tick() {
    try {
      applySnap(await fetchStatus(AbortSignal.timeout(3000)));
    } catch {
      ui.live = false;
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
      <span class="text-sm text-muted">Local Console</span>
    </div>
    <div></div>
    <div class="flex items-center justify-end gap-2 text-sm text-muted">
      <Light health={ui.live ? "good" : "dead"} />
      Console Live
    </div>
  </header>

  <div class="relative z-10 flex min-h-0 flex-1">
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

    <main class="flex min-h-0 min-w-0 flex-1 flex-col overflow-x-hidden overflow-y-auto px-4 py-5 sm:px-6 sm:py-6">
      {#if ui.page === "home" || ui.page === "insights"}
        <ViewSwitch />
      {/if}
      {#if ui.page === "home"}
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
    </main>
  </div>
</div>
