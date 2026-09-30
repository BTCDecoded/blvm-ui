<script lang="ts">
  import CmdBlock from "./CmdBlock.svelte";
  import Icon from "./Icon.svelte";
</script>

<section class="ui-page">
  <article class="panel ui-card intro">
    <div class="ui-head !mb-2">
      <h2 class="ui-title"><Icon name="mining" class="h-5 w-5 text-muted" /> Mining</h2>
      <span class="ui-chip">Not wired yet</span>
    </div>
    <p class="ui-lead max-w-3xl">
      Mining runs in optional modules, not inside the node. This console will not invent hashrate or pool stats, so
      this page stays empty until those modules report here. Until then, drive them from <code>blvm/</code>.
    </p>
  </article>

  <div class="mining-grid">
    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="bolt" class="h-5 w-5 text-muted" /> Stratum V2</h2>
        <span class="ui-chip font-mono">127.0.0.1:3333</span>
      </div>
      <p class="ui-lead">Miners connect here. It hands out jobs, checks shares, and submits real blocks to the node.</p>
      <div class="cmds">
        <CmdBlock label="Start with pool + stratum" cmd="./target/release/blvm --config testnet4-pool.toml --verbose" />
        <CmdBlock label="Loaded modules" cmd="./target/release/blvm --config testnet4-pool.toml module list" />
        <CmdBlock label="Load stratum" cmd="./target/release/blvm --config testnet4-pool.toml load blvm-stratum-v2" />
      </div>
    </article>

    <article class="panel ui-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="users" class="h-5 w-5 text-muted" /> Commons Pool</h2>
        <span class="ui-chip">WalkOnly</span>
      </div>
      <p class="ui-lead">Splits the coinbase fairly: a 1% genesis fee first, then miners by share.</p>
      <div class="cmds">
        <CmdBlock label="Pool status" cmd="./target/release/blvm --config testnet4-pool.toml rpc commons_status" />
        <CmdBlock label="Reload pool" cmd="./target/release/blvm --config testnet4-pool.toml reload blvm-commons-pool" />
      </div>
    </article>
  </div>
</section>

<style>
  .mining-grid {
    display: grid;
    flex: 1 1 auto;
    gap: 0.75rem;
  }
  @media (min-width: 1024px) {
    .mining-grid {
      grid-template-columns: repeat(2, minmax(0, 1fr));
    }
  }
  .cmds {
    margin-top: 1.4rem;
    display: grid;
    grid-template-columns: minmax(0, 1fr);
    gap: 1.1rem;
  }
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: 0.88em;
    color: var(--color-orange);
  }
</style>
