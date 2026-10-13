<script lang="ts">
  import { isMissing, nodeAction, previewSwitch, switchNetwork } from "./api";
  import Confirm from "./Confirm.svelte";
  import { fmt } from "./format";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import NetCarousel from "./NetCarousel.svelte";
  import { fmtBytes, NETWORKS, networkById, networkLabel, networkText, rpcAddrOf } from "./networks";
  import { switchRunning, ui } from "./status.svelte";
  import type { NetworkId, NetworkPreview } from "./types";

  // Design G.2 + C: the managed Network card. Picking another network asks first, with real numbers; the host
  // does the switch (stop the old node and keep its data, start the other one) and reports its phase.
  const s = $derived(ui.snap);
  const m = $derived(s.managed!);
  const sw = $derived(m.switch);
  const running = $derived(switchRunning(m));
  // A failed switch is "resolved" once the console follows the new network and its node is up again (Try again
  // worked): the box goes away even though the host keeps the record for 30 s.
  const resolved = $derived(
    sw?.phase === "failed" && m.network === sw.to && (m.process.state === "starting" || m.process.state === "running"),
  );
  const shown = $derived(sw && !resolved ? sw : null);
  // No network chosen yet: there is no default, so there is nothing to switch from. Setup picks the first one.
  // A network id this page does not know (a newer host) is neither: nothing to switch from either.
  const none = $derived(m.network === null);
  const unknown = $derived(m.network !== null && !networkById(m.network));
  const chosen = $derived(!none && !unknown);

  const nets = $derived(
    NETWORKS.map((n) => {
      const row = m.networks.find((x) => x.id === n.id);
      return {
        rpc: rpcAddrOf(n),
        chain: n.chain,
        label: n.label,
        detail: row?.has_data ? fmtBytes(row.bytes) : "No data yet",
      };
    }),
  );
  const cur = $derived(networkById(m.network));
  const current = $derived(cur ? rpcAddrOf(cur) : "");

  let open = $state(false);
  let preview = $state<NetworkPreview | null>(null);
  let target = $state<NetworkId | null>(null);
  let loading = $state(false);
  let sending = $state(false);
  let msg = $state("");
  let retrying = $state(false);
  // Bumped when the dialog is cancelled so the carousel snaps back to the active network.
  let carouselKey = $state(0);

  async function onpick(rpc: string) {
    const net = NETWORKS.find((n) => rpcAddrOf(n) === rpc);
    if (!net || net.id === m.network || loading || running) return;
    loading = true;
    msg = "";
    try {
      preview = await previewSwitch(net.id);
      target = net.id;
      open = true;
    } catch (err) {
      msg = isMissing(err)
        ? "Switching networks is not available on this host."
        : err instanceof Error
          ? err.message
          : "Could not check that network.";
      carouselKey++;
    } finally {
      loading = false;
    }
  }

  function why(err: unknown): string {
    // 409 busy: another switch (or a click from another tab) got there first.
    return err instanceof Error ? (err.message === "busy" ? "Another switch is already running." : err.message) : "The switch failed.";
  }

  async function confirm() {
    if (!target || sending) return;
    sending = true;
    try {
      const d = await switchNetwork(target);
      if (d.status) ui.snap = d.status;
      open = false;
    } catch (err) {
      msg = why(err);
      open = false;
      carouselKey++;
    } finally {
      sending = false;
    }
  }

  function cancelled() {
    carouselKey++;
  }

  // The carousel is rebuilt on cancel (so it snaps back to the active network): the element that opened the
  // dialog is gone, so focus goes back to the new front card.
  let wrap: HTMLDivElement | undefined = $state();
  function frontCard(): HTMLElement | null {
    return wrap?.querySelector<HTMLElement>(".net.is-front") ?? null;
  }


  /**
   * "Try again" after a failed switch. If the old network is still the chosen one (the old node would not stop,
   * or the new one could not be spawned) the switch runs again. If the console already follows the new network
   * (its node was spawned and then crashed) the same click starts that node again.
   */
  async function tryAgain() {
    if (!sw || sw.phase !== "failed" || retrying) return;
    retrying = true;
    msg = "";
    try {
      const d = m.network === sw.to ? await nodeAction("start") : await switchNetwork(sw.to);
      if (d.status) ui.snap = d.status;
      if (d.ok === false && d.error) msg = d.error;
    } catch (err) {
      msg = why(err);
    } finally {
      retrying = false;
    }
  }

  // Dialog numbers (design C step 2).
  const fromLabel = $derived(networkLabel(preview?.from));
  const toLabel = $derived(networkLabel(preview?.to));
  // A host from before this field existed answers without `from_height` / `allowed`: fall back to what the status
  // table already says, and to the old `blocked` flag.
  const fromHeight = $derived(
    preview?.from_height ?? m.networks.find((x) => x.id === preview?.from)?.last_height ?? null,
  );
  const allowed = $derived(!!preview && (preview.allowed ?? !preview.blocked));
  const toHasData = $derived(!!preview && (preview.to_bytes > 0 || preview.to_height !== null));
  const short = $derived(!!preview && preview.estimate_bytes !== null && !toHasData && preview.free_bytes < preview.estimate_bytes);

  const phaseText = $derived.by(() => {
    if (!sw) return "";
    switch (sw.phase) {
      case "stopping":
        return `Stopping ${networkLabel(sw.from)} and saving its chain state. This can take a few minutes.`;
      case "starting":
        return `Starting ${networkLabel(sw.to)}. The node can take a minute or two before it answers.`;
      case "done":
        return `Now on ${networkLabel(sw.to)}. ${networkLabel(sw.from)} keeps its data on disk.`;
      default:
        return sw.error || `Could not switch to ${networkLabel(sw.to)}.`;
    }
  });
