<script lang="ts">
  import { toggleNode } from "./api";
  import Light from "./Light.svelte";
  import Ring from "./Ring.svelte";
  import { ui } from "./status.svelte";

  const s = $derived(ui.snap);
  const health = $derived(s.health || "dead");
  const haveVol = $derived(Number(s.disk_total_bytes) > 0);
  let powerBusy = $state(false);
  let powerHint = $state("Starts a stopped node, or stops a running one.");

  async function onPower() {
    if (powerBusy || s.node_busy) return;
    powerBusy = true;
    try {
      const d = await toggleNode();
      if (d.status) ui.snap = d.status;
      powerHint = d.error || d.message || powerHint;
    } catch (err) {
      powerHint = err instanceof Error ? err.message : "Power request failed.";
    } finally {
      powerBusy = false;
    }
  }
</script>

<section class="grid gap-5 lg:grid-cols-2">
  <article class="panel p-6">
    <div class="mb-4 flex items-center justify-between">
      <h2 class="text-sm font-medium">Node</h2>
      <span class="flex items-center gap-2 text-sm text-muted">
        <Light health={health} />
        {s.node_status}
      </span>
    </div>
    <dl class="divide-y divide-line text-sm">
      <div class="flex items-center justify-between py-2.5">
        <dt class="text-muted">Uptime</dt>
        <dd>{s.uptime}</dd>
      </div>
      <div class="flex items-center justify-between py-2.5">
        <dt class="text-muted">BLVM</dt>
        <dd>{s.blvm_version}</dd>
      </div>
      <div class="flex items-center justify-between py-2.5">
        <dt class="text-muted">Network</dt>
        <dd>{s.network}</dd>
      </div>
      <div class="flex items-center justify-between py-2.5">
        <dt class="text-muted">RPC</dt>
        <dd class="font-mono text-xs">{s.rpc_addr}</dd>
      </div>
    </dl>
    <p class="mt-4 text-sm text-faint">{powerHint}</p>
    <button
      type="button"
      disabled={powerBusy || s.node_busy}
      onclick={onPower}
      class="mt-4 rounded-md px-4 py-2 text-sm font-medium disabled:opacity-50 {s.node_running
        ? 'border border-dead text-dead'
        : 'bg-good text-bg'}"
    >
      {s.node_power_label}
    </button>
  </article>

  <article class="panel p-6">
    <h2 class="mb-4 text-sm font-medium">Disk Footprint</h2>
    <div class="text-center">
      <p class="flex items-end justify-center gap-1 leading-none">
        <span class="text-4xl font-medium"
          >{!s.disk_used_num || s.disk_used_num === "—" ? "0" : s.disk_used_num}</span
        >
        <small class="mb-1 text-muted">{s.disk_used_unit || "GB"}</small>
      </p>
      <span class="mt-1 block text-[0.72rem] font-medium tracking-wide text-muted uppercase"
        >Chain Size</span
      >
    </div>
    <div class="mt-4">
      <Ring
        mode="disk"
        free={Number(s.disk_free_bytes) || 0}
        total={Number(s.disk_total_bytes) || 0}
        centerValue={haveVol
          ? !s.disk_free_num || s.disk_free_num === "—"
            ? "0"
            : s.disk_free_num
          : "—"}
        centerUnit={s.disk_free_unit || "GB"}
      />
    </div>
  </article>
</section>
