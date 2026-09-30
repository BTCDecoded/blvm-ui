<script lang="ts">
  import { cubicOut } from "svelte/easing";
  import BlockCube from "./BlockCube.svelte";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import { ui } from "./status.svelte";
  import type { FeedItem } from "./types";

  const s = $derived(ui.snap);
  let known = $state(new Set<string>());
  let lastRpc = $state("");

  let stage = $state<HTMLOListElement | undefined>();
  let cubePx = $state(143);
  // Whole blocks on screen. The half block fading out on the left is extra.
  let visible = $state(4);
  const GAP = 16;
  const PEEK = 0.5;
  const MIN_VISIBLE = 3;

  const pack = $derived((s.feed || []).slice(0, visible + 1));
  const newestKey = $derived(pack[0]?.key ?? "");
  const waiting = $derived(!!s.feed_waiting);

  $effect(() => {
    const el = stage;
    if (!el) return;
    const fit = () => {
      const w = el.clientWidth;
      // Block size follows window height (the size it has always had);
      // wider windows fit more blocks instead of bigger ones.
      const target = Math.max(72, window.innerHeight * 0.185);
      const n = Math.max(MIN_VISIBLE, Math.round((w - PEEK * target) / (target + GAP)));
      // Resize slightly so n blocks + the half block fill the width exactly.
      visible = n;
      cubePx = Math.max(56, Math.floor((w - n * GAP) / (n + PEEK)));
    };
    const ro = new ResizeObserver(fit);
    ro.observe(el);
    window.addEventListener("resize", fit);
    fit();
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", fit);
    };
  });

  type Slot =
    | { empty: true; key: string }
    | { empty: false; key: string; item: FeedItem };

  // Oldest on the left, newest on the right. Slot 0 is the half block fading out,
  // slot 1 is dimmed a little so the fade reads left to right. Empty blocks pad
  // the left until real blocks arrive and push them out.
  const slotsList = $derived.by((): Slot[] => {
    const empties = visible + 1 - pack.length;
    const out: Slot[] = [];
    for (let i = 0; i < empties; i++) {
      out.push({ empty: true, key: `empty:${i}` });
    }
    for (const item of [...pack].reverse()) {
      out.push({ empty: false, key: item.key, item });
    }
    return out;
  });

  // Slide blocks left when a new one arrives. Right edges line up, so a block
  // moving into the half slot keeps its visible half in place.
  function roll(node: Element, { from, to }: { from: DOMRect; to: DOMRect }) {
    const dx = from.right - to.right;
    if (!dx) return { duration: 0 };
    return {
      duration: 650,
      easing: cubicOut,
      css: (_t: number, u: number) => `transform: translateX(${u * dx}px)`,
    };
  }

  // The block that drops off the left end slides a bit further and fades away.
  function drop(_node: Element) {
    return {
      duration: 650,
      easing: cubicOut,
      css: (t: number, u: number) => `opacity: ${t * 0.35}; transform: translateX(${-u * cubePx * 0.5}px)`,
    };
  }

  $effect(() => {
    if (s.rpc_addr !== lastRpc) {
      lastRpc = s.rpc_addr;
      known = new Set();
    }
    const next = new Set(pack.map((i) => i.key));
    const t = setTimeout(() => {
      known = next;
    }, 400);
    return () => clearTimeout(t);
  });
</script>

<article
  class="blocks-card panel flex min-w-0 flex-col overflow-hidden px-5 py-2.5 sm:px-6"
  style="--cube:{cubePx}px; --gap:{GAP}px; --peek:{PEEK}"
