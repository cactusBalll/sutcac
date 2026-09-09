import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Dev server proxies the catus-server API/WebSocket, so `npm run dev`
// here works against `cargo run -p catus-server` on the default port.
const SERVER_TARGET = "http://127.0.0.1:3117";

export default defineConfig({
  plugins: [vue()],
  server: {
    port: 4173,
    proxy: {
      "/api": { target: SERVER_TARGET, changeOrigin: true },
      "/ws": { target: SERVER_TARGET, ws: true },
    },
  },
});
