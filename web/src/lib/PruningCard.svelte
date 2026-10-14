<script lang="ts">
  import { untrack } from "svelte";
  import { ApiError, getSettings, nodeAction, patchSettings } from "./api";
  import Confirm from "./Confirm.svelte";
  import { fmt } from "./format";
  import Icon from "./Icon.svelte";
  import { fmtBytes, networkById } from "./networks";
  import PrunePicker from "./PrunePicker.svelte";
  import { keepMax, PRUNE_DEFAULT, pendingText, storageText, storageView, toKeep, validKeep, type StorageChoice } from "./pruning";
  import { ui } from "./status.svelte";
  import type { ConsoleSettings } from "./types";

  // The managed "Blocks to keep" card: block pruning for the network the node runs on. The node reads the setting
  // at start, so a change applies at the next start or after Restart. Going from a pruned node back to a full one
  // needs the chain data deleted (the node would otherwise offer blocks it no longer has); the host refuses that
  // until it is confirmed, and this card asks first.
  const s = $derived(ui.snap);
  const m = $derived(s.managed!);
  const net = $derived(networkById(m.network));
  // The network id as a plain string: `m` is a new object on every status poll, a string is only "new" when the
  // network really changes. The settings effect below depends on this and on nothing else.
  const netId = $derived(m?.network ?? null);
  // The node is up or on its way up or down: the chain data cannot be deleted until it has stopped.
  const active = $derived(m.process.state === "running" || m.process.state === "starting" || m.process.state === "stopping");
  const agentGone = $derived(m.agent.external && !m.agent.alive);
  const row = $derived(m.networks.find((n) => n.id === net?.id));
  // The host is deleting this network's chain data right now (it keeps going when a Save request times out).
  const wiping = $derived(row?.wiping === true);

  const FIRST_STOP_NOTE = "The node must be stopped first.";
  const CONVERT_WARNING =
    "The node converts the existing store on its next start: it deletes old blocks one by one from the start of the chain and does not answer meanwhile. Minutes on Testnet4, possibly hours on mainnet.";

  let settings = $state<ConsoleSettings>({});
  let loaded = $state(false);
  let choice = $state<StorageChoice>("full");
  let keep = $state<number | null>(PRUNE_DEFAULT);
  let saving = $state(false);
  let err = $state("");
  let warning = $state("");
  let note = $state("");
  let savedNote = $state(false);

  const known = $derived(net ? keepMax(net, s) : { max: 0, live: false });
  const stored = $derived<number | null>(net ? (settings.prune_by_network?.[net.id] ?? null) : null);
  // `undefined`: a pruned choice with an unusable number (never read as "full node").
  const value = $derived(toKeep(choice, keep));
  const valid = $derived(value !== undefined);
  const dirty = $derived(valid && value !== stored);
  const effective = $derived(settings.prune_effective?.network === net?.id ? settings.prune_effective : null);
  // Chain data on disk: the host's live answer, else what the status says about the folder.
  const hasData = $derived(effective ? effective.has_data : !!row?.has_data);
  // Chain data that a pruning node has worked on: only that makes "back to a full node" a deletion. (A host that
  // does not say, or a setting changed behind this page's back, is caught by the host's 409 `needs: "wipe"`.)
  const hasPrunedData = $derived(!!effective?.has_pruned_data);
  const lowering = $derived(typeof value === "number" && (stored === null || value < stored));
  const removing = $derived(value === null && stored !== null);
  // What the running node does now (its own answer), next to the saved setting.
  const storage = $derived(storageView(s.pruning, true));
  const nodeAnswers = $derived(s.pruning?.node_enabled != null || s.pruning?.node_reports_pruned != null);

  // Read the settings when the card opens and again whenever the node's network changes (no polling): what
  // the card shows has to belong to the network it names. `seq` is not reactive on purpose.
  let seq = 0;
  function reload() {
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
  }
  $effect(() => {
    void netId;
    untrack(reload);
  });
  // The host finished deleting (it keeps going when the Save request failed on the way, and `err` is hidden while
  // `wiping` is true): drop the stale error and read the settings the host saved after the delete.
  let wasWiping = false;
  $effect(() => {
    const now = wiping;
    untrack(() => {
      if (wasWiping && !now) reload();
      wasWiping = now;
    });
  });

  // Fill the form from what is stored: when the settings arrive, and when the node's network changes.
  let filledFor = "";
  $effect(() => {
    if (!loaded || !net) return;
    const key = `${net.id}:${seq}`;
    if (filledFor === key) return;
    filledFor = key;
    choice = stored === null ? "full" : "pruned";
    keep = stored ?? PRUNE_DEFAULT;
  });

  async function patch(confirmWipe: boolean) {
    if (!net || value === undefined) return false;
    saving = true;
    err = "";
    warning = "";
    note = "";
    savedNote = false;
    try {
      const d = await patchSettings({ prune_by_network: { [net.id]: value }, ...(confirmWipe ? { confirm_wipe: true } : {}) });
      settings = { ...settings, ...d };
      warning = d.warning ?? "";
      note = d.note ?? "";
      savedNote = true;
      return true;
    } catch (e) {
      err = e instanceof Error ? e.message : "Could not save that setting.";
      // The host wants the data deleted first (it may know of data this page did not): ask.
      if (e instanceof ApiError && e.needs === "wipe") wipeOpen = true;
      return false;
    } finally {
      saving = false;
    }
  }

  // What the Save button does: confirm first where the change costs something.
  function save() {
    if (!net || saving || wiping || !valid || !dirty) return;
    if (removing && hasPrunedData) {
      err = "";
      wipeOpen = true;
    } else if (lowering && hasData) {
      err = "";
      convertOpen = true;
    } else {
      void patch(false);
    }
  }

  let convertOpen = $state(false);
  async function convert() {
    if (await patch(false)) convertOpen = false;
  }

  let wipeOpen = $state(false);
  let stopping = $state(false);
  // A delete in progress answers the question the dialog asked: close it and show the state on the card.
  $effect(() => {
    if (wiping) wipeOpen = false;
  });
  async function wipe() {
    if (await patch(true)) wipeOpen = false;
  }
  async function stopNode() {
    if (stopping) return;
    stopping = true;
    err = "";
    try {
      const d = await nodeAction("stop");
      if (d.status) ui.snap = d.status;
      if (d.error) err = d.error;
    } catch (e) {
      err = e instanceof Error ? e.message : "Could not stop the node.";
    } finally {
      stopping = false;
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
    <h2 class="ui-title"><Icon name="cube" class="h-5 w-5 text-muted" /> Blocks to keep</h2>
  </div>
  {#if !net}
    <p class="ui-lead">No network has been chosen yet. Run setup to pick one.</p>
  {:else}
    <p class="ui-lead">
      A full node keeps every block ever made. A pruned node keeps the newest blocks only and deletes older ones after
      they are verified; it still verifies everything and serves recent blocks, but not old history. Setting for
      <strong class="text-cream">{net.label}</strong>.
    </p>
    {#if !loaded}
      <p class="ui-note mt-3">{err || "Loading…"}</p>
    {:else}
      <PrunePicker {net} bind:choice bind:keep max={known.max} live={known.live} id="settings-prune" />
      {#if removing && hasPrunedData}
        <p class="ui-note ui-warn mt-3">
          Going back to a full node means downloading the whole chain again: the console deletes this network's chain
          data first.
        </p>
      {:else if lowering && hasData && dirty}
        <p class="ui-note ui-warn mt-3">{CONVERT_WARNING}</p>
      {/if}
      {#if wiping}
        <p class="ui-note ui-warn wiping mt-3" role="status">
          <span class="ring" aria-hidden="true"></span>
          <span><strong>Deleting the chain data…</strong> The node downloads the chain again afterwards. Saving is on hold until this is done.</span>
        </p>
      {/if}
      <div class="actions">
        <button type="button" class="ui-btn" disabled={saving || wiping || !valid || !dirty} onclick={save}>
          {saving ? "Saving…" : wiping ? "Deleting…" : "Save"}
        </button>
        {#if savedNote && !dirty}<span class="ui-note" role="status">Saved.</span>{/if}
      </div>
      {#if warning}<p class="ui-note ui-warn mt-3" role="status">{warning}</p>{/if}
      {#if note}<p class="ui-note mt-3" role="status">{note}</p>{/if}
      {#if active || nodeAnswers}
        <p class="ui-sub mb-2 mt-4">The node is running with</p>
        <code class="ui-code" aria-label="Pruning the running node reports"
          >{nodeAnswers ? storageText(storage) : `${storageText(storage)} (the node does not answer yet)`}</code
        >
        {#if storage.pending}<p class="mt-2"><span class="ui-chip">{pendingText(storage)}</span></p>{/if}
      {/if}
      {#if effective}
        <p class="ui-sub mb-2 mt-4">The node will start with</p>
        <code class="ui-code" aria-label="Pruning the node starts with">{effective.keep_blocks
            ? `pruning: keeps the newest ${fmt(effective.keep_blocks)} blocks`
            : "no pruning: keeps every block"}</code>
      {/if}
      {#if active}
        <div class="restart">
          <p class="ui-note min-w-0">Applies at the next start.</p>
          <button type="button" class="ui-btn" disabled={restarting || wiping || agentGone || m.switch?.phase === "stopping" || m.switch?.phase === "starting"} onclick={() => (confirmOpen = true)}>
            Restart now
          </button>
        </div>
      {/if}
      <!-- While the delete runs, a timed-out Save ("Could not save") is not news: the delete goes on. -->
      {#if err && !wiping}<p class="ui-note ui-bad mt-3" role="alert">{err}</p>{/if}
    {/if}
  {/if}
</article>

<Confirm bind:open={convertOpen} title="Convert the existing chain data?" confirmLabel="Save" busy={saving} disabled={wiping} onconfirm={convert}>
  <p>{CONVERT_WARNING}</p>
  <p class="mt-2">Blocks that are deleted do not come back unless you download the chain again.</p>
  {#if err}<p class="ui-bad">{err}</p>{/if}
</Confirm>

<Confirm
  bind:open={wipeOpen}
  title="Delete the chain data for {net?.label ?? 'this network'}?"
  confirmLabel="Delete and download again"
  busy={saving}
  disabled={active || stopping || wiping || agentGone}
  onconfirm={wipe}
>
  <p>
    The node cannot go back to a full node without downloading the chain again. The console deletes
    {row && row.bytes > 0 ? `${fmtBytes(row.bytes)} of` : "the"} pruned chain data for {net?.label ?? "this network"}
    and the node downloads everything again. {FIRST_STOP_NOTE}
  </p>
  {#if agentGone}
    <p class="ui-bad mt-3">The node agent is not answering, so the console cannot confirm that the node is stopped.</p>
  {/if}
  {#if active}
    <p class="mt-3">
      <button type="button" class="ui-btn-ghost" disabled={stopping || agentGone} onclick={stopNode}>
        {stopping ? "Stopping…" : "Stop the node"}
      </button>
    </p>
  {/if}
  {#if err}<p class="ui-bad">{err}</p>{/if}
</Confirm>

<Confirm bind:open={confirmOpen} title="Restart the node?" confirmLabel="Restart now" busy={restarting} onconfirm={restart}>
  <p>Restart the node to apply the new setting? The node will be unavailable for a minute.</p>
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
  .wiping {
    display: flex;
    align-items: flex-start;
    gap: 0.6rem;
  }
  .wiping strong {
    font-weight: 500;
  }
  .ring {
    flex: none;
    margin-top: 0.15rem;
    width: 1rem;
    height: 1rem;
    border-radius: 999px;
    border: 2px solid rgb(240 230 208 / 0.18);
    border-top-color: var(--color-orange);
    animation: wipe-spin 0.9s linear infinite;
  }
  @keyframes wipe-spin {
    to {
      transform: rotate(360deg);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .ring {
      animation-duration: 3s;
    }
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
