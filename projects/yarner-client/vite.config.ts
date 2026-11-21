import { defineConfig } from "vite";

export default defineConfig({
  // Relative asset paths, so the built client also works as a static player from any path.
  base: "./",
  server: {
    // The read-only player imports the wasm build from ../yarner-wasm/pkg.
    fs: { allow: [".."] },
    proxy: {
      "/api": { target: "http://127.0.0.1:7878", ws: true, changeOrigin: true },
    },
  },
  build: { outDir: "dist", emptyOutDir: true, target: "es2022" },
});
