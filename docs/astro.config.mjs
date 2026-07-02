// @ts-check
import { defineConfig } from "astro/config";

// Static site — Vercel builds `dist/` and serves it as static files.
export default defineConfig({
  output: "static",
});
