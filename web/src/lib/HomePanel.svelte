<script lang="ts">
  import { untrack } from "svelte";
  import BlockRail from "./BlockRail.svelte";
  import Globe, { type GlobeHover, type GlobePeer } from "./Globe.svelte";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import Ring from "./Ring.svelte";
  import { behindLine, fmt, simpleLabel } from "./format";
  import { blocksStoredText, pendingText, storageText, storageView } from "./pruning";
  import { ui } from "./status.svelte";
  import { flatFor, pushSample, rateOf, rateText, STALL_MS, type Sample } from "./syncrate";

  const s = $derived(ui.snap);
  // Block pruning: shown for a node this console runs (it knows the setting) and for any node that says it is
  // pruned. The heights above stay the validated chain; pruning only changes how many old blocks are kept.
  const sv = $derived(storageView(s.pruning, !!s.managed));
  const showStored = $derived(!!s.managed || sv.pruned);
  const pct = $derived(
    s.network_height ? Math.min(100, (s.local_height / s.network_height) * 100) : 0,
  );
  // `s.local_height` is the raw RPC value: the node's `blocks`, the last block it has fully validated and connected
  // (max of getblockchaininfo.blocks and getblockcount). Nothing here smooths, counts up or fills in between
  // polls. The rate and the "waiting" hint below are worked out from the heights of the last 30 s of polls and
  // are shown next to the figure; they never change it.
  let samples = $state<Sample[]>([]);
  let sampleRpc = "";
  $effect(() => {
    // One run per status poll (each poll is a new snapshot object), also when the height has not moved.
    const snap = ui.snap;
    untrack(() => {
      if (snap.rpc_addr !== sampleRpc) {
        sampleRpc = snap.rpc_addr;
        samples = [];
      }
      // Only a node that answers has a height worth timing; a gap in the polls must not read as "flat".
      if (!snap.connected || snap.frozen) {
        samples = [];
        return;
      }
      samples = pushSample(samples, Date.now(), snap.local_height);
    });
  });
  const syncing = $derived(s.ibd && s.connected && !s.frozen);
  // Rate: only while the first sync runs. While the waiting hint shows (the height has been flat long enough), the
  // hint is shown instead of the rate: the 30 s average only reaches 0 after the window empties.
  const waiting = $derived(syncing && flatFor(samples) >= STALL_MS);
  const rate = $derived(syncing && !waiting ? rateText(rateOf(samples)) : null);
  const num = $derived(s.sync_pct_num && s.sync_pct_num !== "—" ? s.sync_pct_num : "0");

  const globePeers = $derived.by((): GlobePeer[] => [
    ...(s.self_geo && Number.isFinite(s.self_geo.lat) && Number.isFinite(s.self_geo.lon)
      ? [
          {
            key: "__self__",
            lat: s.self_geo.lat,
            lon: s.self_geo.lon,
            inbound: false,
            self: true,
            label: "You!",
            detail:
              s.self_geo.source === "timezone"
                ? `Near ${s.self_geo.city ?? "you"} (time zone)`
                : [s.self_geo.city, s.self_geo.country].filter(Boolean).join(", "),
          },
        ]
      : []),
    ...(s.peer_rows || [])
      .filter((p) => p.geo && Number.isFinite(p.geo.lat) && Number.isFinite(p.geo.lon))
      .map((p) => ({
        key: p.addr,
        lat: p.geo!.lat,
        lon: p.geo!.lon,
        inbound: p.inbound,
        label: p.addr,
        detail: [p.geo!.city, p.geo!.country].filter(Boolean).join(", "),
      })),
  ]);
  let hover = $state<GlobeHover>(null);

  const phase = $derived.by(() => {
    if (s.frozen) return "Frozen";
    // Only a node that is answering can be Synced / Syncing.
    if (!s.connected) return simpleLabel(s);
    if (s.ibd || s.syncing) return "Syncing";
    if (s.behind === 0) return "Synced";
    return simpleLabel(s);
  });
</script>

