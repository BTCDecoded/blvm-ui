<script lang="ts">
  import CmdBlock from "./CmdBlock.svelte";
  import Icon from "./Icon.svelte";
  import { ui } from "./status.svelte";

  const s = $derived(ui.snap);
  const flag = $derived.by(() => {
    const n = s.network;
    if (n === "testnet4" || !n || n === "—") return "--config testnet4-dev.toml";
    if (n === "main") return "--network mainnet";
    if (n === "test") return "--network testnet";
    return `--network ${n}`;
  });

  const tags = [
    { tag: "[TIP_FOLLOW]", text: "One line per block connected at the tip, with fetch and validate timings." },
    { tag: "[IBD_RESUME]", text: "A warning that parallel IBD is about to re-validate. Small lags only log at debug." },
    { tag: "Shutdown signal received", text: "Clean stop started. The node gets up to 30 seconds to flush." },
    { tag: "module", text: "Lines about loading or crashing modules such as Stratum V2 or the Commons pool." },
  ];
</script>

<section class="ui-page">
  <div class="logs-grid">
    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="logs" class="h-5 w-5 text-muted" /> Node Logs</h2>
        <span class="ui-chip">Not streamed yet</span>
      </div>
      <p class="ui-lead">
        This console does not capture node output yet. The node prints its log in the terminal that started it. Run
        these from the <code>blvm/</code> folder to see or save it.
      </p>
      <div class="cmds">
        <CmdBlock label="Run with verbose logs" cmd={`./target/release/blvm ${flag} --verbose`} />
        <CmdBlock
          label="Debug filter"
          cmd={`RUST_LOG=blvm=debug,blvm_node=debug ./target/release/blvm ${flag}`}
        />
        <CmdBlock label="Save to a file" cmd={`./target/release/blvm ${flag} --verbose 2>&1 | tee blvm.log`} />
        <CmdBlock label="Follow a saved log" cmd="tail -f blvm.log" />
      </div>
    </article>

    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="info" class="h-5 w-5 text-muted" /> What To Look For</h2>
      </div>
      <ul class="tags">
        {#each tags as t}
          <li>
            <code>{t.tag}</code>
            <p class="ui-lead">{t.text}</p>
          </li>
        {/each}
      </ul>
      <p class="ui-note mt-auto pt-4">
        <code>RUST_LOG</code> wins over <code>BLVM_LOG_LEVEL</code>. Use the same network flags as the running node.
      </p>
    </article>
  </div>
</section>

<style>
  .logs-grid {
    display: grid;
    flex: 1 1 auto;
    gap: 0.75rem;
  }
  @media (min-width: 1024px) {
    .logs-grid {
      grid-template-columns: minmax(0, 1.6fr) minmax(0, 1fr);
    }
  }
  .cmds {
    margin-top: 1.4rem;
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 1.1rem;
  }
  .tags {
    display: grid;
    gap: 0.35rem;
  }
  .tags li {
    padding: 0.75rem 0;
    border-top: 1px solid var(--color-line);
  }
  .tags li:first-child {
    border-top: 0;
    padding-top: 0;
  }
  .tags li p {
    margin-top: 0.3rem;
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.88em;
    color: var(--color-orange);
  }
</style>
