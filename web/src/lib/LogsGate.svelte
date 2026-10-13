<script lang="ts">
  import { patchSettings } from "./api";
  import Icon from "./Icon.svelte";
  import { ui } from "./status.svelte";

  // A typed #logs while the Logs page is off.
  let err = $state("");
  let busy = $state(false);

  async function turnOn() {
    if (busy) return;
    busy = true;
    err = "";
    try {
      const saved = await patchSettings({ show_logs: true });
      ui.snap = { ...ui.snap, show_logs: saved.show_logs === true };
    } catch (e) {
      err = e instanceof Error ? e.message : "Could not save that setting.";
    } finally {
      busy = false;
    }
  }
</script>

<section class="ui-page">
  <article class="panel ui-card max-w-2xl">
    <div class="ui-head">
      <h2 class="ui-title"><Icon name="logs" class="h-5 w-5 text-muted" /> Logs</h2>
    </div>
    <p class="ui-lead">The Logs page is off. It shows the node's raw log output. You can turn it on in Settings, or here.</p>
    <div class="pt-5">
      <button type="button" class="ui-btn" disabled={busy} onclick={turnOn}>Show the Logs page</button>
    </div>
    {#if err}<p class="ui-note ui-bad mt-3">{err}</p>{/if}
  </article>
</section>
