<script lang="ts">
  import { callSetting } from "./api";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import Ring from "./Ring.svelte";
  import { ui } from "./status.svelte";

  const s = $derived(ui.snap);
  const online = $derived(s.connected && !s.frozen);

  let addPeer = $state("");
  let banAddr = $state("");
  let addMsg = $state("");
  let banMsg = $state("");
  let tableMsg = $state("");
  let netMsg = $state("");
  let netBusy = $state(false);

  const listenText = $derived(
    s.frozen
      ? "RPC frozen"
      : s.accepting_inbound && (s.connected || s.node_running)
        ? "Accepting inbound connections"
        : "Not listening",
  );

  function errText(err: unknown): string {
    return err instanceof Error ? err.message : String(err);
  }

  function fmtBanUntil(n: number | null): string {
    if (!n) return "Permanent";
    const d = new Date(n * 1000);
    return Number.isNaN(d.getTime()) ? String(n) : d.toLocaleString();
  }

  async function onAddPeer(e: Event) {
    e.preventDefault();
    const addr = addPeer.trim();
    if (!addr) return;
    try {
      await callSetting("addnode", [addr, "onetry"]);
      addPeer = "";
      addMsg = `Trying ${addr}.`;
    } catch (err) {
      addMsg = errText(err);
    }
  }

  async function onBan(e: Event) {
    e.preventDefault();
    const addr = banAddr.trim();
    if (!addr) return;
    try {
      await callSetting("setban", [addr, "add", 86400]);
      banAddr = "";
      banMsg = `${addr} blocked for 24 hours.`;
    } catch (err) {
      banMsg = errText(err);
    }
  }

  async function clearBans() {
    try {
      await callSetting("clearbanned", []);
      banMsg = "All bans cleared.";
    } catch (err) {
      banMsg = errText(err);
    }
  }

  async function peerAct(act: "disconnect" | "block" | "unban", addr: string) {
    try {
      if (act === "disconnect") {
        await callSetting("disconnectnode", [addr]);
        tableMsg = `Disconnected ${addr}.`;
      } else if (act === "block") {
        await callSetting("setban", [addr, "add", 86400]);
        tableMsg = `${addr} blocked for 24 hours.`;
      } else {
        await callSetting("setban", [addr, "remove"]);
        banMsg = `Unbanned ${addr}.`;
      }
    } catch (err) {
      if (act === "unban") banMsg = errText(err);
      else tableMsg = errText(err);
    }
  }

  async function toggleNetwork() {
    if (netBusy || !s.connected) return;
    const on = !s.network_active;
    netBusy = true;
    try {
      await callSetting("setnetworkactive", [on]);
      ui.snap = { ...ui.snap, network_active: on };
      netMsg = on ? "Network is active." : "Peer activity paused.";
    } catch (err) {
      netMsg = errText(err);
    } finally {
      netBusy = false;
    }
  }
</script>

