import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// `npm run dev` proxies the API to `qbo-local serve` on 8780, so the UI hot
// reloads against a real replica. The Tauri build loads `dist/` directly.
export default defineConfig({
  plugins: [react()],
  base: "./",
  server: { port: 5173, proxy: { "/api": "http://127.0.0.1:8780" } },
  build: { outDir: "dist", sourcemap: false, target: "es2021" },
});
