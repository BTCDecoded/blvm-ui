<script lang="ts">
  import { tick, type Snippet } from "svelte";

  // Native <dialog> in the console's card styling. Esc and Cancel close it, Tab stays inside it,
  // focus returns to the opener. The body is a snippet so callers can put real numbers in it.
  let {
    open = $bindable(false),
    title,
    confirmLabel = "Confirm",
    cancelLabel = "Cancel",
    busy = false,
    disabled = false,
    onconfirm,
    oncancel,
    returnTo,
    children,
  }: {
    open?: boolean;
    title: string;
    confirmLabel?: string;
    cancelLabel?: string;
    /** Confirm is in flight: both buttons are inert. */
    busy?: boolean;
    /** Confirm is not possible (for example, not enough disk space). Cancel still works. */
    disabled?: boolean;
    onconfirm: () => void;
    oncancel?: () => void;
    /** Where focus goes when the dialog closes and the element that opened it is gone (the caller rebuilt it). */
    returnTo?: () => HTMLElement | null | undefined;
    children?: Snippet;
  } = $props();

  let dlg: HTMLDialogElement | undefined = $state();
  let opener: Element | null = null;
  const titleId = `confirm-${Math.random().toString(36).slice(2, 8)}`;

  $effect(() => {
    if (!dlg) return;
    if (open && !dlg.open) {
      opener = document.activeElement;
      dlg.showModal();
      // Start on Cancel: the safe choice for a destructive question.
      dlg.querySelector<HTMLElement>("[data-cancel]")?.focus();
    } else if (!open && dlg.open) {
      dlg.close();
    }
  });

  // Fires for Esc and for dlg.close(); keeps `open` honest and gives focus back.
  function onClose() {
    if (open) {
      open = false;
      oncancel?.();
    }
    // A caller that rebuilds the opener (a remounted carousel) names where focus belongs instead. Wait a tick:
    // the rebuilt element exists only after the caller's own update has been applied.
    const was = opener;
    opener = null;
    void tick().then(() => {
      const target = was instanceof HTMLElement && was.isConnected ? was : returnTo?.();
      target?.focus();
    });
  }

  function cancel() {
    if (busy) return;
    open = false;
    oncancel?.();
  }

  function onCancelEvent(e: Event) {
    // Esc: let our own close path run so `oncancel` fires exactly once.
    e.preventDefault();
    cancel();
  }

  function onBackdrop(e: MouseEvent) {
    if (e.target === dlg) cancel();
  }

  function trap(e: KeyboardEvent) {
    if (e.key !== "Tab" || !dlg) return;
    const f = [...dlg.querySelectorAll<HTMLElement>('button, a[href], input, select, textarea, [tabindex]:not([tabindex="-1"])')].filter(
      (el) => !el.hasAttribute("disabled"),
    );
    if (f.length === 0) return;
    const first = f[0];
    const last = f[f.length - 1];
    const at = document.activeElement;
    if (e.shiftKey && (at === first || !dlg.contains(at))) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && (at === last || !dlg.contains(at))) {
      e.preventDefault();
      first.focus();
    }
  }
</script>

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_noninteractive_element_interactions -->
<dialog
  bind:this={dlg}
  class="confirm panel"
  aria-labelledby={titleId}
  onclose={onClose}
  oncancel={onCancelEvent}
  onclick={onBackdrop}
  onkeydown={trap}
>
  <div class="inner">
    <h2 id={titleId} class="ui-title">{title}</h2>
    <div class="body">{@render children?.()}</div>
    <div class="actions">
      <button type="button" class="ui-btn-ghost" data-cancel disabled={busy} onclick={cancel}>{cancelLabel}</button>
      <button type="button" class="ui-btn" disabled={busy || disabled} onclick={onconfirm}>
        {busy ? "Working…" : confirmLabel}
      </button>
    </div>
  </div>
</dialog>

<style>
  .confirm {
    width: min(32rem, calc(100vw - 2rem));
    max-height: calc(100dvh - 2rem);
    margin: auto;
    padding: 0;
    overflow: auto;
    color: var(--color-cream);
    background: var(--color-card);
    box-shadow: 0 30px 80px rgb(0 0 0 / 0.7);
  }
  .confirm::backdrop {
    background: rgb(0 0 0 / 0.62);
    backdrop-filter: blur(3px);
  }
  .inner {
    display: flex;
    flex-direction: column;
    gap: 1rem;
    padding: 1.625rem;
  }
  .body {
    display: flex;
    flex-direction: column;
    gap: 0.75rem;
    font-size: 0.95rem;
    line-height: 1.5;
    color: var(--color-muted);
  }
  .actions {
    margin-top: 0.5rem;
    display: flex;
    flex-wrap: wrap;
    justify-content: flex-end;
    gap: 0.6rem;
  }
  .actions :global(.ui-btn-ghost) {
    padding: 0.6rem 1rem;
    font-size: 0.95rem;
  }
</style>
