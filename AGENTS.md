# AGENTS.md — Handoff & Repository Guide (`777amp/my-sd`)

## Target Hardware & OS
- **Device:** Raspberry Pi 4B (BCM2711 quad-core Cortex-A72, VideoCore VI GPU, 2/4/8 GB RAM, microSD storage)
- **OS:** Debian GNU/Linux 13 (Trixie) `arm64` (Wayland `labwc`/Wayfire or X11)

## Branch Strategy
- **`main`**: Clean upstream Signal Desktop (`8.39.0`, Electron-based, synced from `777amp/SD-temp`) with aggressive, non-breaking Raspberry Pi 4B / Debian 13 optimizations. **Do not rewrite or replace the Electron app on `main`.**
- **`rust-tauri`**: Archived full Rust / Tauri v2 + `presage` implementation (preserved intact, not built on `main`).

## Active RPi4 / Debian 13 Optimizations on `main`
- **Electron / Chromium & GPU (`app/main.main.ts`)**:
  - `defaultWebPrefs`: `backgroundThrottling: true`, `v8CacheOptions: 'bypassHeatCheck'`.
  - Wayland/X11 auto-detect (`--ozone-platform-hint=auto`, `WaylandWindowDecorations`).
  - VideoCore VI GPU tuning (`--ignore-gpu-blocklist`, `--enable-gpu-rasterization`, `--enable-zero-copy`, `CanvasOopRasterization`; Intel-only `VaapiVideoDecodeLinuxGL` intentionally omitted; escape hatch: `SIGNAL_RPI_SW_RENDER=1`).
  - Dynamic V8 heap cap (`--max-old-space-size=512|1024|1536` + `--optimize-for-size` on ≤4 GB RAM).
  - Disabled Chromium background networking, breakpad, component updates, domain reliability, speech API, sync, `MediaRouter`, `CalculateNativeWinOcclusion`; capped `disk-cache-size` (32 MB) and `media-cache-size` (16 MB).
- **microSD-Friendly SQLCipher & Batched Logging**:
  - `ts/sql/Server.node.ts`: WAL `synchronous = NORMAL`, `temp_store = MEMORY`, `cache_size = -16000` (16 MB), `mmap_size = 67108864` (64 MB), `wal_autocheckpoint = 1000` on Linux.
  - `ts/util/rotatingPinoDest.node.ts`: `sync: true` with `minLength: 8192` on Linux.
    - **Gotchas (verified in CI):**
      - `minLength` must be `< 16384` (sonic-boom default `maxWrite`).
      - Do **not** set `sync: false` (async fd open causes `maybeRotate()`'s `fs.statSync` and `flushSync()` to throw `sonic boom is not ready yet`).
      - Do **not** set `periodicFlush` (Electron renderer preload `setInterval` returns a numeric ID without `.unref()`).
- **RAM Caches, Virtualization & Media**:
  - `app/OptionalResourceService.main.ts`: `MAX_CACHE_SIZE` capped to 16 MB on Linux (down from 50 MB).
  - `ts/components/fun/panels/FunPanelGifs.dom.tsx`: `FunGifBlobCache` capped to 16 MB; `overscan` reduced to `GIF_WATERFALL_COLUMNS`.
  - `ts/components/fun/panels/FunPanelEmojis.dom.tsx` & `FunPanelStickers.dom.tsx`: `overscan` reduced `8` → `2`.
  - `ts/hooks/useReducedMotion.dom.ts`: Always returns `true` (enables Signal's native reduced-motion mode across stickers, GIF picker, and JS animations).
  - `ts/components/conversation/GIF.dom.tsx`: `autoPlay={false}`, `preload="metadata"`.
- **VideoCore VI CSS (`stylesheets/_rpi4_optimizations.scss`, imported in `stylesheets/manifest.scss`)**:
  - Strips `backdrop-filter: blur(...)`, collapses transitions/animations to `0.001ms` (preserves `transitionend`/`animationend`), disables smooth scrolling, strips non-focus `box-shadow` while preserving `:focus`/`:focus-visible`/`:focus-within` rings.
- **Debian 13 ARM64 Packaging (`package.json`, `build/available-locales.json`, `scripts/generate-compact-locales.mjs`, `scripts/copy-language-packs.mjs`)**:
  - `arch: "arm64"`, `version: "8.39.0"` (production mode), `libasound2 | libasound2t64` for Debian 13 Trixie, English-only compact locales and Chromium `.pak` pruning (`en-US.pak`, `en-GB.pak`).

## CI & Releases
- `.github/workflows/release-rpi4.yml`: Runs natively on `ubuntu-24.04-arm`, builds `preload.bundle.cache` via `xvfb-run`, packages `signal-desktop_8.39.0_arm64.deb`, and publishes a GitHub Release with SHA-256 and copy-paste install command.
- `.github/workflows/notify-by-slack.yml`: Sends Slack notification when the release workflow finishes.

## Agent Skills & MCP
- **Caveman skill:** `.agents/skills/caveman/SKILL.md` (use when user activates `/caveman` or asks for terse/caveman mode).
- **MCP config:** `.agents/mcp_config.json` configures `@modelcontextprotocol/server-memory` (`memory` MCP). New sessions should use the `memory` MCP server to persist and recall project decisions/state when available.
- **Workflow rule:** Always inspect first, present a concise plan, and ask for confirmation via interactive multiple-choice questions (`ask_question`) before modifying code or pushing.
