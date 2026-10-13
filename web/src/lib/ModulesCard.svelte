<script lang="ts">
  import { onMount } from "svelte";
  import { fetchModules, isMissing, moduleAction } from "./api";
  import { withV } from "./format";
  import Confirm from "./Confirm.svelte";
  import Icon from "./Icon.svelte";
  import { moduleReason, moduleTitle } from "./modules";
  import type { ModuleAction, ModulesResponse } from "./types";

  // Design G.4 + E: installed modules (enable toggle, Remove) and available ones (Install).
  let data = $state<ModulesResponse | null>(null);
  let err = $state("");
  // The host has no module routes (every host before the module manager).
  let missing = $state(false);
  // Names with an action in flight; cleared when the list shows the result (or after a minute).
  let pending = $state<Record<string, { action: ModuleAction; at: number }>>({});
  let removing = $state<string | null>(null);
  let removeOpen = $state(false);

  async function load() {
    try {
      data = await fetchModules();
      err = "";
      missing = false;
      const have = new Map(data.installed.map((i) => [i.name, i]));
      for (const [name, p] of Object.entries(pending)) {
        const a = p.action;
        const done =
          a === "install"
            ? have.has(name)
            : a === "remove"
              ? !have.has(name)
              : a === "enable"
                ? have.get(name)?.enabled === true
                : have.get(name)?.enabled === false;
        if (done) delete pending[name];
        else if (Date.now() - p.at > 60000) {
          delete pending[name];
          err = `${moduleTitle(name, data.index.find((e) => e.name === name))}: the host did not report the change.`;
        }
      }
    } catch (e) {
      if (isMissing(e)) {
        missing = true;
        data = null;
        err = "";
        return;
      }
      err = e instanceof Error ? e.message : "Could not load modules.";
    }
  }

  // Design E: every ten seconds while this card is open, every second while an action is in flight so the
  // sidebar follows live.
  onMount(() => {
    let tick = 0;
    void load();
    const id = setInterval(() => {
      tick++;
      if (Object.keys(pending).length > 0 || tick % 10 === 0) void load();
    }, 1000);
    return () => clearInterval(id);
  });

  async function act(action: ModuleAction, name: string) {
    err = "";
    pending[name] = { action, at: Date.now() };
    try {
      const d = await moduleAction(action, name);
      if (d.ok === false) throw new Error(d.error || "That did not work.");
      void load();
    } catch (e) {
      delete pending[name];
      err = e instanceof Error ? e.message : "That did not work.";
    }
  }

  const installed = $derived(data?.installed ?? []);
  const installedNames = $derived(new Set(installed.map((i) => i.name)));
  const available = $derived((data?.index ?? []).filter((e) => !installedNames.has(e.name)));
  const entry = (name: string) => data?.index.find((e) => e.name === name);

  function askRemove(name: string) {
    removing = name;
    removeOpen = true;
  }
  async function doRemove() {
    const name = removing;
    removeOpen = false;
    if (name) await act("remove", name);
  }
</script>

<article class="panel ui-card">
  <div class="ui-head">
    <h2 class="ui-title"><Icon name="cube" class="h-5 w-5 text-muted" /> Modules</h2>
    <span class="ui-chip">{missing ? "Not available" : `${installed.length} installed`}</span>
  </div>
  <p class="ui-lead">Optional add-ons that run next to the node. Mining pages appear in the sidebar when a mining module is on.</p>

  {#if missing}
    <p class="ui-note mt-3">Modules are not available on this host yet.</p>
  {:else}

  {#if data && !data.can_install}
    <p class="ui-note ui-warn mt-3">{data.reason || "Installing modules is not available on this setup."}</p>
  {/if}

  <p class="ui-sub mt-5">Installed</p>
  {#if installed.length === 0}
    <p class="ui-note mt-2">{data ? "No modules installed yet." : "Loading…"}</p>
  {:else}
    <ul class="ui-list mt-3">
      {#each installed as mod (mod.name)}
        {@const busy = !!pending[mod.name]}
        <li>
          <div class="min-w-0">
            <p class="name">
              {moduleTitle(mod.name, entry(mod.name))}
              <span class="ver">{withV(mod.version)}</span>
            </p>
            <p class="ui-note">
              {entry(mod.name)?.description ?? mod.name}
              {#if pending[mod.name]}
                · {pending[mod.name]?.action === "remove" ? "Removing…" : "Working…"}
              {:else if mod.enabled && !mod.loaded}
                · Not loaded
              {:else if !mod.enabled}
                · Off
              {/if}
            </p>
          </div>
          <div class="row-act">
            <button
              type="button"
              role="switch"
              aria-checked={mod.enabled}
              aria-label={`${mod.enabled ? "Turn off" : "Turn on"} ${moduleTitle(mod.name, entry(mod.name))}`}
              class="ui-switch"
              disabled={busy}
              onclick={() => act(mod.enabled ? "disable" : "enable", mod.name)}
            ></button>
            <button type="button" class="ui-btn-ghost is-danger" disabled={busy} onclick={() => askRemove(mod.name)}>Remove</button>
          </div>
        </li>
      {/each}
    </ul>
  {/if}

  <p class="ui-sub mt-5">Available</p>
  {#if available.length === 0}
    <p class="ui-note mt-2">{data ? "Everything in the index is installed." : "Loading…"}</p>
  {:else}
    <ul class="ui-list mt-3">
      {#each available as e (e.name)}
        {@const why = moduleReason(e, data)}
        {@const busy = !!pending[e.name]}
        <li>
          <div class="min-w-0">
            <p class="name">{moduleTitle(e.name, e)}</p>
            <p class="ui-note">{e.description}</p>
            {#if why}<p class="ui-note ui-warn">{why}</p>{/if}
          </div>
          <div class="row-act">
            <button type="button" class="ui-btn-ghost" disabled={!!why || busy} onclick={() => act("install", e.name)}>
              {busy ? "Installing…" : "Install"}
            </button>
          </div>
        </li>
      {/each}
    </ul>
  {/if}
  {/if}
  {#if err}<p class="ui-note ui-bad mt-3">{err}</p>{/if}
</article>

<Confirm
  bind:open={removeOpen}
  title={`Remove ${removing ? moduleTitle(removing, entry(removing)) : "module"}?`}
  confirmLabel="Remove"
  onconfirm={doRemove}
>
  <p>
    This unloads the module and deletes its files from the data folder. Your node and its chain data are not touched. You
    can install it again later.
  </p>
</Confirm>

<style>
  .name {
    font-size: 1rem;
    color: var(--color-cream);
  }
  .ver {
    margin-left: 0.4rem;
    font-size: 0.8rem;
    color: var(--color-faint);
  }
  .row-act {
    display: flex;
    flex-shrink: 0;
    align-items: center;
    gap: 0.6rem;
  }
  @media (max-width: 767px) {
    /* The name/description block takes the full row; the switch and Remove wrap below it when it is tight. */
    .ui-list > li > :global(.min-w-0) {
      flex: 1 1 12rem;
      overflow-wrap: anywhere;
    }
    .row-act {
      flex-shrink: 1;
      flex-wrap: wrap;
    }
  }
</style>
