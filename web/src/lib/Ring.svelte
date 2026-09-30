<script lang="ts">
  let {
    inbound = 0,
    outbound = 0,
    free = 0,
    total = 0,
    mode = "peers",
    centerTitle,
    centerValue,
    centerUnit = "",
    class: extra = "h-[6.5rem] w-[6.5rem]",
  }: {
    inbound?: number;
    outbound?: number;
    free?: number;
    total?: number;
    mode?: "peers" | "disk";
    centerTitle?: string;
    centerValue: string;
    centerUnit?: string;
    class?: string;
  } = $props();

  const slices = $derived.by(() => {
    if (mode === "disk") {
      if (!total) return { a: 0, b: 0, off: 0 };
      const used = Math.max(0, total - free);
      const usedPct = (used / total) * 100;
      const freePct = (free / total) * 100;
      return { a: usedPct, b: freePct, off: -usedPct };
    }
    const sum = inbound + outbound;
    if (!sum) return { a: 0, b: 0, off: 0 };
    const outPct = (outbound / sum) * 100;
    const inPct = (inbound / sum) * 100;
    return { a: outPct, b: inPct, off: -outPct };
  });

  // Center text scales with the ring (container units) and shrinks for long values,
  // so it always fits inside the inner circle at any ring size.
  const valueSize = $derived(Math.min(21, 62 / Math.max(1, String(centerValue).length * 0.6)));
</script>

<div class="relative mx-auto {extra}" style="container-type: size">
  <svg viewBox="0 0 36 36" class="h-full w-full" aria-hidden="true">
    <circle
      cx="18"
      cy="18"
      r="15.915"
      fill="none"
      stroke="rgb(240 230 208 / 0.08)"
      stroke-width="3.4"
    />
    <circle
      cx="18"
      cy="18"
      r="15.915"
      fill="none"
      stroke={mode === "disk" ? "#f7931a" : "#fde047"}
      stroke-width="3.4"
      stroke-dasharray="{slices.a} 100"
      transform="rotate(-90 18 18)"
    />
    <circle
      cx="18"
      cy="18"
      r="15.915"
      fill="none"
      stroke={mode === "disk" ? "#3dcf8e" : "#a78bfa"}
      stroke-width="3.4"
      stroke-dasharray="{slices.b} 100"
      stroke-dashoffset={slices.off}
      transform="rotate(-90 18 18)"
    />
  </svg>
  <div class="absolute inset-0 flex flex-col items-center justify-center">
    <strong class="font-medium leading-none text-cream" style="font-size: {valueSize}cqw"
      >{centerValue}</strong
    >
    <span
      class="font-medium tracking-[0.18em] text-muted uppercase"
      style="font-size: 6.2cqw; margin-top: 3cqw"
    >
      {centerUnit || centerTitle}
    </span>
  </div>
</div>
