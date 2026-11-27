#!/usr/bin/env node
// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Stages static frontend assets into `dist-tauri/` for Tauri v2 bundling.
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const rootDir = path.resolve(__dirname, '..');
const distDir = path.join(rootDir, 'dist-tauri');

fs.rmSync(distDir, { recursive: true, force: true });
fs.mkdirSync(distDir, { recursive: true });

const filesToCopy = [
  'background.html',
  'about.html',
  'debug_log.html',
  'loading.html',
];

for (const file of filesToCopy) {
  const src = path.join(rootDir, file);
  if (fs.existsSync(src)) {
    fs.copyFileSync(src, path.join(distDir, file));
  }
}

const dirsToCopy = [
  'bundles',
  'stylesheets',
  'images',
  'fonts',
  'sounds',
  '_locales',
];

for (const dir of dirsToCopy) {
  const src = path.join(rootDir, dir);
  if (fs.existsSync(src)) {
    fs.cpSync(src, path.join(distDir, dir), { recursive: true });
  }
}

// oxlint-disable-next-line no-console
console.log(`Staged Tauri frontend assets in ${distDir}`);
