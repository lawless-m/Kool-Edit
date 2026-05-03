import { defineConfig } from "vite";

// SharedArrayBuffer (used for engine ↔ AudioWorklet ring buffers per
// 02-architecture.md) requires cross-origin isolation. Set the headers in
// dev/preview so AudioWorklet wiring works the same way it will in production.
export default defineConfig({
  server: {
    headers: {
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
    },
  },
  preview: {
    headers: {
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
    },
  },
  worker: {
    format: "es",
  },
});