>
  <div class="mb-0.5 flex shrink-0 items-center justify-between gap-3">
    <h2 class="flex items-center gap-2 text-[0.9625rem] font-medium text-cream">
      <Icon name="cube" class="h-[1.1rem] w-[1.1rem] text-muted" />
      Latest Blocks
    </h2>
    <span class="flex items-center gap-2 text-[0.825rem] font-medium tracking-wide text-muted uppercase">
      <Light
        class="!h-[0.55rem] !w-[0.55rem]"
        health={waiting ? (s.health === "dead" ? "dead" : "heal") : "good"}
      />
      {waiting ? "Waiting" : "Live"}
    </span>
  </div>

  <ol class="block-stage" bind:this={stage}>
    {#each slotsList as slot, i (slot.key)}
      <li
        class="block-slot {i === 0 ? 'is-leaving' : ''} {i === 1 ? 'is-fading' : ''}"
        animate:roll
        out:drop
      >
        {#if slot.empty}
          <BlockCube empty leaving={i === 0} flow={pack.length === 0 && i === slotsList.length - 1} />
        {:else}
          {#if slot.key === newestKey}
            <span class="newest-tag">Newest</span>
          {/if}
          <BlockCube
            item={slot.item}
            leaving={i === 0}
            newest={slot.key === newestKey}
            flow={slot.key === newestKey}
            fresh={i > 0 && !known.has(slot.item.key)}
          />
        {/if}
      </li>
    {/each}
  </ol>
  <div class="blocks-lift" aria-hidden="true">
    <div class="flow-arrow">
      <svg viewBox="0 0 120 16" preserveAspectRatio="none" fill="none">
        <path d="M118 8H4" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" vector-effect="non-scaling-stroke" />
        <path d="M11 2 3 8l8 6" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" vector-effect="non-scaling-stroke" />
      </svg>
    </div>
  </div>

</article>

<style>
  .blocks-card {
    flex: 0 0 auto;
  }
  .block-stage {
    display: flex;
    flex: 0 0 auto;
    height: calc(var(--cube) + 0.9rem);
    /* Room above the blocks for the "Newest" label. */
    margin-top: calc(var(--cube) * 0.1 + 1.55rem);
    align-items: flex-start;
    justify-content: flex-start;
    gap: var(--gap);
    overflow: visible;
  }
  /* Below the floor shadows: a fixed arrow under the newest slot. */
  .blocks-lift {
    display: flex;
    flex: 0 0 auto;
    justify-content: flex-end;
    padding: 2.15rem 0 0.7rem;
  }
  .flow-arrow {
    display: flex;
    width: var(--cube);
    /* Skip the cube's depth side so the arrow centers under the front face. */
    padding-right: clamp(10px, calc(var(--cube) * 0.08), 16px);
    justify-content: center;
    color: rgb(247 147 26 / 0.75);
  }
  .flow-arrow svg {
    width: 62%;
    height: 0.9rem;
  }
  .block-slot {
    position: relative;
    display: flex;
    flex: 0 0 auto;
    align-items: center;
    justify-content: center;
  }
  .block-slot :global(.iso) {
    width: var(--cube);
  }
  .block-slot {
    transition: opacity 0.65s ease;
  }
  /* Half block on the far left: only its right half shows, fading into the card. */
  .block-slot.is-leaving {
    width: calc(var(--cube) * var(--peek));
    justify-content: flex-end;
    overflow: hidden;
    opacity: 0.55;
    mask-image: linear-gradient(90deg, transparent 0%, rgb(0 0 0 / 0.35) 35%, #000 100%);
    -webkit-mask-image: linear-gradient(90deg, transparent 0%, rgb(0 0 0 / 0.35) 35%, #000 100%);
    pointer-events: none;
  }
  .block-slot.is-leaving :global(.iso) {
    flex-shrink: 0;
    width: var(--cube);
  }
  .newest-tag {
    position: absolute;
    bottom: calc(100% + 0.65rem);
    /* Centered over the whole block, including its 3D side. */
    left: 0;
    right: 0;
    z-index: 4;
    text-align: center;
    font-size: 0.8rem;
    font-weight: 600;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: #fff;
    pointer-events: none;
    animation: tag-in 0.4s ease-out;
  }
  @keyframes tag-in {
    from {
      opacity: 0;
      transform: translateY(4px);
    }
  }
  /* Next block in is part-way through the same fade. */
  .block-slot.is-fading {
    opacity: 0.82;
  }
</style>
