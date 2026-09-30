<script lang="ts">
  import { healthClass } from "./format";

  let { health, class: extra = "" }: { health: string; class?: string } = $props();
  const h = $derived(healthClass(health));
</script>

<span
  class="inline-block h-2 w-2 shrink-0 rounded-full {extra} {h === 'good'
    ? 'light-good'
    : h === 'heal'
      ? 'light-heal'
      : 'light-dead'}"
></span>
