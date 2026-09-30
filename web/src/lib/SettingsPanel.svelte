<script lang="ts">
  import { connectTo, toggleNode } from "./api";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import NetCarousel from "./NetCarousel.svelte";
  import { setPage, ui } from "./status.svelte";

  const s = $derived(ui.snap);
  const presets = [
    { rpc: "127.0.0.1:8332", chain: "main", label: "Mainnet" },
    { rpc: "127.0.0.1:48332", chain: "testnet4", label: "Testnet4" },
    { rpc: "127.0.0.1:18332", chain: "test", label: "Testnet3" },
    { rpc: "127.0.0.1:38332", chain: "signet", label: "Signet" },
    { rpc: "127.0.0.1:18443", chain: "regtest", label: "Regtest" },
  ];

  let rpcInput = $state("127.0.0.1:48332");
  let connectMsg = $state("");
  let connecting = $state(false);
  let powerHint = $state("Starts a stopped node, or stops a running one.");
  let powerBusy = $state(false);

  $effect(() => {
    if (document.activeElement?.id !== "rpc-addr") rpcInput = s.rpc_addr || rpcInput;
  });

  const connectHint = $derived.by(() => {
    if (s.connected || s.node_running) return `Using ${s.rpc_addr}.`;
    if (s.frozen) {
      return s.network && s.network !== "—"
        ? `${s.network} node is running at ${s.rpc_addr}, but RPC is not answering. IBD may be stuck.`
        : `Something is listening at ${s.rpc_addr}, but RPC is not answering.`;
    }
    if (s.show_manual) {
      return s.error
        ? "Auto-connect failed. If the node is running, set the address here."
        : "If the node is on, enter the address and connect.";
    }
    return s.error
      ? `No RPC at ${s.rpc_addr || "127.0.0.1:48332"}.`
      : `Waiting for ${s.rpc_addr || "127.0.0.1:48332"}…`;
  });

  const connectLabel = $derived(
    s.connected || s.node_running
      ? s.health === "good"
        ? "Connected"
        : s.health_label || "Connected"
      : s.frozen
        ? "Frozen"
        : s.health_label || "Node down",
  );

  async function target(addr: string) {
    if (!addr || connecting) return;
    connecting = true;
    try {
      ui.snap = await connectTo(addr);
      connectMsg = `Console now targeting ${addr}.`;
    } catch (err) {
      connectMsg = err instanceof Error ? err.message : String(err);
    } finally {
      connecting = false;
    }
  }

  function onConnect(e: Event) {
    e.preventDefault();
    void target(rpcInput.trim());
  }

  async function onPower() {
    if (powerBusy || s.node_busy) return;
    powerBusy = true;
    powerHint = s.node_running ? "Stopping the node. This console stays here." : "Starting the node.";
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

<section class="ui-page">
  <div class="settings-grid">
    <div class="settings-col">
      <article class="panel ui-card">
        <div class="ui-head">
          <h2 class="ui-title"><Icon name="link" class="h-5 w-5 text-muted" /> Node Connection</h2>
          <span class="ui-chip"><Light health={s.health} /> {connectLabel}</span>
        </div>
        <p class="ui-lead">
          This console talks to a local BLVM node over stock JSON-RPC. It auto-connects. Point it at another
          address if your node runs on a different port.
        </p>
        <form class="pt-5" onsubmit={onConnect}>
          <label class="ui-label" for="rpc-addr">RPC Address</label>
          <div class="flex flex-wrap gap-2">
            <input id="rpc-addr" class="ui-input font-mono" bind:value={rpcInput} spellcheck="false" autocomplete="off" />
            <button type="submit" class="ui-btn" disabled={connecting || !rpcInput.trim()}>Connect</button>
          </div>
          <p class="ui-note mt-3">{connectMsg || connectHint}</p>
        </form>
      </article>

      <article class="panel ui-card grow">
        <div class="ui-head">
          <h2 class="ui-title"><Icon name="network" class="h-5 w-5 text-muted" /> Console Target</h2>
        </div>
        <p class="ui-lead">
          Use the arrows to pick a chain's usual local RPC port. The console switches to the one in front. The node
          must already be running there.
        </p>
        <div class="carousel-wrap">
          <NetCarousel nets={presets} current={s.rpc_addr} busy={connecting} onpick={(rpc) => target(rpc)} />
        </div>
      </article>
    </div>

    <div class="settings-col">
      <article class="panel ui-card">
        <div class="ui-head">
          <h2 class="ui-title"><Icon name="power" class="h-5 w-5 text-muted" /> Node Process</h2>
          <span class="ui-chip"><Light health={s.node_running ? "good" : "dead"} /> {s.node_running ? "Running" : "Stopped"}</span>
        </div>
        <p class="ui-lead">
          Stopping sends SIGTERM for a clean flush, then SIGKILL only if the node hangs. This console keeps running
          either way.
        </p>
        <div class="power-row">
          <p class="ui-note min-w-0">{powerHint}</p>
          <button
            type="button"
            disabled={powerBusy || s.node_busy}
            onclick={onPower}
            class="power-btn {s.node_running ? 'is-stop' : 'is-start'}"
          >
            <Icon name="power" class="h-4 w-4" />
            {s.node_power_label}
          </button>
        </div>
      </article>

      <article class="panel ui-card grow">
        <div class="ui-head">
          <h2 class="ui-title"><Icon name="info" class="h-5 w-5 text-muted" /> About</h2>
        </div>
        <dl class="about">
          <div><dt>Chain</dt><dd>{s.network && s.network !== "—" ? s.network : "—"}</dd></div>
          <div><dt>Node RPC</dt><dd class="font-mono text-[0.9rem]">{s.rpc_addr}</dd></div>
          <div><dt>BLVM Version</dt><dd>{s.blvm_version}</dd></div>
          <div><dt>Console</dt><dd>Commons UI {s.ui_version}</dd></div>
          <div><dt>Console Uptime</dt><dd>{s.ui_uptime}</dd></div>
          <div><dt>Last Check</dt><dd>{s.last_check}</dd></div>
          <div>
            <dt>Peer Locations</dt>
            <dd>
              <a class="text-orange hover:underline" href="https://db-ip.com" target="_blank" rel="noopener noreferrer"
                >IP Geolocation by DB-IP</a
              >
            </dd>
          </div>
        </dl>
        <p class="ui-note mt-auto pt-4">
          Changing the node's chain means restarting it with a different network. This console never rewrites the
          datadir. Peers and bans live on the
          <button type="button" class="text-orange hover:underline" onclick={() => setPage("network")}>Network</button>
          page.
        </p>
      </article>
    </div>
  </div>
</section>

<style>
  .settings-grid {
    display: grid;
    flex: 1 1 auto;
    gap: 0.75rem;
  }
  @media (min-width: 1024px) {
    .settings-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }
  .settings-col {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
  }
  .grow {
    flex: 1 1 auto;
  }
  .carousel-wrap {
    margin-block: auto;
    padding-top: 1.25rem;
  }
  .power-row {
    margin-top: 1.25rem;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
  .power-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    border-radius: 0.65rem;
    padding: 0.65rem 1.1rem;
    font-size: 0.95rem;
    font-weight: 500;
    transition: opacity 0.15s, background 0.15s;
  }
  .power-btn.is-start {
    background: var(--color-good);
    color: var(--color-bg);
  }
  .power-btn.is-stop {
    border: 1px solid rgb(224 91 85 / 0.6);
    color: var(--color-dead);
  }
  .power-btn.is-stop:hover {
    background: rgb(224 91 85 / 0.1);
  }
  .power-btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .about {
    font-size: 1.02rem;
  }
  .about > div {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.65rem 0;
    border-top: 1px solid var(--color-line);
  }
  .about > div:first-child {
    border-top: 0;
  }
  .about dt {
    color: var(--color-muted);
  }
  .about dd {
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
</style>
