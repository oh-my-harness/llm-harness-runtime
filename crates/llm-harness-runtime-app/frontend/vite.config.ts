import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

const backendPort = process.env.RUNTIME_APP_PORT || "7878";

export default defineConfig({
  base: "/app/agent-team/",
  plugins: [react()],
  server: {
    port: 5173,
    proxy: {
      "/api": `http://127.0.0.1:${backendPort}`,
    },
  },
});
