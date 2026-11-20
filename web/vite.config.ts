import { defineConfig } from "vite";

// The API is same-origin in production (the WAF routes /api and /venticode on
// the workspace host to control). In development it is the synthetic mock.
const api = process.env.VENTICODE_API || "http://127.0.0.1:4189";

export default defineConfig({
  envDir: false,
  server: {
    proxy: {
      "/api/v1/venticode/browser-agent": { target: api, ws: true },
      "/api/v1/venticode/computer/screen": { target: api, ws: true },
      "/api": { target: api },
      "/venticode": { target: api },
      "/__dev": { target: api },
    },
  },
  build: { assetsInlineLimit: 0, sourcemap: false },
});
