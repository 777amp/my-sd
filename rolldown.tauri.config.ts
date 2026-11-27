// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Browser-safe Rolldown configuration for the Tauri v2 (WebKitGTK) frontend.
 *
 * Aliases `electron` -> `ts/shims/electronTauriShim.dom.ts` and emits
 * browser ES bundles into `dist-tauri/bundles/` without modifying the
 * existing Electron `rolldown.config.ts`.
 */

import { resolve } from 'node:path';
import { defineConfig } from 'rolldown';
import type { RolldownOptions } from 'rolldown';

const isProd = process.argv.some(arg => arg === '--minify');
const electronShimPath = resolve(__dirname, 'ts/shims/electronTauriShim.dom.ts');

const sandboxPreload: Record<string, string> = {
  'preload/about': 'ts/windows/about/preload.sandbox.ts',
  'preload/debuglog': 'ts/windows/debuglog/preload.sandbox.ts',
  'preload/loading': 'ts/windows/loading/preload.sandbox.ts',
  'preload/permissions': 'ts/windows/permissions/preload.sandbox.ts',
};

const sandboxDOM: Record<string, string> = {
  'dom/about': 'ts/windows/about/app.dom.tsx',
  'dom/debuglog': 'ts/windows/debuglog/app.dom.tsx',
  'dom/loading': 'ts/windows/loading/start.dom.ts',
  'dom/permissions': 'ts/windows/permissions/app.dom.tsx',
};

const baseOptions: Partial<RolldownOptions> = {
  platform: 'browser',
  resolve: {
    alias: {
      electron: electronShimPath,
    },
  },
  transform: {
    jsx: 'react-jsx',
    define: {
      'process.env.IS_BUNDLED': 'true',
      'process.env.SIGNAL_BACKEND': '"tauri-rust"',
      'process.env.NODE_ENV': isProd ? '"production"' : '"development"',
    },
  },
};

export default defineConfig([
  ...Object.entries(sandboxPreload).map(([key, entry]): RolldownOptions => ({
    ...baseOptions,
    input: { [key]: entry },
    output: {
      format: 'iife',
      dir: 'dist-tauri/bundles',
      codeSplitting: false,
      sourcemap: !isProd,
    },
  })),
  {
    ...baseOptions,
    input: sandboxDOM,
    output: {
      format: 'es',
      dir: 'dist-tauri/bundles',
      chunkFileNames: 'chunks/[name]-[hash].js',
      sourcemap: !isProd,
    },
  },
  {
    ...baseOptions,
    input: {
      'shims/tauriBridge': 'ts/shims/tauriBridge.dom.ts',
    },
    output: {
      format: 'es',
      dir: 'dist-tauri/bundles',
      codeSplitting: false,
      sourcemap: !isProd,
    },
  },
]);
