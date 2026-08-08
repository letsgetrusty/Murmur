import { defineConfig } from "vite";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("frontend", import.meta.url));

export default defineConfig({
  root,
  clearScreen: false,
  test: {
    // Frontend unit tests (pure helpers + shared constants). jsdom gives a DOM
    // for future component-level tests; the current suite is dependency-free.
    environment: "jsdom",
    include: ["**/*.test.js"],
  },
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    target: "safari15",
    rollupOptions: {
      // Multi-page: the frameless overlay pill, the main settings window, and
      // the first-run onboarding window.
      input: {
        overlay: resolve(root, "index.html"),
        settings: resolve(root, "settings.html"),
        onboarding: resolve(root, "onboarding.html"),
      },
    },
  },
});