<section class="home-stack">
  <article class="panel sync-card relative overflow-hidden">
    <div class="globe-back" aria-hidden="true">
      <Globe peers={globePeers} onhover={(h) => (hover = h)} />
    </div>
    <div class="globe-fade" aria-hidden="true"></div>
    <div class="sync-inner p-[1.625rem] sm:p-[1.95rem]">
    <div class="sync-grid relative z-10">
      <div class="sync-main">
        <div class="mb-[0.975rem] flex items-center gap-2 text-[1.1375rem] font-medium text-cream">
          <Icon name="insights" class="h-5 w-5 text-muted" />
          Network Sync
        </div>
        <div class="sync-readout" class:is-stale={s.frozen}>
          <p class="flex items-end gap-1 leading-none">
            <span class="text-[5.2rem] font-medium tracking-tight sm:text-[6.175rem]">{num}</span>
            <small class="mb-2 text-[1.95rem] text-muted">%</small>
          </p>
          <p class="mt-[0.65rem] text-[1.1375rem] text-muted">{behindLine(s)}</p>
          {#if rate}
            <p class="ui-sub sync-note" aria-label="Sync speed">{rate}</p>
          {/if}
          {#if waiting}
            <p class="ui-sub sync-note" role="status">
              Waiting for a missing block from a peer; the queue continues when it arrives
            </p>
          {/if}
          <div class="mt-[1.3rem] h-[0.4875rem] max-w-xl overflow-hidden rounded-full bg-faint/30">
            <i class="block h-full rounded-full bg-orange" style="width:{pct}%"></i>
          </div>
        </div>

        <div class="heights-strip" class:is-stale={s.frozen}>
          <div class="min-w-0" title="Blocks the node has fully validated and connected (its blocks count)">
            <span class="flex items-center gap-1.5 text-[0.845rem] font-medium tracking-wide text-muted uppercase">
              <Icon name="pin" class="h-[0.975rem] w-[0.975rem]" />
              Validated height
            </span>
            <strong class="mt-1 block truncate text-[1.4625rem] font-medium">{fmt(s.local_height)}</strong>
          </div>
          <div class="min-w-0">
            <span class="flex items-center gap-1.5 text-[0.845rem] font-medium tracking-wide text-muted uppercase">
              <Icon name="link" class="h-[0.975rem] w-[0.975rem]" />
              Network Height
            </span>
            <strong class="mt-1 block truncate text-[1.4625rem] font-medium">
              {s.network_height ? fmt(s.network_height) : "—"}
            </strong>
          </div>
          <div class="min-w-0">
            <span class="flex items-center gap-1.5 text-[0.845rem] font-medium tracking-wide text-muted uppercase">
              <Icon name="peers" class="h-[0.975rem] w-[0.975rem]" />
              Peers
            </span>
            <strong class="mt-1 block truncate text-[1.4625rem] font-medium">{s.peers}</strong>
          </div>
        </div>
        {#if showStored}
          <div class="stored-line" class:is-stale={s.frozen} aria-label="Blocks stored">
            <span class="flex items-center gap-1.5 text-[0.845rem] font-medium tracking-wide text-muted uppercase">
              <Icon name="cube" class="h-[0.975rem] w-[0.975rem]" />
              Blocks stored
            </span>
            <strong class="text-[1.1rem] font-medium">{blocksStoredText(sv, s.local_height)}</strong>
            {#if sv.pruned}
              <span class="ui-chip">{storageText(sv)}</span>
              {#if sv.stored !== null}<span class="stored-src">{sv.source === "verified" ? "verified" : "estimated"}</span>{/if}
            {/if}
            {#if sv.pending}
              <span class="ui-chip">{pendingText(sv)}</span>
            {/if}
          </div>
        {/if}
      </div>

      <aside class="p2p-float">
        <div class="flex items-center justify-between gap-3">
          <h2 class="flex items-center gap-2 text-[1.1375rem] font-medium text-cream">
            <Icon name="network" class="h-5 w-5 text-muted" />
            P2P Connections
          </h2>
          <span class="flex items-center gap-2 text-[0.8125rem] font-medium tracking-wide text-muted uppercase">
            <Light health={s.health} />
            {phase}
          </span>
        </div>
        <div class="p2p-body">
          <Ring
            inbound={s.inbound}
            outbound={s.outbound}
            centerValue={String(s.peers)}
            centerUnit="Peers"
            class="h-[12.35rem] w-[12.35rem] max-h-[68%] shrink"
          />
          <ul class="p2p-stats">
            <li>
              <span>
                <i class="inline-block h-[0.65rem] w-[0.65rem] rounded-full bg-outbound"></i>
                Outbound
              </span>
              <strong>{s.outbound}</strong>
            </li>
            <li>
              <span>
                <i class="inline-block h-[0.65rem] w-[0.65rem] rounded-full bg-inbound"></i>
                Inbound
              </span>
              <strong>{s.inbound}</strong>
            </li>
          </ul>
        </div>
      </aside>
    </div>
    </div>
  </article>

  <BlockRail />
</section>

{#if hover}
  <div class="peer-bubble" style="left:{hover.x}px; top:{hover.y}px" role="tooltip">
    {#if hover.peer.self}
      <span class="flex items-center gap-2">
        <i class="h-2 w-2 rounded-full bg-good"></i>
        <strong class="text-[0.9rem]">You! 🤗</strong>
      </span>
      {#if hover.peer.detail}<span class="text-muted">{hover.peer.detail}</span>{/if}
    {:else}
      <span class="flex items-center gap-2">
        <i class="h-2 w-2 rounded-full {hover.peer.inbound ? 'bg-inbound' : 'bg-outbound'}"></i>
        <strong class="font-mono">{hover.peer.label}</strong>
      </span>
      <span class="text-muted">
        {hover.peer.inbound ? "Inbound" : "Outbound"}{hover.peer.detail ? ` · ${hover.peer.detail}` : ""}
      </span>
    {/if}
  </div>
{/if}

<style>
  .peer-bubble {
    position: fixed;
    z-index: 50;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    transform: translate(-50%, calc(-100% - 14px));
    border: 1px solid rgb(240 230 208 / 0.18);
    border-radius: 0.7rem;
    background: rgb(12 12 12 / 0.92);
    padding: 0.45rem 0.7rem;
    font-size: 0.8rem;
    white-space: nowrap;
    color: var(--color-cream);
    box-shadow: 0 10px 28px rgb(0 0 0 / 0.55);
    backdrop-filter: blur(6px);
    pointer-events: none;
  }
  .peer-bubble::after {
    content: "";
    position: absolute;
    left: 50%;
    bottom: -5px;
    width: 9px;
    height: 9px;
    transform: translateX(-50%) rotate(45deg);
    border-right: 1px solid rgb(240 230 208 / 0.18);
    border-bottom: 1px solid rgb(240 230 208 / 0.18);
    background: rgb(12 12 12 / 0.92);
  }
  .home-stack {
    display: flex;
    flex: 1 1 auto;
    min-height: 0;
    flex-direction: column;
    gap: 0.75rem;
  }
  .sync-card {
    display: flex;
    /* Grows with the window but never shrinks below its content: stacked (narrow) the card is taller than 24rem
       and the page scrolls, instead of the panels drawing over each other. */
    flex: 1 0 auto;
    min-height: 24rem;
    flex-direction: column;
    /* The card is its own size container, like the Settings sub-page: the layout follows the room the card really
       has (the window minus the sidebar), not the window width. */
    container-type: inline-size;
  }
  .sync-inner {
    position: relative;
    display: flex;
    box-sizing: border-box;
    flex: 1 1 auto;
    min-height: 0;
    height: 100%;
    flex-direction: column;
  }
  .sync-grid {
    display: grid;
    flex: 1 1 auto;
    min-height: 0;
    height: 100%;
    grid-template-columns: minmax(0, 1fr) minmax(22.1rem, 32.3%);
    align-items: stretch;
    gap: 1.3rem;
  }
  .sync-main {
    display: flex;
    min-width: 0;
    min-height: 0;
    height: 100%;
    flex-direction: column;
  }
  /* Frozen node: last known numbers, dimmed so they read as old. */
  .is-stale :global(span),
  .is-stale :global(strong),
  .is-stale :global(i) {
    opacity: 0.55;
  }
  .sync-readout {
    margin-top: auto;
  }
  .heights-strip {
    margin-top: 1.3rem;
    display: grid;
    max-width: 36rem;
    /* Three across when they fit; two and one below on a phone, so no figure is cut to "712,...". */
    grid-template-columns: repeat(auto-fit, minmax(7rem, 1fr));
    gap: 0.975rem;
    border-radius: 0.975rem;
    background: rgb(5 5 5 / 0.55);
    padding: 0.975rem 1.625rem;
  }
  .stored-line {
    margin-top: 0.65rem;
    display: flex;
    max-width: 36rem;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.35rem 0.85rem;
    border-radius: 0.975rem;
    background: rgb(5 5 5 / 0.55);
    padding: 0.7rem 1.625rem;
  }
  .sync-note {
    margin-top: 0.55rem;
    max-width: 36rem;
    /* A sentence, not a label: keep the small-print look but not the capitals. */
    text-transform: none;
    color: var(--color-muted);
    letter-spacing: 0.02em;
    line-height: 1.4;
  }
  .stored-src {
    font-size: 0.78rem;
    font-weight: 500;
    letter-spacing: 0.06em;
    text-transform: uppercase;
    color: var(--color-faint);
  }
  .globe-back {
    pointer-events: none;
    position: absolute;
    z-index: 0;
    top: -28%;
    bottom: -36%;
    left: 2%;
    right: 0;
    transform: translateX(10%) translateY(5.5%) scale(0.94);
    mask-image: linear-gradient(90deg, transparent 0%, rgb(0 0 0 / 0.45) 10%, #000 22%, #000 100%);
    -webkit-mask-image: linear-gradient(
      90deg,
      transparent 0%,
      rgb(0 0 0 / 0.45) 10%,
      #000 22%,
      #000 100%
    );
  }
  .globe-fade {
    pointer-events: none;
    position: absolute;
    inset: 0;
    z-index: 1;
    background: linear-gradient(90deg, var(--color-card) 0%, rgb(12 12 12 / 0.45) 8%, transparent 22%);
  }
  .p2p-float {
    display: flex;
    box-sizing: border-box;
    height: 90%;
    margin-top: 10%;
    min-height: 0;
    flex-direction: column;
    align-self: end;
    border: 1px solid var(--color-line);
    background: rgb(12 12 12 / 0.72);
    border-radius: 1.365rem;
    padding: 1.378rem 1.547rem;
  }
  .p2p-body {
    display: flex;
    min-height: 0;
    flex: 1 1 auto;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 1.1rem;
    padding-top: 0.6rem;
  }
  .p2p-stats {
    display: grid;
    grid-template-columns: auto auto;
    justify-content: center;
    width: 100%;
    flex-shrink: 0;
    column-gap: 0.6rem;
    row-gap: 0.65rem;
    font-size: 1.1375rem;
    color: var(--color-muted);
  }
  .p2p-stats li {
    display: contents;
  }
  .p2p-stats span {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .p2p-stats strong {
    justify-self: end;
    font-size: 1.4625rem;
    color: var(--color-cream);
  }
  /* Side by side only while the card keeps room for both (px, not rem: the root font size scales with the
     window). Below that the P2P panel goes under the sync figures, at its own height. */
  @container (max-width: 760px) {
    .sync-grid {
      grid-template-columns: minmax(0, 1fr);
      height: auto;
      align-items: start;
    }
    .sync-main {
      height: auto;
    }
    .sync-readout {
      margin-top: 1.5rem;
    }
    .p2p-float {
      height: auto;
      margin-top: 0;
      align-self: stretch;
    }
    .p2p-body {
      padding-top: 0.9rem;
    }
  }
  @container (min-width: 480px) and (max-width: 760px) {
    .p2p-body {
      flex-direction: row;
      justify-content: space-evenly;
    }
    .p2p-stats {
      width: auto;
    }
  }
  @container (max-width: 479px) {
    .heights-strip {
      padding: 0.8rem 1rem;
    }
    .stored-line {
      padding: 0.7rem 1rem;
    }
  }
</style>
