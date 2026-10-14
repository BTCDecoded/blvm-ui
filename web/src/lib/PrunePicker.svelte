<script lang="ts">
  import { fmt } from "./format";
  import HeightSlider from "./HeightSlider.svelte";
  import { diskWords, type NetworkInfo } from "./networks";
  import { keepSizeWords, PRUNE_MAX, PRUNE_MIN, PRUNE_STEP, validKeep, type StorageChoice } from "./pruning";

  // "Keep every block" or "keep only the newest N", shared by the setup wizard and the Settings Pruning card.
  // N is picked with the same slider and number box as the assume-valid height (HeightSlider), from 288 up to the
  // chain height the page knows for the network.
  let {
    net,
    choice = $bindable<StorageChoice>("full"),
    keep = $bindable<number | null>(10_000),
    max,
    live = false,
    id = "prune",
  }: {
    net: NetworkInfo;
    choice?: StorageChoice;
    keep?: number | null;
    /** The chain height the page knows for this network. */
    max: number;
    live?: boolean;
    id?: string;
  } = $props();

  const pick = (c: StorageChoice) => (choice = c);
  const size = $derived(validKeep(keep) ? keepSizeWords(net, keep) : null);
</script>

<div class="choices" role="group" aria-label="Blocks to keep on {net.label}">
  <button type="button" aria-pressed={choice === "full"} class="choice {choice === 'full' ? 'is-on' : ''}" onclick={() => pick("full")}>
    <span class="choice-name">Keep every block (full node)</span>
    <span class="choice-text">
      {net.label} needs {diskWords(net)}. Can give any block, old or new, to other nodes.
    </span>
  </button>
  <button type="button" aria-pressed={choice === "pruned"} class="choice {choice === 'pruned' ? 'is-on' : ''}" onclick={() => pick("pruned")}>
    <span class="choice-name">Keep only the newest blocks (pruned)</span>
    <span class="choice-text">Deletes older blocks after they are verified. Much smaller on disk; cannot give out old history.</span>
  </button>
</div>

{#if choice === "pruned"}
  <HeightSlider
    bind:value={keep}
    {max}
    {live}
    {id}
    label="Newest blocks to keep"
    min={PRUNE_MIN}
    step={PRUNE_STEP}
    limit={PRUNE_MAX}
    tipText="Chain height"
    noun="blocks"
    overText="More blocks than the chain has: the node keeps every block for now, and starts deleting once the chain grows past this number."
  />
  <p class="ui-note mt-2">
    The least allowed is {fmt(PRUNE_MIN)} blocks (about two days).
    {#if size}{size.charAt(0).toUpperCase() + size.slice(1)} on disk for the blocks themselves (approximate).{/if}
  </p>
{/if}

<style>
  .choices {
    margin-top: 1rem;
    display: grid;
    gap: 0.6rem;
    grid-template-columns: repeat(auto-fit, minmax(min(100%, 14rem), 1fr));
  }
  .choice {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 0.3rem;
    border: 1px solid var(--color-line);
    border-radius: 1rem;
    background: #0e0d0b;
    padding: 1rem 1.1rem;
    text-align: left;
    color: var(--color-muted);
    transition: border-color 0.2s, background 0.2s;
  }
  .choice:hover {
    border-color: rgb(240 230 208 / 0.25);
  }
  .choice.is-on {
    border-color: rgb(247 147 26 / 0.55);
    background: linear-gradient(180deg, #17130d, #0e0d0b);
    color: var(--color-cream);
    box-shadow: 0 0 0 1px rgb(247 147 26 / 0.12);
  }
  .choice-name {
    font-size: 1.05rem;
    font-weight: 500;
    color: var(--color-cream);
  }
  .choice-text {
    font-size: 0.9rem;
    line-height: 1.45;
    color: var(--color-muted);
  }
</style>
