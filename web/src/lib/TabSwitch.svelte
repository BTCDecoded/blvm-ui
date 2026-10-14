<script lang="ts">
  import Icon from "./Icon.svelte";

  // The sub-page tabs of a page (Settings). Same pill look as the Home / Insights switch (ViewSwitch), but the
  // row scrolls sideways on a narrow screen instead of squeezing the labels. Arrow keys, Home and End move
  // between tabs (roving tabindex); the tab panel is the content below, named by `panelId`.
  type Tab = { id: string; label: string; icon: string };
  let {
    tabs,
    active,
    label,
    panelId,
    onselect,
  }: { tabs: Tab[]; active: string; label: string; panelId: string; onselect: (id: string) => void } = $props();

  let list: HTMLDivElement | undefined = $state();

  // A tab that sits off the edge of a narrow row (a remembered Console tab on a phone) is brought into view.
  $effect(() => {
    const el = list?.querySelector<HTMLElement>(`[data-tab="${active}"]`);
    if (list && el) {
      const l = el.offsetLeft;
      if (l < list.scrollLeft || l + el.offsetWidth > list.scrollLeft + list.clientWidth) {
        list.scrollTo({ left: l - (list.clientWidth - el.offsetWidth) / 2 });
      }
    }
  });

  function onKey(e: KeyboardEvent) {
    const i = tabs.findIndex((t) => t.id === active);
    let next = -1;
    if (e.key === "ArrowRight") next = (i + 1) % tabs.length;
    else if (e.key === "ArrowLeft") next = (i - 1 + tabs.length) % tabs.length;
    else if (e.key === "Home") next = 0;
    else if (e.key === "End") next = tabs.length - 1;
    if (next < 0) return;
    e.preventDefault();
    onselect(tabs[next].id);
    list?.querySelector<HTMLElement>(`[data-tab="${tabs[next].id}"]`)?.focus();
  }
</script>

<div class="switch-row">
  <div class="switch" role="tablist" aria-label={label} bind:this={list}>
    {#each tabs as t (t.id)}
      <button
        type="button"
        role="tab"
        id="{panelId}-tab-{t.id}"
        data-tab={t.id}
        aria-selected={t.id === active}
        aria-controls={panelId}
        tabindex={t.id === active ? 0 : -1}
        class="opt {t.id === active ? 'is-on' : ''}"
        onclick={() => onselect(t.id)}
        onkeydown={onKey}
      >
        <Icon name={t.icon} class="h-4 w-4 shrink-0" />
        {t.label}
      </button>
    {/each}
  </div>
</div>

<style>
  .switch-row {
    display: flex;
    flex-shrink: 0;
    justify-content: center;
    min-width: 0;
    margin-bottom: 0.75rem;
  }
  .switch {
    position: relative;
    display: flex;
    max-width: 100%;
    min-width: 0;
    gap: 0.15rem;
    overflow-x: auto;
    padding: 0.3rem;
    border: 1px solid var(--color-line);
    border-radius: 0.95rem;
    background: rgb(12 12 12 / 0.82);
    box-shadow:
      0 10px 30px rgb(0 0 0 / 0.45),
      inset 0 1px 0 rgb(240 230 208 / 0.04);
    backdrop-filter: blur(8px);
    scrollbar-width: none;
  }
  .switch::-webkit-scrollbar {
    display: none;
  }
  .opt {
    display: flex;
    flex: 1 0 auto;
    align-items: center;
    justify-content: center;
    gap: 0.5rem;
    padding: 0.55rem 1.1rem;
    border: 1px solid transparent;
    border-radius: 0.7rem;
    font-size: 0.95rem;
    font-weight: 500;
    white-space: nowrap;
    color: var(--color-muted);
    transition:
      color 0.25s,
      background 0.25s,
      border-color 0.25s;
  }
  .opt:hover {
    color: var(--color-cream);
  }
  .opt.is-on {
    color: var(--color-cream);
    border-color: rgb(247 147 26 / 0.35);
    background: linear-gradient(180deg, rgb(247 147 26 / 0.2), rgb(247 147 26 / 0.09));
  }
  @media (max-width: 767px) {
    .opt {
      padding: 0.55rem 0.8rem;
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .opt {
      transition: none;
    }
  }
</style>
