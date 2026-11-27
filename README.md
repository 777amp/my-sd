# Signal Desktop — Ultra-Optimized for Raspberry Pi 4B (`arm64`)

A **Maximum-Stripped** build of Signal Desktop tuned specifically for the **Raspberry Pi 4B** (BCM2711 Cortex-A72, VideoCore VI GPU, 2–8 GB RAM, microSD storage) running **Debian GNU/Linux 13 (Trixie)** (`arm64`).

## Quick Install

Grab the one-line terminal install command from the [Latest Release](https://github.com/777amp/my-sd/releases/latest).

## Raspberry Pi 4B Optimizations

1. **Wayland & X11 Auto-Detection (`app/main.main.ts`)**
   - Automatically enables `--ozone-platform-hint=auto` for Raspberry Pi OS's Wayland (`labwc`/Wayfire) and X11 compositors.
2. **VideoCore VI GPU Tuning (`app/main.main.ts`)**
   - Enables `--ignore-gpu-blocklist`, `--enable-gpu-rasterization`, `--enable-zero-copy`, and `VaapiVideoDecodeLinuxGL,CanvasOopRasterization,WaylandWindowDecorations`.
   - Escape hatch: run `SIGNAL_RPI_SW_RENDER=1 signal-desktop` to fall back to software rendering if needed.
3. **Dynamic V8 Heap & Idle Throttling (`app/main.main.ts`)**
   - Scales `--max-old-space-size` automatically based on total system RAM (512 MB on 2 GB Pi, 1024 MB on 4 GB Pi, 1536 MB on 8 GB Pi) with `--optimize-for-size` on ≤4 GB models.
   - Disables unnecessary Chromium background networking/sync/component-update services, caps Chromium disk cache at 32 MB, and enables `backgroundThrottling: true`.
4. **microSD-Friendly SQLCipher (`ts/sql/Server.node.ts`)**
   - Switches WAL `PRAGMA synchronous = FULL` to `PRAGMA synchronous = NORMAL` on Linux (crash-safe in WAL mode while avoiding per-commit `fsync` microSD stalls and flash wear).
   - Configures `temp_store = MEMORY`, `cache_size = -16000` (16 MB), `mmap_size = 67108864` (64 MB), and `wal_autocheckpoint = 1000`.
5. **Zero Blur / Zero UI Animations (`stylesheets/_rpi4_optimizations.scss`)**
   - Strips all GPU-heavy `backdrop-filter: blur(...)` rules (including 45px–90px blurs) and sets CSS transition/animation durations to `0.001ms` (preserving `transitionend`/`animationend` events while eliminating frame-by-frame repaints).
6. **Stripped Locales & Native ARM64 Bytecode Cache**
   - Prunes non-English Chromium `.pak` files and compact locale bundles (`scripts/copy-language-packs.mjs`, `scripts/generate-compact-locales.mjs`, `build/available-locales.json`).
   - Generates a native ARM64 V8 `preload.bundle.cache` during the GitHub Actions build for faster cold startup.
7. **Debian 13 (Trixie) `.deb` Packaging (`package.json`)**
   - Installs as `signal-desktop` (uses `~/.config/Signal`) with `libasound2 | libasound2t64` dependency compatibility.

## Automated Builds & Releases

GitHub Actions (`.github/workflows/release-rpi4.yml`) builds natively on `ubuntu-24.04-arm` on every push to `main`, `v*` tag, or manual workflow dispatch, and publishes a GitHub Release with the `.deb`, SHA-256 checksum, and copy-pasteable install command.

## License

Derived from [Signal Desktop](https://github.com/signalapp/Signal-Desktop). Licensed under **AGPL-3.0-only** (see `LICENSE`).
