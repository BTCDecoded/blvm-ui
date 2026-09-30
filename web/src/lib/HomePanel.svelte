<script lang="ts">
  import BlockRail from "./BlockRail.svelte";
  import Globe, { type GlobeHover, type GlobePeer } from "./Globe.svelte";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import Ring from "./Ring.svelte";
  import { behindLine, fmt, simpleLabel } from "./format";
  import { ui } from "./status.svelte";

  const s = $derived(ui.snap);
  const pct = $derived(
    s.network_height ? Math.min(100, (s.local_height / s.network_height) * 100) : 0,
  );
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
          <div class="mt-[1.3rem] h-[0.4875rem] max-w-xl overflow-hidden rounded-full bg-faint/30">
            <i class="block h-full rounded-full bg-orange" style="width:{pct}%"></i>
          </div>
        </div>

        <div class="heights-strip" class:is-stale={s.frozen}>
          <div class="min-w-0">
            <span class="flex items-center gap-1.5 text-[0.845rem] font-medium tracking-wide text-muted uppercase">
              <Icon name="pin" class="h-[0.975rem] w-[0.975rem]" />
              Local Height
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
    flex: 1 1 auto;
    min-height: 0;
    flex-direction: column;
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
    grid-template-columns: repeat(3, minmax(0, 1fr));
    gap: 0.975rem;
    border-radius: 0.975rem;
    background: rgb(5 5 5 / 0.55);
    padding: 0.975rem 1.625rem;
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
</style>
