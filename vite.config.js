import { defineConfig } from "vite";

export default defineConfig({
  root: "frontend",
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "safari15",
  },
});
