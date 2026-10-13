<script lang="ts">
  import { patchSettings } from "./api";
  import Icon from "./Icon.svelte";
  import { ui } from "./status.svelte";

  // The page switches of this console. For now one: the Logs page is off until the user turns it on.
  // The switch shows the host's `show_logs` (status), so it also follows a change made from elsewhere.
  let err = $state("");
  let busy = $state(false);
  const on = $derived(ui.snap.show_logs === true);

  async function flip() {
    if (busy) return;
    busy = true;
    err = "";
    try {
      const saved = await patchSettings({ show_logs: !on });
      ui.snap = { ...ui.snap, show_logs: saved.show_logs === true };
    } catch (e) {
      err = e instanceof Error ? e.message : "Could not save that setting.";
    } finally {
      busy = false;
    }
  }

</script>

<article class="panel ui-card">
  <div class="ui-head">
    <h2 class="ui-title"><Icon name="settings" class="h-5 w-5 text-muted" /> Console</h2>
  </div>
  <div class="opt">
    <span>Show the Logs page</span>
    <button
      type="button"
      role="switch"
      aria-checked={on}
      aria-label="Show the Logs page"
      class="ui-switch"
      disabled={busy}
      onclick={flip}
    ></button>
  </div>
  <p class="ui-note opt-help">Shows the node's raw log output. Off by default.</p>
  {#if err}<p class="ui-note ui-bad">{err}</p>{/if}
</article>

<style>
  .opt {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
    padding: 0.55rem 0;
    font-size: 0.95rem;
    color: var(--color-muted);
  }
  .opt-help {
    margin: 0 0 0.35rem;
    font-size: 0.85rem;
  }
</style>
