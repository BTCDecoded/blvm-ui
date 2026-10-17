<script lang="ts">
  import { connectTo, toggleNode } from "./api";
  import { fmt, installedVersion, nodeReports, nodeVersion } from "./format";
  import Icon from "./Icon.svelte";
  import ConsoleCard from "./ConsoleCard.svelte";
  import Light from "./Light.svelte";
  import ModulesCard from "./ModulesCard.svelte";
  import NetCarousel from "./NetCarousel.svelte";
  import NetworkCard from "./NetworkCard.svelte";
  import NodeCard from "./NodeCard.svelte";
  import { chainLabel, fmtBytes, networkById, networkLabel } from "./networks";
  import { isManaged, setPage, setSub, ui, type Sub } from "./status.svelte";
  import TabSwitch from "./TabSwitch.svelte";
  import PruningCard from "./PruningCard.svelte";
  import UpdatesCard from "./UpdatesCard.svelte";
  import ValidationCard from "./ValidationCard.svelte";

  const s = $derived(ui.snap);
  // Managed mode (design A): only when the host says so. An older host never does.
  const managed = $derived(isManaged(s));
  const ver = $derived(nodeVersion(s));
  // Managed: one version for the node program, the same as the Node card (design M.3). What the node says
  // over RPC is a second, muted line while it answers (M.5); standalone keeps the old single line.
  const installed = $derived(installedVersion(s.managed));
  const reports = $derived(nodeReports(s, installed));
  const stored = $derived((s.managed?.networks ?? []).filter((n) => n.bytes > 0 || n.has_data));
  // The port the node listens on for peers: the host's figure (the Umbrel app moves mainnet off 8333), else the table's.
  const p2pPort = $derived.by(() => {
    const id = s.managed?.network;
    if (!id) return null;
    return s.managed?.networks.find((n) => n.id === id)?.p2p_port ?? networkById(id)?.p2pPort ?? null;
  });

  // Sub-pages. Chain and Modules are managed-node settings, so a standalone console does not show them.
  const TABS: { id: Sub; label: string; icon: string; managedOnly?: boolean }[] = [
    { id: "node", label: "Node", icon: "power" },
    { id: "network", label: "Network", icon: "network" },
    { id: "chain", label: "Chain", icon: "cube", managedOnly: true },
    { id: "modules", label: "Modules", icon: "bolt", managedOnly: true },
    { id: "console", label: "Console", icon: "settings" },
  ];
  const tabs = $derived(TABS.filter((t) => managed || !t.managedOnly));
  // A hash or a remembered tab that this mode does not have falls back to Node.
  const tab = $derived(tabs.some((t) => t.id === ui.sub) ? ui.sub : "node");
  // Keep the address bar on the tab that is showing (#settings -> #settings/node).
  // `tab` stays "node" when the hash asks for a tab this mode lacks, so the effect also reads `ui.sub`: it then
  // runs again on every hash change and rewrites the address bar (#settings/modules -> #settings/node). The same
  // typed hash twice leaves `ui.sub` unchanged, so a counter bumped by `hashchange` covers that case too.
  let hashTick = $state(0);
  $effect(() => {
    const bump = () => hashTick++;
    window.addEventListener("hashchange", bump);
    return () => window.removeEventListener("hashchange", bump);
  });
  $effect(() => {
    void ui.sub;
    void hashTick;
    const hash = `#settings/${tab}`;
    if (ui.page === "settings" && location.hash !== hash) history.replaceState(null, "", hash);
  });
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
    if (managed && s.managed?.network === null) return "No network chosen yet, so there is nothing to connect to.";
    if (s.connected || s.node_running) return `Using ${s.rpc_addr}.`;
    if (s.frozen) {
      return s.network && s.network !== "—"
        ? `${chainLabel(s.network)} node is running at ${s.rpc_addr}, but RPC is not answering. IBD may be stuck.`
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
  <TabSwitch {tabs} active={tab} label="Settings" panelId="settings-panel" onselect={(id) => setSub(id as Sub)} />

  <div class="settings-frame">
    <div class="sub-grid" id="settings-panel" role="tabpanel" aria-labelledby="settings-panel-tab-{tab}">
      {#if tab === "node"}
        {#if managed}
          <NodeCard />
          <UpdatesCard />
        {:else}
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
            {#if s.bundle_available}
              <p class="ui-note mt-4">
                A node program came with this console.
                <button type="button" class="ui-link" onclick={() => (ui.setupOpen = true)}>Use a managed node</button>
              </p>
            {/if}
          </article>
        {/if}
      {:else if tab === "network"}
        {#if managed}<NetworkCard />{/if}
        <article class="panel ui-card">
          <div class="ui-head">
            <h2 class="ui-title"><Icon name="link" class="h-5 w-5 text-muted" /> Node Connection</h2>
            <span class="ui-chip"><Light health={s.health} /> {connectLabel}</span>
          </div>
          <p class="ui-lead">
            {#if managed}
              This console talks to the node it runs over stock JSON-RPC. The address follows the network you pick.
            {:else}
              This console talks to a local BLVM node over stock JSON-RPC. It auto-connects. Point it at another
              address if your node runs on a different port.
            {/if}
          </p>
          <form class="pt-5" onsubmit={onConnect}>
            <label class="ui-label" for="rpc-addr">RPC Address</label>
            <div class="flex flex-wrap gap-2">
              <input
                id="rpc-addr"
                class="ui-input font-mono"
                bind:value={rpcInput}
                spellcheck="false"
                autocomplete="off"
                readonly={managed}
              />
              {#if !managed}
                <button type="submit" class="ui-btn" disabled={connecting || !rpcInput.trim()}>Connect</button>
              {/if}
            </div>
            <p class="ui-note mt-3">{connectMsg || connectHint}</p>
          </form>
          {#if managed && p2pPort}
            <dl class="about mt-4">
              <div><dt>P2P port</dt><dd>{p2pPort}</dd></div>
            </dl>
          {/if}
        </article>
        {#if !managed}
          <article class="panel ui-card">
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
        {/if}
      {:else if tab === "chain"}
        <ValidationCard />
        <PruningCard />
        {#if managed && s.managed}
          <article class="panel ui-card">
            <div class="ui-head">
              <h2 class="ui-title"><Icon name="info" class="h-5 w-5 text-muted" /> Storage</h2>
            </div>
            <dl class="about">
              {#each stored as n (n.id)}
                <div>
                  <dt>{networkLabel(n.id)}</dt>
                  <dd>
                    {fmtBytes(n.bytes)}
                    {#if n.prune_keep_blocks}<span class="ui-chip ml-2">Pruned · {fmt(n.prune_keep_blocks)}</span>{/if}
                  </dd>
                </div>
              {:else}
                <div><dt>Networks</dt><dd>No data yet</dd></div>
              {/each}
              <div><dt>Free disk</dt><dd>{fmtBytes(s.managed.free_bytes)}</dd></div>
              <div><dt>Data folder</dt><dd class="font-mono text-[0.9rem]" title={s.managed.data_root}>{s.managed.data_root}</dd></div>
            </dl>
            <p class="ui-note mt-auto pt-4">
              Switching networks keeps each one's data on disk. This console only deletes a network's data when you ask
              it to, to take a pruned node back to a full one.
            </p>
          </article>
        {/if}
      {:else if tab === "modules"}
        <ModulesCard />
      {:else}
        {#if s.show_logs !== undefined}
          <ConsoleCard />
        {/if}
        <article class="panel ui-card">
          <div class="ui-head">
            <h2 class="ui-title"><Icon name="info" class="h-5 w-5 text-muted" /> About</h2>
          </div>
          <dl class="about">
            <div><dt>Chain</dt><dd>{chainLabel(s.network) || "—"}</dd></div>
            <div><dt>Node RPC</dt><dd class="font-mono text-[0.9rem]">{s.rpc_addr}</dd></div>
            <div>
              <dt>BLVM Version</dt>
              <dd>
                {#if managed && s.managed}
                  {installed.text}
                  {#if installed.custom}<span class="ui-chip">Custom build</span>{/if}
                  {#if reports}<span class="ui-seen ml-2">{reports}</span>
                  {:else if ver.seen}<span class="ui-seen ml-2">{ver.seen}</span>{/if}
                {:else}
                  {ver.text}
                  {#if ver.seen}<span class="ui-seen ml-2">{ver.seen}</span>{/if}
                {/if}
              </dd>
            </div>
            <div><dt>Console</dt><dd>Commons Node View {s.ui_version}</dd></div>
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
            {#if managed}
              Each network keeps its own data; the Chain tab lists what is stored.
            {:else}
              Changing the node's chain means restarting it with a different network. This console never rewrites the
              datadir.
            {/if}
            Peers and bans live on the
            <button type="button" class="text-orange hover:underline" onclick={() => setPage("network")}>Network</button>
            page.
            {#if s.mode !== undefined}
              <button type="button" class="ui-link" onclick={() => (ui.setupOpen = true)}>Run setup again</button>
            {/if}
          </p>
        </article>
      {/if}
    </div>
  </div>
</section>

<style>
  /* The sub-page is its own size container: it follows the content area (the window minus the sidebar), so the
     two-column layout is decided by the room the cards really get, not by the window width. */
  .settings-frame {
    min-width: 0;
    container-type: inline-size;
  }
  .sub-grid {
    display: grid;
    align-items: start;
    gap: 0.75rem;
    /* One column that may shrink below its content (an implicit "auto" track grows to the widest card). */
    grid-template-columns: minmax(0, 1fr);
    min-width: 0;
  }
  /* Two columns only while each card keeps at least ~420px (2 x 420 + gap). px, not rem: the root font size
     scales with the window. */
  @container (min-width: 860px) {
    .sub-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }
  .sub-grid > :global(.ui-card) {
    min-width: 0;
    max-width: 100%;
  }
  .carousel-wrap {
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
  @media (max-width: 767px) {
    .about > div {
      flex-wrap: wrap;
      gap: 0.15rem 1rem;
    }
    /* Long values (data folder, versions) wrap instead of being cut off. */
    .about dd {
      max-width: 100%;
      overflow: visible;
      white-space: normal;
      overflow-wrap: anywhere;
    }
  }
</style>
