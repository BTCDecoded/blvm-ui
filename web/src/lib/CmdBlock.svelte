<script lang="ts">
  let { label, cmd }: { label: string; cmd: string } = $props();
  let copied = $state(false);

  async function copy() {
    try {
      await navigator.clipboard.writeText(cmd);
      copied = true;
      setTimeout(() => (copied = false), 1400);
    } catch {
      copied = false;
    }
  }
</script>

<div class="cmd">
  <div class="cmd-head">
    <span class="ui-label !mb-0">{label}</span>
    <button type="button" class="ui-btn-ghost" onclick={copy}>{copied ? "Copied" : "Copy"}</button>
  </div>
  <code class="ui-code">{cmd}</code>
</div>

<style>
  .cmd {
    min-width: 0;
  }
  .cmd-head {
    margin-bottom: 0.45rem;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.75rem;
  }
</style>