<section class="ui-page">
  <div class="net-top">
    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="network" class="h-5 w-5 text-muted" /> P2P Overview</h2>
        <span class="ui-chip"><Light health={s.health} /> {s.network && s.network !== "—" ? s.network : "offline"}</span>
      </div>
      <div class="overview">
        <Ring
          inbound={s.inbound}
          outbound={s.outbound}
          centerValue={String(s.peers)}
          centerUnit="Peers"
          class="h-[8.5rem] w-[8.5rem] shrink-0"
        />
        <ul class="legend">
          <li>
            <span><i class="dot bg-outbound"></i>Outbound</span>
            <strong>{s.outbound}</strong>
          </li>
          <li>
            <span><i class="dot bg-inbound"></i>Inbound</span>
            <strong>{s.inbound}</strong>
          </li>
        </ul>
      </div>
      <div class="net-switch">
        <div class="min-w-0">
          <p class="font-medium">Network Active</p>
          <p class="ui-note flex items-center gap-2">
            <Light health={s.accepting_inbound && s.connected ? "good" : "dead"} />
            {netMsg || listenText}
          </p>
        </div>
        <button
          type="button"
          role="switch"
          aria-checked={!!s.network_active}
          aria-label="Network active"
          class="ui-switch"
          disabled={!s.connected || netBusy}
          onclick={toggleNetwork}
        ></button>
      </div>
    </article>

    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="plus" class="h-5 w-5 text-muted" /> Add Peer</h2>
      </div>
      <p class="ui-lead">Ask the node to connect once to a peer at <code>IP:port</code>.</p>
      <form class="mt-auto pt-5" onsubmit={onAddPeer}>
        <label class="ui-label" for="add-peer-addr">Peer Address</label>
        <div class="flex flex-wrap gap-2">
          <input
            id="add-peer-addr"
            class="ui-input"
            bind:value={addPeer}
            placeholder="1.2.3.4:48333"
            spellcheck="false"
            autocomplete="off"
          />
          <button type="submit" class="ui-btn" disabled={!online || !addPeer.trim()}>Add Peer</button>
        </div>
        <p class="ui-note mt-3 min-h-[1.35rem]">{addMsg || (online ? "" : "Node must be online to add peers.")}</p>
      </form>
    </article>

    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="shield" class="h-5 w-5 text-muted" /> Block Address</h2>
        <button type="button" class="ui-btn-ghost is-danger" disabled={!online || !s.banned?.length} onclick={clearBans}>
          Clear All Bans
        </button>
      </div>
      <p class="ui-lead">Ban an address for 24 hours. It shows up under Blocked Peers below.</p>
      <form class="mt-auto pt-5" onsubmit={onBan}>
        <label class="ui-label" for="ban-addr">Address To Block</label>
        <div class="flex flex-wrap gap-2">
          <input
            id="ban-addr"
            class="ui-input"
            bind:value={banAddr}
            placeholder="1.2.3.4:48333"
            spellcheck="false"
            autocomplete="off"
          />
          <button type="submit" class="ui-btn" disabled={!online || !banAddr.trim()}>Block</button>
        </div>
        <p class="ui-note mt-3 min-h-[1.35rem]">{banMsg}</p>
      </form>
    </article>
  </div>

  <div class="net-bottom">
    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="users" class="h-5 w-5 text-muted" /> Connected Peers</h2>
        <span class="text-[1.1375rem] font-medium">{s.peer_rows?.length ?? 0}</span>
      </div>
      <div class="ui-table-wrap">
        <table class="ui-table">
          <thead>
            <tr><th>Address</th><th>Direction</th><th>Agent</th><th class="text-right">Actions</th></tr>
          </thead>
          <tbody>
            {#if !s.peer_rows?.length}
              <tr><td class="ui-empty" colspan="4">{online ? "No peers connected yet" : "Node is offline"}</td></tr>
            {:else}
              {#each s.peer_rows as p (p.addr)}
                <tr>
                  <td class="font-mono text-[0.85rem]">{p.addr}</td>
                  <td>
                    <span class="flex items-center gap-2">
                      <i class="dot {p.inbound ? 'bg-inbound' : 'bg-outbound'}"></i>{p.inbound ? "Inbound" : "Outbound"}
                    </span>
                  </td>
                  <td class="max-w-[14rem] truncate text-muted" title={p.subver}>{p.subver || "—"}</td>
                  <td>
                    <div class="flex justify-end gap-2">
                      <button type="button" class="ui-btn-ghost" onclick={() => peerAct("disconnect", p.addr)}>Disconnect</button>
                      <button type="button" class="ui-btn-ghost is-danger" onclick={() => peerAct("block", p.addr)}>Block</button>
                    </div>
                  </td>
                </tr>
              {/each}
            {/if}
          </tbody>
        </table>
      </div>
      {#if tableMsg}<p class="ui-note mt-3">{tableMsg}</p>{/if}
    </article>

    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="shield" class="h-5 w-5 text-muted" /> Blocked Peers</h2>
        <span class="text-[1.1375rem] font-medium">{s.banned?.length ?? 0}</span>
      </div>
      <div class="ui-table-wrap">
        <table class="ui-table">
          <thead>
            <tr><th>Address</th><th>Until</th><th class="text-right">Actions</th></tr>
          </thead>
          <tbody>
            {#if !s.banned?.length}
              <tr><td class="ui-empty" colspan="3">No blocked addresses</td></tr>
            {:else}
              {#each s.banned as b (b.address)}
                <tr>
                  <td class="font-mono text-[0.85rem]">{b.address}</td>
                  <td class="text-muted">{fmtBanUntil(b.banned_until)}</td>
                  <td>
                    <div class="flex justify-end">
                      <button type="button" class="ui-btn-ghost" onclick={() => peerAct("unban", b.address)}>Unban</button>
                    </div>
                  </td>
                </tr>
              {/each}
            {/if}
          </tbody>
        </table>
      </div>
    </article>
  </div>
</section>

<style>
  .net-top,
  .net-bottom {
    display: grid;
    gap: 0.75rem;
  }
  .net-bottom {
    flex: 1 1 auto;
    min-height: 18rem;
  }
  /* Peer tables scroll inside their cards instead of stretching the page. */
  .net-bottom > :global(.ui-card) {
    min-height: 0;
  }
  @media (min-width: 1024px) {
    .net-top {
      grid-template-columns: repeat(3, minmax(0, 1fr));
    }
    .net-bottom {
      grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr);
    }
  }
  .overview {
    padding-bottom: 1.1rem;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: center;
    gap: 1.5rem;
  }
  .legend {
    display: grid;
    grid-template-columns: auto auto;
    column-gap: 0.9rem;
    row-gap: 0.55rem;
    font-size: 1.05rem;
    color: var(--color-muted);
  }
  .legend li {
    display: contents;
  }
  .legend span {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .legend strong {
    justify-self: end;
    font-size: 1.3rem;
    font-weight: 500;
    color: var(--color-cream);
  }
  .dot {
    display: inline-block;
    width: 0.65rem;
    height: 0.65rem;
    flex-shrink: 0;
    border-radius: 999px;
  }
  .net-switch {
    margin-top: auto;
    padding-top: 1.1rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    border-top: 1px solid var(--color-line);
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.88em;
    color: var(--color-cream);
  }
</style>
