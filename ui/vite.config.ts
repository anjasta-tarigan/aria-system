import { defineConfig } from "vite";

export default defineConfig({
  root: __dirname,
  // Relative asset paths so the built stylesheet/script resolve against the same
  // origin as the document. Tauri serves the bundle over its asset protocol,
  // where Vite's default absolute `/assets/...` paths (and the `crossorigin`
  // attribute) resolve to a different origin and the browser blocks the CSS,
  // leaving the UI completely unstyled.
  base: "./",
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
