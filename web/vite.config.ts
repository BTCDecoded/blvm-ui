import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";
import { defineConfig, loadEnv } from "vite";

export default defineConfig(({ mode }) => {
  // The Rust host by default. `VITE_API_PROXY=http://127.0.0.1:3850 npm run dev` develops against the mock instead.
  const apiProxy = loadEnv(mode, ".", "VITE_").VITE_API_PROXY || "http://127.0.0.1:3849";
  return {
    plugins: [tailwindcss(), svelte()],
    server: {
      port: 5174,
      strictPort: true,
      host: "127.0.0.1",
      proxy: {
        "/api": apiProxy,
      },
    },
    build: {
      outDir: "dist",
      emptyOutDir: true,
      assetsInlineLimit: 0,
    },
  };
});
