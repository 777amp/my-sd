// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Browser-safe `electron` module alias for Tauri v2 builds (`rolldown.tauri.config.ts`).
 *
 * Allows existing Signal Desktop renderer and sandbox preload modules that
 * `import { ipcRenderer, contextBridge, webFrame, clipboard } from 'electron'`
 * to run inside Tauri's WebKitGTK webview without Chromium/Electron.
 */

import { tauriIpcRenderer, invoke, send } from './tauriBridge.dom.ts';

export const ipcRenderer = tauriIpcRenderer;

export const contextBridge = {
  exposeInMainWorld(apiKey: string, api: unknown): void {
    (globalThis as Record<string, unknown>)[apiKey] = api;
  },
};

export const webFrame = {
  setZoomFactor(factor: number): void {
    send('setZoomFactor', factor);
  },
  getZoomFactor(): number {
    return 1.0;
  },
  getZoomFactorAsync(): Promise<number> {
    return invoke<number>('getZoomFactor');
  },
};

export const clipboard = {
  writeText(text: string): void {
    send('SignalClipboard.writeText', text);
  },
  clear(): void {
    send('SignalClipboard.clear');
  },
};

export const shell = {
  showItemInFolder(fullPath: string): void {
    send('show-item-in-folder', fullPath);
  },
};

export default {
  ipcRenderer,
  contextBridge,
  webFrame,
  clipboard,
  shell,
};