</script>

<article class="panel ui-card grow">
  <div class="ui-head">
    <h2 class="ui-title"><Icon name="network" class="h-5 w-5 text-muted" /> Network</h2>
    {#if running && shown}
      <span class="ui-chip"><Light health="heal" /> Switching to {networkLabel(shown.to)}</span>
    {:else if shown?.phase === "failed"}
      <span class="ui-chip"><Light health="dead" /> Switch failed</span>
    {:else if chosen}
      <span class="ui-chip"><Light health="good" /> {networkLabel(m.network)}</span>
    {:else}
      <span class="ui-chip"><Light health="dead" /> {networkText(m.network)}</span>
    {/if}
  </div>
  <p class="ui-lead">
    Each network keeps its own data. Picking another one stops the node, keeps everything on disk, and starts the
    other network. You can switch back any time.
  </p>

  {#if shown}
    <div class="phase {shown.phase === 'failed' ? 'is-failed' : ''}" role="status" aria-live="polite">
      {#if shown.phase !== "failed"}
        <ol>
          <li class={shown.phase === "stopping" ? "is-now" : "is-done"}>Stop {networkLabel(shown.from)}</li>
          <li class={shown.phase === "starting" ? "is-now" : shown.phase === "done" ? "is-done" : ""}>Start {networkLabel(shown.to)}</li>
        </ol>
      {/if}
      <p class="ui-note {shown.phase === 'failed' ? 'ui-bad' : ''}">{phaseText}</p>
      {#if shown.phase === "failed"}
        {#if shown.log?.length}
          <p class="ui-sub mb-2 mt-3">Last log lines</p>
          <code class="ui-code ui-log" aria-label="Last log lines">{shown.log.join("\n")}</code>
        {/if}
        <div class="mt-3">
          <button type="button" class="ui-btn" disabled={retrying} onclick={tryAgain}>{retrying ? "Working…" : "Try again"}</button>
        </div>
      {/if}
    </div>
  {/if}

  {#if chosen}
    <div class="carousel-wrap {running ? 'is-locked' : ''}" inert={running} aria-disabled={running} bind:this={wrap}>
      {#key carouselKey}
        <NetCarousel {nets} {current} busy={loading} disabled={running || loading} {onpick} />
      {/key}
    </div>
  {:else if unknown}
    <p class="ui-note ui-warn mt-4">
      Unknown network. The console reports a network this page does not know ("{m.network}"), so it cannot offer a switch.
      <button type="button" class="ui-link" onclick={() => (ui.setupOpen = true)}>Run setup</button>
    </p>
  {:else}
    <p class="ui-note mt-4">
      No network has been chosen yet. The setup wizard picks the first one.
      <button type="button" class="ui-link" onclick={() => (ui.setupOpen = true)}>Run setup</button>
    </p>
  {/if}
  {#if msg}<p class="ui-note ui-bad mt-3">{msg}</p>{/if}
</article>

<Confirm
  bind:open
  title={`Switch to ${toLabel}?`}
  confirmLabel={`Switch to ${toLabel}`}
  busy={sending}
  disabled={!allowed}
  onconfirm={confirm}
  oncancel={cancelled}
  returnTo={frontCard}
>
  {#if preview}
    <p>
      Switching to <strong class="text-cream">{toLabel}</strong> will stop the node on
      <strong class="text-cream">{fromLabel}</strong>
      {#if preview.from_bytes > 0}and keep its data ({fmtBytes(preview.from_bytes)}){:else}and keep whatever it has on disk{/if}.
      {#if toHasData}
        {toLabel} already has data{preview.to_height !== null ? ` (block ${fmt(preview.to_height)})` : ""}; it catches up
        from there.
      {:else if preview.estimate_bytes !== null}
        {toLabel} starts its own sync (about {fmtBytes(preview.estimate_bytes)}, you have {fmtBytes(preview.free_bytes)} free).
      {:else}
        {toLabel} starts its own sync from scratch. Its size varies, and you have
        {fmtBytes(preview.free_bytes)} free.
      {/if}
      You can switch back any time{fromHeight ? `; ${fromLabel} will continue from block ${fmt(fromHeight)}` : ""}.
    </p>
    {#if preview.from_ibd_pct !== null}
      <p>{fromLabel} is {Math.round(preview.from_ibd_pct)}% synced; it resumes where it stopped.</p>
    {/if}
    {#if !allowed}
      <p class="ui-bad">{preview.reason || "Less than 5 GB is free on this disk, so the switch is blocked until you free up space."}</p>
    {:else if short}
      <p class="ui-warn">
        {toLabel} needs about {fmtBytes(preview.estimate_bytes)} and only {fmtBytes(preview.free_bytes)} is free. The sync may
        run out of space.
      </p>
    {/if}
    {#each preview.warnings as w}
      <p class="ui-warn">{w}</p>
    {/each}
  {/if}
</Confirm>

<style>
  .carousel-wrap {
    margin-block: auto;
    padding-top: 1.25rem;
  }
  .phase {
    margin-top: 1.1rem;
    border: 1px solid rgb(232 163 23 / 0.35);
    border-radius: 0.85rem;
    background: rgb(232 163 23 / 0.05);
    padding: 0.85rem 1rem;
  }
  .phase.is-failed {
    border-color: rgb(224 91 85 / 0.45);
    background: rgb(224 91 85 / 0.06);
  }
  .carousel-wrap.is-locked {
    opacity: 0.55;
    filter: grayscale(0.4);
    transition: opacity 0.2s;
  }
  .phase ol {
    display: flex;
    flex-wrap: wrap;
    gap: 0.4rem 1.25rem;
    margin-bottom: 0.4rem;
    font-size: 0.95rem;
    color: var(--color-faint);
  }
  .phase li::before {
    content: "○ ";
  }
  .phase li.is-done {
    color: var(--color-good);
  }
  .phase li.is-done::before {
    content: "✓ ";
  }
  .phase li.is-now {
    color: var(--color-heal);
  }
  .phase li.is-now::before {
    content: "● ";
  }
</style>
