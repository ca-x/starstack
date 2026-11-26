import { defineConfig } from "vite";

export default defineConfig({
  build: { target: "es2022" },
  server: { proxy: { "/api": process.env.STARSTACK_DEV_API || "http://127.0.0.1:8080" } },
});
