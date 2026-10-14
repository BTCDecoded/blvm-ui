<script lang="ts">
  import { fmt } from "./format";
  import { KNOWN_HEIGHT_AS_OF } from "./networks";
  import { MAX_HEIGHT, STEP } from "./validation";

  // A height from 0 up to the known tip: a slider for the rough position and a number box for the exact height,
  // both bound to the same value. The box may go above the tip; it then warns, and the user decides.
  // The same control picks a number of blocks (the pruning setting): `min`, `step`, `limit`, `tipText`, `overText`
  // and `noun` change what it says; the defaults are the assume-valid wording.
  let {
    value = $bindable<number | null>(0),
    max,
    label,
    live = false,
    id = "height",
    min = 0,
    step = STEP,
    limit = MAX_HEIGHT,
    tipText = "Known tip",
    overText = "Above the known tip for this network: every block would be assumed valid until the chain passes it. Only use a height you trust.",
    noun = "blocks",
  }: {
    value?: number | null;
    /** The known tip of the network (live when `live`, otherwise approximate). */
    max: number;
    label: string;
    /** `max` comes from the running node, not from the console's built-in figure. */
    live?: boolean;
    id?: string;
    /** Smallest accepted value. The slider has a position for 0 and clamps it to this. */
    min?: number;
    step?: number;
    /** Largest accepted value. */
    limit?: number;
    /** Start of the line under the control: "<tipText>: 915,000 (approximate, ...)". */
    tipText?: string;
    /** Shown when the value is above `max`. */
    overText?: string;
    noun?: string;
  } = $props();

  // A range input needs a maximum that is a whole step and above zero; the line below states the real figure.
  const rangeMax = $derived(Math.max(step, Math.ceil(max / step) * step));
  const over = $derived(typeof value === "number" && Number.isFinite(value) && value > max);
  const bad = $derived(value === null || value === undefined || !Number.isInteger(value) || value < min || value > limit);
</script>

<div class="hs">
  <label class="ui-label" for="{id}-num">{label}</label>
  <div class="row">
    <input
      id="{id}-range"
      class="range"
      type="range"
      min="0"
      max={rangeMax}
      step={step}
      aria-label="{label} (slider)"
      value={Math.min(value ?? 0, rangeMax)}
      oninput={(e) => (value = Math.max(min, Number(e.currentTarget.value)))}
    />
    <input
      id="{id}-num"
      class="ui-input num font-mono"
      type="number"
      min={min}
      max={limit}
      step="1"
      inputmode="numeric"
      bind:value
      aria-invalid={bad}
    />
  </div>
  <p class="ui-note mt-2">
    {#if max > 0}
      {tipText}: {fmt(max)} ({live ? "live, from your node" : `approximate, as of ${KNOWN_HEIGHT_AS_OF}`}).
    {:else}
      No known tip for this network (a private chain).
    {/if}
  </p>
  {#if bad}
    <p class="ui-bad mt-2 text-[0.95rem]" role="alert">Enter a whole number of {noun}, from {fmt(min)} to {fmt(limit)}.</p>
  {:else if over}
    <p class="ui-warn mt-2 text-[0.95rem]" role="alert">{overText}</p>
  {/if}
</div>

<style>
  .hs {
    margin-top: 1rem;
  }
  .row {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.75rem 1rem;
  }
  .range {
    flex: 1 1 12rem;
    min-width: 0;
    accent-color: var(--color-orange);
  }
  .num {
    flex: 0 0 9.5rem;
    width: 9.5rem;
  }
</style>
