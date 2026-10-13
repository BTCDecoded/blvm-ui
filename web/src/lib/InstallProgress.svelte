<script lang="ts">
  import type { InstallJob, JobPhase } from "./types";

  // Progress of a node install or update (shared by the setup wizard and the Updates card).
  // Phases come from the host job: download (bytes of total), verify, test, install, restart.
  let { job, restart = false }: { job: InstallJob; restart?: boolean } = $props();

  const ALL: { phase: JobPhase; label: string; hint: string }[] = [
    { phase: "download", label: "Download", hint: "Downloading the node program" },
    { phase: "verify", label: "Check", hint: "Checking it against the release's published checksum" },
    { phase: "test", label: "Test", hint: "Running it once to make sure it starts" },
    { phase: "install", label: "Install", hint: "Putting it in place" },
    { phase: "restart", label: "Restart", hint: "Restarting the node on the new version" },
  ];
  const STEPS = $derived(restart ? ALL : ALL.filter((x) => x.phase !== "restart"));
  // A restart phase in a plain install (no restart step) reads as the last step.
  const idx = $derived.by(() => {
    const i = STEPS.findIndex((x) => x.phase === job.phase);
    return i >= 0 ? i : job.phase === "restart" ? STEPS.length - 1 : job.phase === "done" ? STEPS.length : -1;
  });
  const mb = (n: number) => (n / 1e6).toFixed(1);
  const pct = $derived(job.total > 0 ? Math.min(100, Math.round((job.bytes / job.total) * 100)) : 0);
  const now = $derived(STEPS[idx]);
</script>

<div class="prog" role="status" aria-live="polite">
  <ol class="steps" aria-label="Progress">
    {#each STEPS as st, i (st.phase)}
      <li class={i < idx ? "is-done" : i === idx ? "is-now" : ""} aria-current={i === idx ? "step" : undefined}>{st.label}</li>
    {/each}
  </ol>
  {#if job.phase === "download"}
    <div class="bar" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow={job.total > 0 ? pct : undefined} aria-label="Download">
      <span class={job.total > 0 ? "" : "is-wait"} style={job.total > 0 ? `width:${pct}%` : ""}></span>
    </div>
    <p class="ui-note mt-2">
      {#if job.total > 0}
        Downloading: {mb(job.bytes)} MB of {mb(job.total)} MB ({pct}%)
      {:else if job.bytes > 0}
        Downloading: {mb(job.bytes)} MB
      {:else}
        Starting the download…
      {/if}
    </p>
  {:else if now}
    <p class="ui-note mt-3">{now.hint}…</p>
  {/if}
</div>

<style>
  .prog {
    margin-top: 1rem;
  }
  .steps {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(0, 1fr));
    gap: 0.4rem;
  }
  .steps li {
    border-top: 3px solid rgb(240 230 208 / 0.14);
    padding-top: 0.45rem;
    font-size: 0.85rem;
    color: var(--color-faint);
  }
  .steps li.is-done {
    border-color: var(--color-good);
    color: var(--color-muted);
  }
  .steps li.is-now {
    border-color: var(--color-orange);
    color: var(--color-cream);
    animation: pulse-heal 1.6s ease-out infinite;
  }
  .bar {
    margin-top: 0.9rem;
    height: 0.5rem;
    border-radius: 999px;
    background: rgb(240 230 208 / 0.1);
    overflow: hidden;
  }
  .bar span {
    display: block;
    height: 100%;
    border-radius: inherit;
    background: var(--color-orange);
    transition: width 0.4s linear;
  }
  .bar span.is-wait {
    width: 35%;
    animation: slide 1.4s ease-in-out infinite alternate;
  }
  @keyframes slide {
    from {
      transform: translateX(0);
    }
    to {
      transform: translateX(185%);
    }
  }
</style>
