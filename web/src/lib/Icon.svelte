<script lang="ts">
  let { name, class: extra = "h-4 w-4" }: { name: string; class?: string } = $props();
</script>

<svg class={extra} viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" aria-hidden="true">
  {#if name === "home"}
    <path d="M4 11.5 12 4l8 7.5V20a1 1 0 0 1-1 1h-5v-6H10v6H5a1 1 0 0 1-1-1z" stroke-linejoin="round" />
  {:else if name === "insights"}
    <path d="M5 19V10M12 19V5M19 19v-7" stroke-linecap="round" />
  {:else if name === "settings"}
    <circle cx="12" cy="12" r="3" />
    <path
      d="M12 3.5v2.2M12 18.3v2.2M4.7 7.1l1.9 1.1M17.4 15.8l1.9 1.1M4.7 16.9l1.9-1.1M17.4 8.2l1.9-1.1"
      stroke-linecap="round"
    />
  {:else if name === "node"}
    <rect x="4" y="4" width="7" height="7" rx="1" />
    <rect x="13" y="4" width="7" height="7" rx="1" />
    <rect x="4" y="13" width="7" height="7" rx="1" />
    <rect x="13" y="13" width="7" height="7" rx="1" />
  {:else if name === "mining"}
    <path d="M4 18 10 6h4l6 12" stroke-linejoin="round" />
    <path d="M8 18h8" stroke-linecap="round" />
  {:else if name === "network"}
    <circle cx="6" cy="12" r="2" />
    <circle cx="18" cy="7" r="2" />
    <circle cx="18" cy="17" r="2" />
    <path d="M8 12h8M16.4 8.4 8 12M16.4 15.6 8 12" />
  {:else if name === "logs"}
    <path d="M7 4h10v16H7z" />
    <path d="M10 8h4M10 12h4M10 16h3" stroke-linecap="round" />
  {:else if name === "bolt"}
    <path d="M13 3 5 14h7l-1 7 8-11h-7z" stroke-linejoin="round" />
  {:else if name === "pin"}
    <path d="M12 21s7-6.2 7-11a7 7 0 1 0-14 0c0 4.8 7 11 7 11z" />
    <circle cx="12" cy="10" r="2" />
  {:else if name === "users"}
    <circle cx="9" cy="8" r="3" />
    <path d="M3.5 19a5.5 5.5 0 0 1 11 0" />
    <circle cx="17" cy="9" r="2.4" />
    <path d="M16 19a4.4 4.4 0 0 0 4.8-4" />
  {:else if name === "cube"}
    <path d="M12 3 20 7.5v9L12 21 4 16.5v-9z" stroke-linejoin="round" />
    <path d="M12 21V12M20 7.5 12 12 4 7.5" />
  {:else if name === "peers"}
    <circle cx="12" cy="8" r="3" />
    <path d="M5 19a7 7 0 0 1 14 0" />
  {:else if name === "wave"}
    <path d="M3 14c2-4 4-4 6 0s4 4 6 0 4-4 6 0" stroke-linecap="round" />
  {:else if name === "link"}
    <circle cx="8" cy="12" r="3" />
    <circle cx="16" cy="8" r="2.2" />
    <circle cx="16" cy="16" r="2.2" />
    <path d="M10.6 10.4 14.2 8.8M10.6 13.6 14.2 15.2" />
  {:else if name === "shield"}
    <path d="M12 3 5 6v5.5c0 4.3 3 7.6 7 9.5 4-1.9 7-5.2 7-9.5V6z" stroke-linejoin="round" />
    <path d="M9 9l6 6M15 9l-6 6" stroke-linecap="round" />
  {:else if name === "plus"}
    <circle cx="12" cy="12" r="8.5" />
    <path d="M12 8.5v7M8.5 12h7" stroke-linecap="round" />
  {:else if name === "power"}
    <path d="M12 3.5v8" stroke-linecap="round" />
    <path d="M7 6.8a7 7 0 1 0 10 0" stroke-linecap="round" />
  {:else if name === "terminal"}
    <rect x="3.5" y="5" width="17" height="14" rx="2" />
    <path d="M7.5 10l3 2.5-3 2.5M12.5 15h4" stroke-linecap="round" stroke-linejoin="round" />
  {:else if name === "info"}
    <circle cx="12" cy="12" r="8.5" />
    <path d="M12 11v5M12 8h.01" stroke-linecap="round" />
  {/if}
</svg>
