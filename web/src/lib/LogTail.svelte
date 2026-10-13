<script lang="ts">
  import { onMount, tick } from "svelte";
  import { fetchLogs } from "./api";
  import Icon from "./Icon.svelte";
  import Light from "./Light.svelte";
  import { ui } from "./status.svelte";
  import type { LogLine } from "./types";

  // Design G / U3: the managed Logs page. Polls /api/node/logs?after=<seq> every second, follows the end of
  // the log, and stops following when you scroll up.
  const MAX_LINES = 2000;
  const s = $derived(ui.snap);

  let lines = $state<LogLine[]>([]);
  let file = $state("");
  let err = $state("");
  let copied = $state(false);
  let follow = $state(true);
  let box: HTMLDivElement | undefined = $state();
  let lastSeq: number | undefined;

  async function poll() {
    try {
      // The first request asks for the last 500 lines (what was written before the page opened); later ones for
      // everything new since the last line seen.
      const d = await fetchLogs(lastSeq, lastSeq === undefined ? 500 : 1000);
      err = "";
      file = d.file || file;
      if (d.lines.length) {
        lastSeq = d.lines[d.lines.length - 1].seq;
        lines = [...lines, ...d.lines].slice(-MAX_LINES);
        if (follow) await toEnd();
      }
    } catch (e) {
      err = e instanceof Error ? e.message : "Could not read the log.";
    }
  }

  async function toEnd() {
    await tick();
    if (box) box.scrollTop = box.scrollHeight;
  }

  function onScroll() {
    if (!box) return;
    follow = box.scrollHeight - box.scrollTop - box.clientHeight < 24;
  }

  function resume() {
    follow = true;
    void toEnd();
  }

  async function copyPath() {
    try {
      await navigator.clipboard.writeText(file);
      copied = true;
      setTimeout(() => (copied = false), 1400);
    } catch {
      copied = false;
    }
  }

  onMount(() => {
    void poll();
    const id = setInterval(poll, 1000);
    return () => clearInterval(id);
  });

  const tags = [
    { tag: "[TIP_FOLLOW]", text: "One line per block connected at the tip, with fetch and validate timings." },
    { tag: "[IBD_RESUME]", text: "A warning that parallel IBD is about to re-validate. Small lags only log at debug." },
    { tag: "Shutdown signal received", text: "Clean stop started. The node gets up to 30 seconds to flush." },
    { tag: "module", text: "Lines about loading or crashing modules such as Stratum V2." },
  ];
</script>

<section class="ui-page">
  <div class="tail-grid">
    <article class="panel ui-card tail-card">
      <div class="ui-head">
        <h2 class="ui-title"><Icon name="logs" class="h-5 w-5 text-muted" /> Node Logs</h2>
        <span class="ui-chip">
          {#if err}
            <Light health="dead" /> Can't read the log
          {:else if follow}
            <Light health="good" /> Live
          {:else}
            <Light health="heal" /> Paused
          {/if}
        </span>
      </div>

      <div class="viewport">
        <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
        <div class="log ui-code" bind:this={box} onscroll={onScroll} role="log" aria-live="off" tabindex="0" aria-label="Node log">
          {#if lines.length === 0}
            <span class="empty">{err || (s.managed?.process.state === "stopped" ? "The node is stopped. Nothing new will appear until it starts." : "Waiting for log lines…")}</span>
          {:else}
            {#each lines as l (l.seq)}<div class="line">{l.text}</div>{/each}
          {/if}
        </div>
        {#if !follow}
          <button type="button" class="ui-btn jump" onclick={resume}>Jump to latest</button>
        {/if}
      </div>

      <div class="path">
        <span class="ui-label !mb-0">Log file</span>
        <code class="ui-code" title={file}>{file || "—"}</code>
        <button type="button" class="ui-btn-ghost" disabled={!file} onclick={copyPath}>{copied ? "Copied" : "Copy"}</button>
      </div>
      <p class="ui-note mt-3">Showing the last {lines.length ? lines.length : 0} lines. The file keeps the full history.</p>
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
      <p class="ui-note mt-auto pt-4">Scroll up to stop following. The log keeps filling in the background.</p>
    </article>
  </div>
</section>

<style>
  .tail-grid {
    display: grid;
    flex: 1 1 auto;
    gap: 0.75rem;
  }
  @media (min-width: 1024px) {
    .tail-grid {
      grid-template-columns: minmax(0, 1.9fr) minmax(0, 1fr);
    }
  }
  .tail-card {
    min-height: 28rem;
  }
  .viewport {
    position: relative;
    display: flex;
    min-height: 18rem;
    flex: 1 1 0;
  }
  .log {
    min-width: 0;
    flex: 1 1 auto;
    overflow: auto;
    white-space: pre;
    font-size: 0.78rem;
    line-height: 1.55;
    padding: 0.75rem 0.9rem;
  }
  .line {
    min-width: max-content;
  }
  .empty {
    color: var(--color-faint);
    white-space: normal;
  }
  .jump {
    position: absolute;
    right: 1.1rem;
    bottom: 1rem;
    padding: 0.45rem 0.85rem;
    font-size: 0.85rem;
    box-shadow: 0 8px 24px rgb(0 0 0 / 0.5);
  }
  .path {
    margin-top: 1rem;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.6rem;
  }
  .path code {
    min-width: 0;
    flex: 1 1 14rem;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    padding: 0.5rem 0.75rem;
    font-size: 0.8rem;
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
  .path code {
    color: var(--color-cream);
  }
</style>
