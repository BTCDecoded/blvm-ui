<script lang="ts">
  import Icon from "./Icon.svelte";
  import { MODULE_PAGES } from "./modules";
  import { setPage } from "./status.svelte";

  // Design L: a module page reached by URL while its module is not installed or is off.
  let { page }: { page: string } = $props();
  const info = $derived(MODULE_PAGES[page]);
</script>

<section class="ui-page">
  <article class="panel ui-card max-w-2xl">
    <div class="ui-head">
      <h2 class="ui-title"><Icon name={info?.icon ?? "cube"} class="h-5 w-5 text-muted" /> {info?.label ?? "This page"}</h2>
    </div>
    <p class="ui-lead">
      Install the {info?.installHint ?? "matching"} module in Settings to use this page. It appears in the sidebar once a
      module for it is installed and on.
    </p>
    <div class="pt-5">
      <button type="button" class="ui-btn" onclick={() => setPage("settings")}>Open Settings</button>
    </div>
  </article>
</section>
