<script lang="ts">
  import { onMount } from "svelte";
  import { getReleases, installNode, isMissing, jobRunning, normalizeJob, updateNode } from "./api";
  import { agoLabel, withV } from "./format";
  import Icon from "./Icon.svelte";
  import InstallProgress from "./InstallProgress.svelte";
  import Light from "./Light.svelte";
  import { ui } from "./status.svelte";
  import type { InstallJob, ReleaseInfo } from "./types";

  // Design G.3 + D: installed vs latest from GET /api/node/releases, one button, progress from the job the host
  // carries in `managed.update` while it runs (same shape as /api/node/install).
  const m = $derived(ui.snap.managed!);
  const job = $derived(normalizeJob(m.update as unknown as Record<string, unknown>));
  const running = $derived(jobRunning(job));

  let rel = $state<ReleaseInfo | null>(null);
  let err = $state("");
  let sending = $state(false);
  // The host has no release routes (every host before the updater): say so instead of guessing.
  let missing = $state(false);
  // How the last job ended, kept here because the host may reset `managed.update` to idle soon after.
  let outcome = $state<InstallJob | null>(null);
  let sawRunning = false;

  async function load() {
    try {
      rel = await getReleases();
      err = "";
      missing = false;
    } catch (e) {
      if (isMissing(e)) {
        missing = true;
        err = "";
        return;
      }
      err = e instanceof Error ? e.message : "Could not check for updates.";
    }
  }
  onMount(() => {
    void load();
    const id = setInterval(load, 30000);
    return () => clearInterval(id);
  });

  $effect(() => {
    const j = job;
    if (jobRunning(j)) {
      sawRunning = true;
      outcome = null;
    } else if (j.phase === "done" || j.phase === "failed") {
      if (sawRunning || !outcome) outcome = j;
      sawRunning = false;
      void load();
    }
  });

  const channel = $derived(rel?.channel ?? m.update.channel);
  const unsupported = $derived(!!rel && rel.platform === "unsupported");
  const installedTag = $derived(rel?.installed?.tag ?? null);
  const custom = $derived(m.custom_build && !installedTag);
  const shownInstalled = $derived(
    installedTag ? withV(installedTag) : m.node_version ? (custom ? `blvm ${m.node_version.replace(/^blvm\s+/, "")}` : withV(m.node_version)) : null,
  );
  const latestTag = $derived(rel?.latest?.tag ?? null);
  const noBinary = $derived(!installedTag && !m.node_version);
  const blocked = $derived(!!rel && !rel.can_install && !unsupported);
  const canAct = $derived(!!rel && rel.can_install && !!latestTag && !running && (rel.available || custom || noBinary));

  const chip = $derived.by(() => {
    if (missing) return { health: "dead", text: "Not available" };
    if (running) return { health: "heal", text: "Updating…" };
    if (blocked) return channel === "store" ? { health: "good", text: "Managed by the store" } : { health: "dead", text: "Off" };
    if (unsupported) return { health: "dead", text: "Not available here" };
    if (!rel) return { health: "heal", text: "Checking…" };
    if (noBinary) return { health: "heal", text: "No node installed" };
    if (custom) return { health: "heal", text: "Custom build" };
    if (rel.available) return { health: "heal", text: "Update available" };
    return { health: "good", text: "Up to date" };
  });

  async function go() {
    if (!latestTag || sending || !rel) return;
    sending = true;
    err = "";
    outcome = null;
    try {
      // Nothing from a release installed yet (or a custom build): install. Otherwise: update.
      const d = installedTag ? await updateNode(latestTag) : await installNode(latestTag);
      if (d.ok === false) err = d.error || "The update could not start.";
    } catch (e) {
      err = e instanceof Error ? e.message : "The update could not start.";
    } finally {
      sending = false;
    }
  }

  const buttonText = $derived(
    noBinary ? `Install ${withV(latestTag ?? "")}` : custom ? `Install official ${withV(latestTag ?? "")}` : `Update to ${withV(latestTag ?? "")}`,
  );
</script>

<article class="panel ui-card">
  <div class="ui-head">
    <h2 class="ui-title"><Icon name="bolt" class="h-5 w-5 text-muted" /> Updates</h2>
    <span class="ui-chip"><Light health={chip.health} /> {chip.text}</span>
  </div>

  {#if missing}
    <p class="ui-lead">This version of the app cannot install node updates. The node program stays as it is.</p>
  {:else if unsupported}
    <p class="ui-lead">
      The console can install official node releases on Linux (x86_64 and aarch64). No build is published for this
      computer yet, so the node program stays as it is.
    </p>
  {:else if rel}
    <dl class="kv">
      <div>
        <dt>Installed</dt>
        <dd>
          {shownInstalled ?? "None yet"}
          {#if custom}<span class="ui-chip">Custom build</span>{/if}
        </dd>
      </div>
      <div><dt>Latest release</dt><dd>{latestTag ? withV(latestTag) : "—"}</dd></div>
      {#if rel.latest?.published_at}<div><dt>Published</dt><dd>{agoLabel(rel.latest.published_at)}</dd></div>{/if}
    </dl>

    {#if running}
      <InstallProgress {job} restart />
    {:else if outcome?.phase === "done"}
      <p class="ui-note mt-4 text-good">
        {outcome.tag ? `Now on ${withV(outcome.tag)}.` : "Done."}
      </p>
    {:else if outcome?.phase === "failed"}
      <p class="ui-note mt-4 {outcome.rolled_back ? 'ui-warn' : 'ui-bad'}">
        {outcome.error || "The update failed."}
        {outcome.rolled_back ? " Rolled back: your previous version is still in place." : " Nothing was changed."}
      </p>
    {/if}

    {#if blocked}
      <p class="ui-note mt-4">
        {#if channel === "store"}
          {rel.reason || "Update from the Umbrel App Store. This console does not replace the node by itself on Umbrel."}
        {:else}
          {rel.reason || "Updates are turned off for this console."}
        {/if}
      </p>
    {:else}
      <div class="act">
        <p class="ui-note min-w-0">
          {#if canAct}
            Checked against the release's published checksum before it is used. Releases are not signed yet.
            {#if rel.latest?.notes_url}<a class="ui-link" href={rel.latest.notes_url} target="_blank" rel="noopener noreferrer">Release notes</a>{/if}
          {:else if running}
            Keep this page open or close it: the update goes on either way.
          {:else if !latestTag}
            The latest release could not be checked. This needs an internet connection.
          {:else}
            You are on the latest release.
          {/if}
        </p>
        {#if canAct}
          <button type="button" class="ui-btn" disabled={sending || m.switch !== null} onclick={go}>{buttonText}</button>
        {/if}
      </div>
      {#if canAct && (m.process.state === "running" || m.process.state === "starting")}
        <p class="ui-note mt-2">Updating restarts the node. A syncing node can take a few minutes to save and stop first.</p>
      {/if}
    {/if}
  {:else if !err}
    <p class="ui-lead">Checking for updates…</p>
  {/if}
  {#if err}<p class="ui-note ui-bad mt-3">{err}</p>{/if}
</article>

<style>
  .act {
    margin-top: 1.1rem;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: 1rem;
  }
</style>
