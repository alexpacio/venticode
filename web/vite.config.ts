import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The API is same-origin in production (the WAF routes /api and /venticode on
// the workspace host to control). In development it is the synthetic mock.
const api = process.env.VENTICODE_API || "http://127.0.0.1:4189";

export default defineConfig({
  plugins: [react()],
  envDir: false,
  server: {
    proxy: {
      "/api": { target: api },
      "/venticode": { target: api },
      "/__dev": { target: api },
    },
  },
  build: { assetsInlineLimit: 0, sourcemap: false },
});
