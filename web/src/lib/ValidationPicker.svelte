<script lang="ts">
  import HeightSlider from "./HeightSlider.svelte";
  import type { NetworkInfo } from "./networks";
  import type { AssumeValidDefault } from "./types";
  import { defaultText, type ValidationChoice } from "./validation";

  // The three assume-valid choices and the height control, shared by the setup wizard and the Settings
  // Validation card. Heights only: the node's block-hash form does not work yet, so it is never offered.
  let {
    net,
    choice = $bindable<ValidationChoice>("default"),
    height = $bindable<number | null>(0),
    def,
    max,
    live = false,
    id = "av",
  }: {
    net: NetworkInfo;
    choice?: ValidationChoice;
    height?: number | null;
    def?: AssumeValidDefault;
    max: number;
    live?: boolean;
    id?: string;
  } = $props();

  const pick = (c: ValidationChoice) => (choice = c);
</script>

<!-- Users read "assume-valid height" as "start syncing from there": say first what it does not do. -->
<p class="ui-lead plain" role="note">
  Every block is still downloaded and processed from the first one; this setting only skips re-checking signatures
  below the height, which makes that part of the first sync faster.
</p>

<div class="choices" role="group" aria-label="Validation of {net.label}">
  <button type="button" aria-pressed={choice === "default"} class="choice {choice === 'default' ? 'is-on' : ''}" onclick={() => pick("default")}>
    <span class="choice-name">Node default (recommended)</span>
    <span class="choice-text">{defaultText(net, def)}</span>
  </button>
  <button type="button" aria-pressed={choice === "off"} class="choice {choice === 'off' ? 'is-on' : ''}" onclick={() => pick("off")}>
    <span class="choice-name">Verify everything</span>
    <span class="choice-text">Slowest and most careful. Checks every script and signature since the first block.</span>
  </button>
  <button type="button" aria-pressed={choice === "custom"} class="choice {choice === 'custom' ? 'is-on' : ''}" onclick={() => pick("custom")}>
    <span class="choice-name">Custom height</span>
    <span class="choice-text">Choose the height below which scripts and signatures are not re-checked.</span>
  </button>
</div>

{#if choice === "custom"}
  <HeightSlider bind:value={height} {max} {live} {id} label="Assume valid below block" />
  <p class="ui-note mt-2">Applies only to the initial sync; new blocks at the tip are always fully verified.</p>
{/if}

<style>
  .plain {
    margin-top: 1rem;
    color: var(--color-cream);
  }
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
