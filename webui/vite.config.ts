import { defineConfig } from "vite";

export default defineConfig({
  base: "/",
  build: {
    outDir: "dist",
    assetsDir: "assets",
    emptyOutDir: true,
    chunkSizeWarningLimit: 800,
    rollupOptions: { input: { index: "index.html", rtt: "rtt.html" } },
  },
});
