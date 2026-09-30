<script lang="ts">
  import type { FeedItem } from "./types";

  let {
    item = null,
    newest = false,
    fresh = false,
    empty = false,
    leaving = false,
    flow = false,
  }: {
    item?: FeedItem | null;
    newest?: boolean;
    fresh?: boolean;
    empty?: boolean;
    leaving?: boolean;
    flow?: boolean;
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
