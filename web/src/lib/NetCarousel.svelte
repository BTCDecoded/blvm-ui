<script lang="ts">
  import Light from "./Light.svelte";

  type Net = { rpc: string; chain: string; label: string };

  let {
    nets,
    current,
    busy = false,
    onpick,
  }: {
    nets: Net[];
    current: string;
    busy?: boolean;
    onpick: (rpc: string) => void;
  } = $props();

  let idx = $state(0);
  let seen = "";
  let timer: ReturnType<typeof setTimeout> | undefined;

  // Follow the address the console is actually using (auto-connect, manual connect).
  $effect(() => {
    if (current === seen) return;
    seen = current;
    const i = nets.findIndex((n) => n.rpc === current);
    if (i >= 0) idx = i;
  });

  function go(i: number) {
    const next = Math.max(0, Math.min(nets.length - 1, i));
    if (next === idx) return;
    idx = next;
    clearTimeout(timer);
    // Switch once the user stops clicking, not on every step.
    timer = setTimeout(() => {
      if (nets[idx].rpc !== current) onpick(nets[idx].rpc);
    }, 450);
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "ArrowLeft") {
      e.preventDefault();
      go(idx - 1);
    } else if (e.key === "ArrowRight") {
      e.preventDefault();
      go(idx + 1);
    }
  }

  // Depth layout: front card, then each step out sits smaller and further back.
  const SHIFT = [0, 58, 98, 128];
  const SCALE = [1, 0.8, 0.64, 0.52];
  const FADE = [1, 0.62, 0.32, 0];

  function place(i: number): string {
    const o = i - idx;
    const d = Math.min(3, Math.abs(o));
    const x = Math.sign(o) * SHIFT[d];
    return `transform: translate(-50%, -50%) translateX(${x}%) scale(${SCALE[d]}); opacity: ${FADE[d]}; z-index: ${10 - d};`;
  }

  $effect(() => () => clearTimeout(timer));
</script>

<div class="carousel" role="group" aria-roledescription="carousel" aria-label="Network">
  <button type="button" class="arrow" aria-label="Previous network" disabled={idx === 0} onclick={() => go(idx - 1)}>
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
      <path d="M15 5l-7 7 7 7" stroke-linecap="round" stroke-linejoin="round" />
    </svg>
  </button>

  <!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
  <div class="stage" role="region" aria-label="Networks, use left and right arrow keys" tabindex="0" onkeydown={onKey}>
    {#each nets as n, i (n.rpc)}
      {@const front = i === idx}
      {@const live = current === n.rpc}
      <button
        type="button"
        class="net {front ? 'is-front' : ''} {live ? 'is-live' : ''}"
        style={place(i)}
        tabindex={front ? 0 : -1}
        aria-hidden={Math.abs(i - idx) > 2}
        aria-current={front ? "true" : undefined}
        onclick={() => go(i)}
      >
        <span class="name">{n.label}</span>
        <span class="port">127.0.0.1:{n.rpc.split(":")[1]}</span>
        <span class="state">
          {#if live}
            <Light health="good" class="!h-[0.5rem] !w-[0.5rem]" /> Active
          {:else if front && busy}
            Switching…
          {:else}
            RPC {n.rpc.split(":")[1]}
          {/if}
        </span>
      </button>
    {/each}
  </div>

  <button
    type="button"
    class="arrow"
    aria-label="Next network"
    disabled={idx === nets.length - 1}
    onclick={() => go(idx + 1)}
  >
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
      <path d="M9 5l7 7-7 7" stroke-linecap="round" stroke-linejoin="round" />
    </svg>
  </button>
</div>

<div class="dots" aria-hidden="true">
  {#each nets as _, i}
    <span class={i === idx ? "on" : ""}></span>
  {/each}
</div>

<style>
  .carousel {
    display: flex;
    align-items: center;
    gap: 0.5rem;
  }
  .stage {
    position: relative;
    flex: 1 1 auto;
    min-width: 0;
    height: 10rem;
    overflow: hidden;
    outline: none;
    border-radius: 0.9rem;
  }
  .stage:focus-visible {
    box-shadow: 0 0 0 1px var(--color-orange);
  }
  .net {
    position: absolute;
    top: 50%;
    left: 50%;
    display: flex;
    width: 11.5rem;
    height: 8.4rem;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 0.3rem;
    border: 1px solid var(--color-line);
    border-radius: 1rem;
    background: #0e0d0b;
    box-shadow: 0 14px 34px rgb(0 0 0 / 0.55);
    color: var(--color-muted);
    transition:
      transform 0.45s cubic-bezier(0.22, 1, 0.36, 1),
      opacity 0.45s ease,
      border-color 0.3s,
      color 0.3s;
    will-change: transform, opacity;
  }
  .net:not(.is-front) {
    cursor: pointer;
    filter: brightness(0.75);
  }
  .net.is-front {
    border-color: rgb(247 147 26 / 0.55);
    background: linear-gradient(180deg, #17130d, #0e0d0b);
    color: var(--color-cream);
    box-shadow:
      0 18px 40px rgb(0 0 0 / 0.6),
      0 0 0 1px rgb(247 147 26 / 0.12);
  }
  .name {
    font-size: 1.25rem;
    font-weight: 500;
  }
  .port {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.8rem;
    color: var(--color-faint);
  }
  .state {
    margin-top: 0.35rem;
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.75rem;
    font-weight: 500;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-muted);
  }
  .net.is-live .state {
    color: var(--color-good);
  }
  .arrow {
    display: grid;
    width: 2.4rem;
    height: 2.4rem;
    flex-shrink: 0;
    place-items: center;
    border: 1px solid var(--color-line);
    border-radius: 0.75rem;
    background: rgb(5 5 5 / 0.6);
    color: var(--color-cream);
    transition: border-color 0.15s, color 0.15s, opacity 0.15s;
  }
  .arrow:hover:not(:disabled) {
    border-color: var(--color-orange);
    color: var(--color-orange);
  }
  .arrow:disabled {
    opacity: 0.3;
    cursor: not-allowed;
  }
  .dots {
    margin-top: 0.6rem;
    display: flex;
    justify-content: center;
    gap: 0.4rem;
  }
  .dots span {
    width: 0.4rem;
    height: 0.4rem;
    border-radius: 999px;
    background: rgb(240 230 208 / 0.18);
    transition: width 0.3s, background 0.3s;
  }
  .dots span.on {
    width: 1.1rem;
    background: var(--color-orange);
  }
  @media (prefers-reduced-motion: reduce) {
    .net {
      transition: none;
    }
  }
</style>
