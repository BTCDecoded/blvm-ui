<script lang="ts">
  import { untrack } from "svelte";
  import { getSettings, nodeAction, patchSettings } from "./api";
  import Confirm from "./Confirm.svelte";
  import Icon from "./Icon.svelte";
  import { networkById } from "./networks";
  import { ui } from "./status.svelte";
  import type { ConsoleSettings } from "./types";
  import ValidationPicker from "./ValidationPicker.svelte";
  import { HASH_NOTE, knownMax, parseSetting, toSetting, validHeight, type ValidationChoice } from "./validation";

  // The managed Validation card: assume-valid for the network the node runs on. The node reads the setting at
  // start, so a change applies at the next start or after Restart.
  const s = $derived(ui.snap);
  const m = $derived(s.managed!);
  const net = $derived(networkById(m.network));
  // The network id as a plain string: `m` is a new object on every status poll, a string is only "new" when the
  // network really changes. The settings effect below depends on this and on nothing else.
  const netId = $derived(m?.network ?? null);
  const active = $derived(m.process.state === "running" || m.process.state === "starting");
  const agentGone = $derived(m.agent.external && !m.agent.alive);

  let settings = $state<ConsoleSettings>({});
  let loaded = $state(false);
  let choice = $state<ValidationChoice>("default");
  let height = $state<number | null>(0);
  let hashSet = $state(false);
  let saving = $state(false);
  let err = $state("");
  let savedNote = $state(false);

  const def = $derived(net ? settings.assume_valid_defaults?.[net.id] : undefined);
  const known = $derived(net ? knownMax(net, s, def) : { max: 0, live: false });
  const stored = $derived(net ? (settings.assume_valid_by_network?.[net.id] ?? settings.assume_valid ?? "default") : "default");
  const value = $derived(toSetting(choice, height));
  const valid = $derived(choice !== "custom" || validHeight(height));
  // What is stored, in the form `toSetting` produces, to tell whether anything changed.
  const storedNorm = $derived.by(() => {
    const p = parseSetting(stored, 0);
    return p.choice === "hash" ? `hash:${stored}` : toSetting(p.choice, p.height);
  });
  const dirty = $derived(value !== storedNorm);
  const effective = $derived(settings.assume_valid_effective?.network === net?.id ? settings.assume_valid_effective : null);

  // Read the settings when the card opens and again whenever the node's network changes (no polling): what
  // the card shows has to belong to the network it names. `seq` is not reactive on purpose.
  let seq = 0;
  $effect(() => {
    void netId;
    untrack(() => {
      const mine = ++seq;
      loaded = false;
      err = "";
      getSettings()
        .then((d) => {
          if (mine !== seq) return;
          settings = d;
          loaded = true;
        })
        .catch(() => {
          if (mine === seq) err = "Could not read the current setting.";
        });
    });
  });

  // Fill the form from what is stored: when the settings arrive, and when the node's network changes.
  let filledFor = "";
  $effect(() => {
    if (!loaded || !net) return;
    const key = `${net.id}:${seq}`;
    if (filledFor === key) return;
    filledFor = key;
    const p = parseSetting(stored, def?.height ?? 0);
    hashSet = p.choice === "hash";
    choice = p.choice === "hash" ? "default" : p.choice;
    height = p.height;
  });

  async function save() {
    if (!net || saving || !valid) return;
    saving = true;
    err = "";
    savedNote = false;
    try {
      const d = await patchSettings({ assume_valid_by_network: { [net.id]: value } });
      settings = { ...settings, ...d };
      hashSet = false;
      savedNote = true;
    } catch (e) {
      err = e instanceof Error ? e.message : "Could not save that setting.";
    } finally {
      saving = false;
    }
  }

  // Restart now (confirmed): the node comes back with the new setting.
  let confirmOpen = $state(false);
  let restarting = $state(false);
  async function restart() {
    if (restarting) return;
    restarting = true;
    err = "";
    try {
      const d = await nodeAction("restart");
      if (d.status) ui.snap = d.status;
      if (d.error) err = d.error;
      else confirmOpen = false;
    } catch (e) {
      err = e instanceof Error ? e.message : "The restart failed.";
    } finally {
      restarting = false;
    }
  }
</script>

<article class="panel ui-card">
  <div class="ui-head">
    <h2 class="ui-title"><Icon name="settings" class="h-5 w-5 text-muted" /> Validation</h2>
  </div>
  {#if !net}
    <p class="ui-lead">No network has been chosen yet. Run setup to pick one.</p>
  {:else}
    <p class="ui-lead">
      During the first sync, blocks below the assume-valid height are accepted without re-checking their scripts and
      signatures; every block above it is fully verified. This makes the first sync much faster. Only use a height you
      trust. Setting for
      <strong class="text-cream">{net.label}</strong>.
    </p>
    {#if !loaded}
      <p class="ui-note mt-3">{err || "Loading…"}</p>
    {:else}
      {#if hashSet}
        <p class="ui-note ui-warn mt-3">{HASH_NOTE}</p>
      {/if}
      <ValidationPicker {net} bind:choice bind:height {def} max={known.max} live={known.live} id="settings-av" />
      <div class="actions">
        <button type="button" class="ui-btn" disabled={saving || !valid || (!dirty && !hashSet)} onclick={save}>
          {saving ? "Saving…" : "Save"}
        </button>
        {#if savedNote && !dirty}<span class="ui-note" role="status">Saved.</span>{/if}
      </div>
      {#if effective}
        <p class="ui-sub mb-2 mt-4">The node will start with</p>
        <code class="ui-code" aria-label="Node arguments for assume-valid">{effective.argv.length ? effective.argv.join(" ") : "no assume-valid option (the node's own default)"}</code>
      {/if}
      {#if effective?.note}<p class="ui-note ui-warn mt-3" role="note">{effective.note}</p>{/if}
      {#if active}
        <div class="restart">
          <p class="ui-note min-w-0">Applies at the next start.</p>
          <button type="button" class="ui-btn" disabled={restarting || agentGone || m.switch?.phase === "stopping" || m.switch?.phase === "starting"} onclick={() => (confirmOpen = true)}>
            Restart now
          </button>
        </div>
      {/if}
      {#if err}<p class="ui-note ui-bad mt-3" role="alert">{err}</p>{/if}
    {/if}
  {/if}
</article>

<Confirm bind:open={confirmOpen} title="Restart the node?" confirmLabel="Restart now" busy={restarting} onconfirm={restart}>
  <p>Restart the node to apply the new assume-valid setting? The node will be unavailable for a minute.</p>
  {#if err}<p class="ui-bad">{err}</p>{/if}
</Confirm>

<style>
  .actions {
    margin-top: 1.25rem;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.75rem 1rem;
  }
  .restart {
    margin-top: 1.25rem;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem 1rem;
  }
</style>
