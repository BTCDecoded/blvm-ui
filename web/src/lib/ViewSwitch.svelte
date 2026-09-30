<script lang="ts">
  import Icon from "./Icon.svelte";
  import { setPage, ui, type Page } from "./status.svelte";

  const views: { id: Page; label: string; icon: string }[] = [
    { id: "home", label: "Home", icon: "home" },
    { id: "insights", label: "Insights", icon: "insights" },
  ];
  const active = $derived(Math.max(0, views.findIndex((v) => v.id === ui.page)));
</script>

<div class="switch-row">
  <div class="switch" role="tablist" aria-label="Dashboard view" style="--n:{views.length}; --i:{active}">
    <span class="thumb" aria-hidden="true"></span>
    {#each views as v, i}
      <button
        type="button"
        role="tab"
        aria-selected={i === active}
        class="opt {i === active ? 'is-on' : ''}"
        onclick={() => setPage(v.id)}
      >
        <Icon name={v.icon} class="h-4 w-4" />
        {v.label}
      </button>
    {/each}
  </div>
</div>

<style>
  .switch-row {
    display: flex;
    flex-shrink: 0;
    justify-content: center;
    margin-bottom: 0.75rem;
  }
  .switch {
    position: relative;
    display: grid;
    grid-template-columns: repeat(var(--n), minmax(0, 1fr));
    padding: 0.3rem;
    border: 1px solid var(--color-line);
    border-radius: 0.95rem;
    background: rgb(12 12 12 / 0.82);
    box-shadow:
      0 10px 30px rgb(0 0 0 / 0.45),
      inset 0 1px 0 rgb(240 230 208 / 0.04);
    backdrop-filter: blur(8px);
  }
  .thumb {
    position: absolute;
    top: 0.3rem;
    bottom: 0.3rem;
    left: 0.3rem;
    width: calc((100% - 0.6rem) / var(--n));
    border-radius: 0.7rem;
    border: 1px solid rgb(247 147 26 / 0.35);
    background: linear-gradient(180deg, rgb(247 147 26 / 0.2), rgb(247 147 26 / 0.09));
    transform: translateX(calc(100% * var(--i)));
    transition: transform 0.38s cubic-bezier(0.22, 1, 0.36, 1);
  }
  .opt {
    position: relative;
    z-index: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 0.5rem;
    min-width: 8.5rem;
    padding: 0.55rem 1.4rem;
    border-radius: 0.7rem;
    font-size: 0.95rem;
    font-weight: 500;
    color: var(--color-muted);
    transition: color 0.25s;
  }
  .opt:hover {
    color: var(--color-cream);
  }
  .opt.is-on {
    color: var(--color-cream);
  }
  @media (prefers-reduced-motion: reduce) {
    .thumb {
      transition: none;
    }
  }
</style>
