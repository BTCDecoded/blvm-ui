<script lang="ts">
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import Ring from "./Ring.svelte";
  import { ui } from "./status.svelte";

  const s = $derived(ui.snap);
  const health = $derived(s.health || "dead");
  const arrival = $derived(s.arrival?.length ? s.arrival : Array(12).fill(0));
  const maxBar = $derived(Math.max(1, ...arrival));
  const inPeak = $derived(Math.max(1, s.inbound, s.outbound));
  const haveVol = $derived(Number(s.disk_total_bytes) > 0);

  const healthPath = $derived(
    health === "dead"
      ? "M9 9l6 6M15 9l-6 6"
      : health === "heal"
        ? "M8 12h8"
        : "M8 12.5l2.6 2.6L16.5 9.5",
  );
  const healthColor = $derived(
    health === "good" ? "text-good" : health === "heal" ? "text-heal" : "text-dead",
  );
</script>

<section class="insights-stack">
  <div class="insights-top">
    <article class="panel card">
      <div class="card-head">
        <h2><Icon name="node" class="h-5 w-5 text-muted" /> System Health</h2>
        <span class={healthColor} title={s.health_label}>
          <svg viewBox="0 0 24 24" width="24" height="24" aria-hidden="true">
            <circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="1.6"></circle>
            <path
              d={healthPath}
              fill="none"
              stroke="currentColor"
              stroke-width="1.8"
              stroke-linecap="round"
              stroke-linejoin="round"
            ></path>
          </svg>
        </span>
      </div>
      <dl class="health-list">
        <div>
          <dt>Node Status</dt>
          <dd class="flex items-center gap-2"><Light health={health} /> {s.node_status}</dd>
        </div>
        <div>
          <dt>Uptime</dt>
          <dd>{s.uptime}</dd>
        </div>
        <div>
          <dt>BLVM</dt>
          <dd>{s.blvm_version}</dd>
        </div>
        <div>
          <dt>Network</dt>
          <dd>
            <span class="rounded-full border border-line px-2.5 py-0.5 text-[0.85rem]">{s.network}</span>
          </dd>
        </div>
        <div>
          <dt>IBD Status</dt>
          <dd class={s.ibd || s.frozen ? "text-orange" : ""}>
            {s.frozen && s.ibd ? "Frozen" : s.ibd ? "Active" : "Idle"}
          </dd>
        </div>
      </dl>
    </article>

    <article class="panel card">
      <div class="card-head">
        <h2><Icon name="network" class="h-5 w-5 text-muted" /> Connected Peers</h2>
        <span class="text-[1.1375rem] font-medium text-cream">{s.peers}</span>
      </div>
      <div class="peer-bars">
        <div>
          <span class="flex items-center gap-2">
            <i class="inline-block h-[0.65rem] w-[0.65rem] rounded-full bg-inbound"></i>Inbound
          </span>
          <div class="h-2 overflow-hidden rounded-full bg-faint/40">
            <i class="block h-full rounded-full bg-inbound" style="width:{(s.inbound / inPeak) * 100}%"></i>
          </div>
          <strong>{s.inbound}</strong>
        </div>
        <div>
          <span class="flex items-center gap-2">
            <i class="inline-block h-[0.65rem] w-[0.65rem] rounded-full bg-outbound"></i>Outbound
          </span>
          <div class="h-2 overflow-hidden rounded-full bg-faint/40">
            <i class="block h-full rounded-full bg-outbound" style="width:{(s.outbound / inPeak) * 100}%"></i>
          </div>
          <strong>{s.outbound}</strong>
        </div>
      </div>
      <p class="listen-line">
        <Light health={s.accepting_inbound && s.connected ? "good" : "dead"} />
        {s.frozen
          ? "RPC Frozen"
          : s.accepting_inbound && (s.connected || s.node_running)
            ? "Accepting inbound connections"
            : "Not listening"}
      </p>
    </article>

    <article class="panel card">
      <div class="card-head">
        <h2><Icon name="cube" class="h-5 w-5 text-muted" /> Disk Footprint</h2>
        <span class="text-right leading-tight">
          <strong class="text-[1.1375rem] font-medium text-cream"
            >{!s.disk_used_num || s.disk_used_num === "—" ? "0" : s.disk_used_num}</strong
          >
          <small class="text-muted">{s.disk_used_unit || "GB"}</small>
          <span class="block text-[0.7rem] font-medium tracking-wide text-muted uppercase">Chain Size</span>
        </span>
      </div>
      <div class="disk-body">
        <Ring
          mode="disk"
          free={Number(s.disk_free_bytes) || 0}
          total={Number(s.disk_total_bytes) || 0}
          centerValue={haveVol
            ? !s.disk_free_num || s.disk_free_num === "—"
              ? "0"
              : s.disk_free_num
            : "—"}
          centerUnit={(s.disk_free_unit || "GB") + " free"}
          class="h-[10.5rem] w-[10.5rem] shrink-0"
        />
        <ul class="disk-legend">
          <li>
            <span><i class="inline-block h-[0.65rem] w-[0.65rem] rounded-full bg-orange"></i>Used</span>
            <strong>{s.disk_vol_used_label || "—"}</strong>
          </li>
          <li>
            <span><i class="inline-block h-[0.65rem] w-[0.65rem] rounded-full bg-good"></i>Free</span>
            <strong>{s.disk_free_label || "—"}</strong>
          </li>
        </ul>
      </div>
    </article>
  </div>

  <article class="panel card arrival">
    <div class="card-head">
      <h2><Icon name="wave" class="h-5 w-5 text-muted" /> Block Arrival</h2>
      <span class="text-[0.8125rem] font-medium tracking-wide text-muted uppercase">Last 12 Min</span>
    </div>
    <div class="bars">
      {#each arrival as v, i}
        <div class="bar-slot">
          {#if v > 0}<span class="bar-count">{v}</span>{/if}
          <b
            class="bar {i === arrival.length - 1 ? 'bg-orange' : 'bg-orange/45'}"
            style="height:{Math.max(3, (v / maxBar) * 100)}%"
          ></b>
        </div>
      {/each}
    </div>
    <div class="mt-3 flex justify-between text-[0.85rem] text-faint">
      <span>12m ago</span><span>Now</span>
    </div>
  </article>
</section>

<style>
  .insights-stack {
    display: flex;
    flex: 1 1 auto;
    min-height: 0;
    flex-direction: column;
    gap: 0.75rem;
  }
  .insights-top {
    display: grid;
    gap: 0.75rem;
  }
  @media (min-width: 1024px) {
    .insights-top {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }
  }
  .card {
    display: flex;
    min-width: 0;
    flex-direction: column;
    padding: 1.625rem;
  }
  .card-head {
    margin-bottom: 1.1rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
  }
  .card-head h2 {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 1.1375rem;
    font-weight: 500;
    color: var(--color-cream);
  }
  .health-list {
    font-size: 1.02rem;
  }
  .health-list > div {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0.7rem 0;
    border-top: 1px solid var(--color-line);
  }
  .health-list > div:first-child {
    border-top: 0;
  }
  .health-list dt {
    color: var(--color-muted);
  }
  .peer-bars {
    display: grid;
    gap: 1.1rem;
    font-size: 1.02rem;
  }
  .peer-bars > div {
    display: grid;
    grid-template-columns: 6.5rem 1fr 2.25rem;
    align-items: center;
    gap: 0.75rem;
  }
  .peer-bars span {
    color: var(--color-muted);
  }
  .peer-bars strong {
    text-align: right;
    font-size: 1.2rem;
    font-weight: 500;
  }
  .listen-line {
    margin-top: auto;
    padding-top: 1.25rem;
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 1.02rem;
    color: var(--color-muted);
  }
  .disk-body {
    display: flex;
    flex: 1 1 auto;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 1.1rem;
  }
  .disk-legend {
    display: grid;
    grid-template-columns: auto auto;
    justify-content: center;
    column-gap: 0.9rem;
    row-gap: 0.45rem;
    font-size: 1.02rem;
    color: var(--color-muted);
  }
  .disk-legend li {
    display: contents;
  }
  .disk-legend span {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .disk-legend strong {
    justify-self: end;
    font-weight: 500;
    color: var(--color-cream);
  }
  .arrival {
    flex: 1 1 auto;
    min-height: 12rem;
  }
  .bars {
    display: flex;
    flex: 1 1 auto;
    min-height: 6rem;
    align-items: stretch;
    gap: 0.5rem;
  }
  .bar-slot {
    display: flex;
    flex: 1 1 0;
    flex-direction: column;
    justify-content: flex-end;
    align-items: center;
    gap: 0.35rem;
  }
  .bar {
    display: block;
    width: 100%;
    border-radius: 0.4rem 0.4rem 0.15rem 0.15rem;
  }
  .bar-count {
    font-size: 0.8rem;
    color: var(--color-muted);
  }
</style>
