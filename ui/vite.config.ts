import { defineConfig } from "vite";

export default defineConfig({
  root: __dirname,
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
