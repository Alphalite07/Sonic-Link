import { defineConfig } from "vite";
import { VitePWA } from "vite-plugin-pwa";

export default defineConfig(({ mode }) => {
  const base = mode === "gh-pages" ? "/Sonic-Link/" : "/";

  return {
    base,
    clearScreen: false,
    server: {
      port: 1420,
      strictPort: true,
      allowedHosts: true,
      watch: {
        ignored: ["**/target/**", "**/src-tauri/**", "**/crates/**"],
      },
    },
    preview: {
      port: 4173,
      allowedHosts: true,
    },
    plugins: [
      VitePWA({
        registerType: "autoUpdate",
        workbox: {
          globPatterns: ["**/*.{js,css,html,wasm,svg,png,ico}"],
          maximumFileSizeToCacheInBytes: 6 * 1024 * 1024,
        },
        manifest: {
          name: "SonicLink — Encrypted Acoustic Modem",
          short_name: "SonicLink",
          description: "Air-gapped military-grade acoustic messaging, RGB SSTV & live drawing",
          theme_color: "#080c11",
          background_color: "#080c11",
          display: "standalone",
          start_url: base,
          scope: base,
        },
      }),
    ],
  };
});