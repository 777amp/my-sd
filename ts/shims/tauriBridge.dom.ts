// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Drop-in Electron `ipcRenderer`-compatible shim backed by Tauri v2's Rust IPC.
 *
 * Maps Signal Desktop's existing Electron IPC channels (`sql-channel:*`,
 * `erase-*`, `settings:*`, `SignalClipboard.*`, window/tray controls, etc.)
 * to Tauri `#[tauri::command]` handlers in `src-tauri/src/ipc.rs`.
 */

type TauriGlobal = {
  core?: {
    invoke: <T = unknown>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
  };
  event?: {
    listen: <T = unknown>(
      event: string,
      handler: (event: { payload: T }) => void
    ) => Promise<() => void>;
  };
};

function getTauri(): TauriGlobal | undefined {
  return (globalThis as unknown as { __TAURI__?: TauriGlobal }).__TAURI__;
}

export function isTauriRuntime(): boolean {
  return Boolean(getTauri()?.core?.invoke);
}

async function tauriInvoke<T = unknown>(
  cmd: string,
  args?: Record<string, unknown>
): Promise<T> {
  const invoke = getTauri()?.core?.invoke;
  if (!invoke) {
    throw new Error(`Tauri runtime not available for command '${cmd}'`);
  }
  return invoke<T>(cmd, args);
}

type Listener = (event: { sender: null }, ...args: Array<unknown>) => void;

const unlistenMap = new Map<string, Map<Listener, () => void>>();

/**
 * Translates Electron `ipcRenderer.invoke(channel, ...args)` to Rust Tauri commands.
 */
export async function invoke<T = unknown>(
  channel: string,
  ...args: Array<unknown>
): Promise<T> {
  // Dynamic settings:* channels
  if (channel.startsWith('settings:get:')) {
    const key = channel.slice('settings:get:'.length);
    return tauriInvoke<T>('settings_get', { key });
  }
  if (channel.startsWith('settings:set:')) {
    const key = channel.slice('settings:set:'.length);
    return tauriInvoke<T>('settings_set', { key, value: args[0] });
  }
  if (channel.startsWith('sidecar:')) {
    const method = channel.slice('sidecar:'.length);
    return tauriInvoke<T>('sidecar_invoke', { method, params: args[0] ?? {} });
  }

  switch (channel) {
    case 'sql-channel:read':
      return tauriInvoke<T>('sql_read', {
        callName: String(args[0]),
        args: Array.isArray(args[1]) ? args[1] : [],
      });
    case 'sql-channel:write':
      return tauriInvoke<T>('sql_write', {
        callName: String(args[0]),
        args: Array.isArray(args[1]) ? args[1] : [],
      });
    case 'sql-channel:remove-db':
      return tauriInvoke<T>('sql_remove_db');
    case 'erase-sql-key':
      return tauriInvoke<T>('erase_sql_key');
    case 'pause-sql-writes':
      return tauriInvoke<T>('pause_sql_writes');
    case 'resume-sql-writes':
      return tauriInvoke<T>('resume_sql_writes');

    case 'erase-attachments':
      return tauriInvoke<T>('erase_attachments');
    case 'erase-stickers':
      return tauriInvoke<T>('erase_stickers');
    case 'erase-temp':
      return tauriInvoke<T>('erase_temp');
    case 'erase-drafts':
      return tauriInvoke<T>('erase_drafts');
    case 'erase-downloads':
      return tauriInvoke<T>('erase_downloads');
    case 'cleanup-downloads':
      return tauriInvoke<T>('cleanup_downloads');
    case 'cleanup-orphaned-attachments':
      return tauriInvoke<T>('cleanup_orphaned_attachments', {
        knownPaths: Array.isArray(args[0]) ? args[0] : [],
      });

    case 'get-config':
      return tauriInvoke<T>('get_config');
    case 'get-user-data-path':
      return tauriInvoke<T>('get_user_data_path');
    case 'OS.getClassName':
      return tauriInvoke<T>('os_get_class_name');
    case 'getZoomFactor':
      return tauriInvoke<T>('get_zoom_factor');
    case 'get-auto-launch':
      return tauriInvoke<T>('get_auto_launch');
    case 'set-auto-launch':
      return tauriInvoke<T>('set_auto_launch', { enabled: Boolean(args[0]) });
    case 'show-save-dialog': {
      const opts = (args[0] as { defaultPath?: string } | undefined) ?? {};
      return tauriInvoke<T>('show_save_dialog', {
        options: { default_path: opts.defaultPath ?? null },
      });
    }
    case 'delete-all-logs':
      return tauriInvoke<T>('delete_all_logs');

    default:
      throw new Error(`Unhandled Tauri IPC invoke channel: ${channel}`);
  }
}

/**
 * Translates Electron `ipcRenderer.send(channel, ...args)` to fire-and-forget Rust commands.
 */
export function send(channel: string, ...args: Array<unknown>): void {
  const fire = (cmd: string, payload?: Record<string, unknown>) => {
    void tauriInvoke(cmd, payload).catch(err => {
      // oxlint-disable-next-line no-console
      console.error(`[tauriBridge] send('${channel}') -> '${cmd}' failed:`, err);
    });
  };

  switch (channel) {
    case 'show-window':
      fire('show_window');
      return;
    case 'draw-attention':
      fire('draw_attention');
      return;
    case 'set-badge-count':
      fire('set_badge_count', { badgeCount: Number(args[0] ?? 0) });
      return;
    case 'update-tray-icon':
      fire('update_tray_icon', { unreadCount: Number(args[0] ?? 0) });
      return;
    case 'setZoomFactor':
      fire('set_zoom_factor', { zoomFactor: Number(args[0] ?? 1.0) });
      return;
    case 'show-item-in-folder':
      fire('show_item_in_folder', { path: String(args[0] ?? '') });
      return;
    case 'SignalClipboard.clear':
      fire('clipboard_clear');
      return;
    case 'SignalClipboard.clearIfNeeded':
      fire('clipboard_clear_if_needed', { expectedText: String(args[0] ?? '') });
      return;
    case 'SignalClipboard.writeText':
      fire('clipboard_write_text', { text: String(args[0] ?? '') });
      return;
    case 'restart':
      fire('restart_app');
      return;
    case 'shutdown':
      fire('shutdown_app');
      return;
    case 'log-from-renderer':
      fire('log_from_renderer', {
        level: String(args[0] ?? 'info'),
        scope: String(args[1] ?? 'renderer'),
        message: String(args[2] ?? ''),
      });
      return;
    default:
      // Non-critical Electron-only UI events (e.g. menu bar visibility) are no-ops
      return;
  }
}

export function on(channel: string, listener: Listener): void {
  const listen = getTauri()?.event?.listen;
  if (!listen) {
    return;
  }
  void listen(channel, ev => {
    listener({ sender: null }, ev.payload);
  }).then(unlisten => {
    let channelMap = unlistenMap.get(channel);
    if (!channelMap) {
      channelMap = new Map();
      unlistenMap.set(channel, channelMap);
    }
    channelMap.set(listener, unlisten);
  });
}

export function once(channel: string, listener: Listener): void {
  const wrapped: Listener = (ev, ...rest) => {
    removeListener(channel, wrapped);
    listener(ev, ...rest);
  };
  on(channel, wrapped);
}

export function removeListener(channel: string, listener: Listener): void {
  const channelMap = unlistenMap.get(channel);
  const unlisten = channelMap?.get(listener);
  if (unlisten) {
    unlisten();
    channelMap?.delete(listener);
  }
}

export const tauriIpcRenderer = {
  invoke,
  send,
  on,
  once,
  removeListener,
};
