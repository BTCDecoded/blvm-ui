<svg
  class="pointer-events-none absolute inset-x-0 bottom-0 z-0 h-[42vh] w-full text-orange"
  viewBox="0 0 1440 420"
  preserveAspectRatio="none"
  aria-hidden="true"
>
  {#each [40, 95, 150, 205, 260, 315, 370] as y, i}
    <path
      d="M0 {y + 50} C 160 {y - 20 - i * 3}, 320 {y + 70}, 480 {y + 8} S 800 {y + 80}, 960 {y} S 1280 {y + 60}, 1440 {y + 16}"
      fill="none"
      stroke="currentColor"
      stroke-opacity={0.08 + i * 0.018}
      stroke-width="1.1"
    />
  {/each}
  {#each { length: 24 } as _, i}
    <path
      d="M{60 * i} 420 C {60 * i + 10} 280, {60 * i + 28} 160, {60 * i + 40} 36"
      fill="none"
      stroke="currentColor"
      stroke-opacity="0.07"
      stroke-width="1"
    />
  {/each}
</svg>
