<script lang="ts">
  import type { FeedItem } from "./types";

  let {
    item = null,
    newest = false,
    fresh = false,
    empty = false,
    leaving = false,
    flow = false,
    jump = 0,
  }: {
    item?: FeedItem | null;
    newest?: boolean;
    fresh?: boolean;
    empty?: boolean;
    leaving?: boolean;
    flow?: boolean;
    /** Blocks the tip moved since the previous tile (first sync): a small "+N" on the tile when more than 1. */
    jump?: number;
  } = $props();

  const chunk = $derived(!!item && item.kind === "chunk" && item.start !== item.end);
</script>

<div
  class="iso {newest ? 'is-new' : ''} {chunk && !newest ? 'is-chunk' : ''} {empty
    ? 'is-empty'
    : ''} {leaving ? 'is-leaving' : ''} {fresh ? 'tile-enter' : ''}"
  aria-hidden={empty}
>
  <div class="iso-cube">
    <div class="iso-face iso-top"></div>
    <div class="iso-face iso-right"></div>
    <div class="iso-face iso-front">
      {#if item && !empty}
        {#if chunk}
          <span>{item.start}</span>
          <span class="iso-dash">–</span>
          <span>{item.end}</span>
        {:else}
          <span>{item.label}</span>
        {/if}
        {#if jump > 1}<small class="iso-jump" title="The tip moved by {jump} blocks since the last tile">+{jump}</small>{/if}
      {/if}
    </div>
    {#if flow}
      <svg class="iso-flow" viewBox="0 0 100 100" preserveAspectRatio="none" aria-hidden="true">
        <g>
          <path class="f3" d="M 99.4 50 L 99.4 1.1 L 0.6 1.1 L 0.6 50" />
          <path class="f2" d="M 99.4 50 L 99.4 1.1 L 0.6 1.1 L 0.6 50" />
          <path class="f1" d="M 99.4 50 L 99.4 1.1 L 0.6 1.1 L 0.6 50" />
          <path class="f0" d="M 99.4 50 L 99.4 1.1 L 0.6 1.1 L 0.6 50" />
        </g>
        <g>
          <path class="f3" d="M 99.4 50 L 99.4 98.9 L 0.6 98.9 L 0.6 50" />
          <path class="f2" d="M 99.4 50 L 99.4 98.9 L 0.6 98.9 L 0.6 50" />
          <path class="f1" d="M 99.4 50 L 99.4 98.9 L 0.6 98.9 L 0.6 50" />
          <path class="f0" d="M 99.4 50 L 99.4 98.9 L 0.6 98.9 L 0.6 50" />
        </g>
      </svg>
    {/if}
  </div>
</div>

<style>
  /* "+N": the tip moved by N blocks between two polls (shown during the first sync, one tile per poll). */
  .iso-jump {
    margin-top: 0.3em;
    border: 1px solid rgb(247 147 26 / 0.45);
    border-radius: 999px;
    padding: 0.05em 0.55em;
    font-size: 0.62em;
    font-weight: 600;
    letter-spacing: 0.04em;
    line-height: 1.35;
    color: var(--color-orange);
    background: rgb(247 147 26 / 0.1);
  }
  :global(.iso.is-new) .iso-jump {
    border-color: rgb(255 255 255 / 0.55);
    color: #fff;
    background: rgb(0 0 0 / 0.18);
  }
</style>
