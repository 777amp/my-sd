#!/usr/bin/env node
// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Headless Node.js Signal protocol sidecar for the Tauri v2 Rust backend.
 *
 * Transport: Newline-delimited JSON-RPC over stdin/stdout.
 *   Request:  { "id": 1, "method": "ping", "params": { ... } }
 *   Response: { "id": 1, "result": { ... } } | { "id": 1, "error": { "code": -1, "message": "..." } }
 *   Event:    { "event": "signal:message-received", "payload": { ... } }
 *
 * Provides:
 *   - Health & capability introspection (`ping`, `capabilities`)
 *   - `@signalapp/libsignal-client` key operations
 *   - QR device-linking WebSocket session (`linking.start`, `linking.cancel`, `linking.status`)
 *   - Authenticated Signal messaging WebSocket (`messaging.connect`, `messaging.disconnect`,
 *     `messaging.send`, `messaging.status`)
 */

import crypto from 'node:crypto';
import readline from 'node:readline';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);

const DEFAULT_CHAT_WS_URL = 'wss://chat.signal.org/v1/websocket/';
const DEFAULT_PROVISIONING_WS_URL =
  'wss://chat.signal.org/v1/websocket/provisioning/';

let libsignal = null;
let libsignalLoadError = null;

function getLibsignal() {
  if (libsignal) {
    return libsignal;
  }
  if (libsignalLoadError) {
    throw libsignalLoadError;
  }
  try {
    libsignal = require('@signalapp/libsignal-client');
    return libsignal;
  } catch (err) {
    libsignalLoadError = err;
    throw err;
  }
}

function sendLine(obj) {
  process.stdout.write(`${JSON.stringify(obj)}\n`);
}

function emitEvent(event, payload) {
  sendLine({ event, payload });
}

// ---------------------------------------------------------------------------
// QR Device Linking State
// ---------------------------------------------------------------------------

const linkingState = {
  active: false,
  socket: null,
  ephemeralPublicKeyBase64: null,
  ephemeralPrivateKeyPem: null,
  lastQrUrl: null,
};

function generateProvisioningKeyPair() {
  try {
    const ls = getLibsignal();
    const priv = ls.PrivateKey.generate();
    const pubBytes = Buffer.from(priv.getPublicKey().serialize());
    return {
      publicKeyBase64: pubBytes.toString('base64'),
      privateKeySerialized: Buffer.from(priv.serialize()).toString('base64'),
      source: 'libsignal-client',
    };
  } catch {
    // Fallback to Node.js native X25519 so linking URI generation works even without native addon
    const { publicKey, privateKey } = crypto.generateKeyPairSync('x25519');
    const rawPub = publicKey.export({ type: 'spki', format: 'der' }).subarray(-32);
    const djbPub = Buffer.concat([Buffer.from([0x05]), rawPub]);
    return {
      publicKeyBase64: djbPub.toString('base64'),
      privateKeySerialized: privateKey.export({ type: 'pkcs8', format: 'pem' }).toString(),
      source: 'node-crypto-x25519',
    };
  }
}

function cancelLinkingInternal() {
  if (linkingState.socket) {
    try {
      linkingState.socket.close(1000, 'linking-cancelled');
    } catch {
      // ignore close errors
    }
    linkingState.socket = null;
  }
  linkingState.active = false;
  linkingState.ephemeralPublicKeyBase64 = null;
  linkingState.ephemeralPrivateKeyPem = null;
  linkingState.lastQrUrl = null;
}

// ---------------------------------------------------------------------------
// Authenticated Signal Messaging WebSocket State
// ---------------------------------------------------------------------------

const messagingState = {
  state: 'disconnected', // 'disconnected' | 'connecting' | 'connected'
  socket: null,
  url: null,
  connectedAt: null,
  messagesReceived: 0,
  messagesSent: 0,
};

function setMessagingState(nextState, detail = null) {
  messagingState.state = nextState;
  emitEvent('signal:connection-state', {
    state: nextState,
    detail,
    timestamp: Date.now(),
  });
}

function disconnectMessagingInternal() {
  if (messagingState.socket) {
    try {
      messagingState.socket.close(1000, 'client-disconnect');
    } catch {
      // ignore
    }
    messagingState.socket = null;
  }
  messagingState.connectedAt = null;
  setMessagingState('disconnected');
}

// ---------------------------------------------------------------------------
// JSON-RPC Handlers
// ---------------------------------------------------------------------------

const handlers = {
  async ping(params = {}) {
    return {
      ok: true,
      pid: process.pid,
      arch: process.arch,
      nodeVersion: process.version,
      userDataDir: process.env.SIGNAL_USER_DATA_DIR || null,
      echo: params.echo ?? null,
    };
  },

  async capabilities() {
    let hasLibsignal = false;
    try {
      getLibsignal();
      hasLibsignal = true;
    } catch {
      hasLibsignal = false;
    }

    const mem = process.memoryUsage();
    return {
      hasLibsignal,
      hasWebSocket: typeof globalThis.WebSocket === 'function',
      rssMb: Math.round(mem.rss / (1024 * 1024)),
      heapUsedMb: Math.round(mem.heapUsed / (1024 * 1024)),
      platform: process.platform,
      arch: process.arch,
    };
  },

  async 'libsignal.generateIdentityKeyPair'() {
    const ls = getLibsignal();
    const kp = ls.IdentityKeyPair.generate();
    return {
      publicKeyBase64: Buffer.from(kp.publicKey.serialize()).toString('base64'),
      privateKeyBase64: Buffer.from(kp.privateKey.serialize()).toString('base64'),
    };
  },

  async 'linking.status'() {
    return {
      active: linkingState.active,
      qrUrl: linkingState.lastQrUrl,
    };
  },

  async 'linking.start'(params = {}) {
    cancelLinkingInternal();

    const wsUrl = params.provisioningUrl || DEFAULT_PROVISIONING_WS_URL;
    const keyPair = generateProvisioningKeyPair();
    linkingState.active = true;
    linkingState.ephemeralPublicKeyBase64 = keyPair.publicKeyBase64;
    linkingState.ephemeralPrivateKeyPem = keyPair.privateKeySerialized;

    if (params.offlinePreview) {
      const fakeUuid = crypto.randomUUID();
      const qrUrl = `sgnl://linkdevice?uuid=${encodeURIComponent(fakeUuid)}&pub_key=${encodeURIComponent(keyPair.publicKeyBase64)}`;
      linkingState.lastQrUrl = qrUrl;
      emitEvent('signal:linking-qr', { qrUrl, uuid: fakeUuid });
      return {
        started: true,
        qrUrl,
        keySource: keyPair.source,
      };
    }

    if (typeof globalThis.WebSocket !== 'function') {
      throw new Error('Global WebSocket is unavailable in this Node.js runtime');
    }

    const ws = new globalThis.WebSocket(wsUrl);
    linkingState.socket = ws;

    ws.addEventListener('open', () => {
      emitEvent('signal:linking-state', { state: 'connected', wsUrl });
    });

    ws.addEventListener('message', async ev => {
      try {
        const rawBuf =
          ev.data instanceof ArrayBuffer
            ? Buffer.from(ev.data)
            : Buffer.from(await ev.data.arrayBuffer());
        emitEvent('signal:linking-frame', {
          size: rawBuf.length,
          payloadBase64: rawBuf.toString('base64'),
          ephemeralPublicKeyBase64: linkingState.ephemeralPublicKeyBase64,
        });
      } catch (err) {
        emitEvent('signal:linking-error', {
          message: err instanceof Error ? err.message : String(err),
        });
      }
    });

    ws.addEventListener('close', ev => {
      const wasActive = linkingState.active;
      linkingState.active = false;
      linkingState.socket = null;
      if (wasActive) {
        emitEvent('signal:linking-state', {
          state: 'closed',
          code: ev.code,
          reason: ev.reason,
        });
      }
    });

    ws.addEventListener('error', () => {
      emitEvent('signal:linking-error', {
        message: 'Provisioning WebSocket connection error',
      });
    });

    return {
      started: true,
      wsUrl,
      ephemeralPublicKeyBase64: keyPair.publicKeyBase64,
      keySource: keyPair.source,
    };
  },

  async 'linking.cancel'() {
    cancelLinkingInternal();
    emitEvent('signal:linking-state', { state: 'cancelled' });
    return { ok: true };
  },

  async 'messaging.status'() {
    return {
      state: messagingState.state,
      url: messagingState.url,
      connectedAt: messagingState.connectedAt,
      messagesReceived: messagingState.messagesReceived,
      messagesSent: messagingState.messagesSent,
    };
  },

  async 'messaging.connect'(params = {}) {
    disconnectMessagingInternal();

    const baseUrl = params.wsUrl || DEFAULT_CHAT_WS_URL;
    const query = new URLSearchParams();
    if (params.login) {
      query.set('login', String(params.login));
    }
    if (params.password) {
      query.set('password', String(params.password));
    }
    const fullUrl = query.toString() ? `${baseUrl}?${query.toString()}` : baseUrl;

    if (typeof globalThis.WebSocket !== 'function') {
      throw new Error('Global WebSocket is unavailable in this Node.js runtime');
    }

    messagingState.url = baseUrl;
    setMessagingState('connecting');

    const ws = new globalThis.WebSocket(fullUrl);
    messagingState.socket = ws;

    ws.addEventListener('open', () => {
      messagingState.connectedAt = Date.now();
      setMessagingState('connected');
    });

    ws.addEventListener('message', async ev => {
      messagingState.messagesReceived += 1;
      try {
        const rawBuf =
          typeof ev.data === 'string'
            ? Buffer.from(ev.data, 'utf8')
            : ev.data instanceof ArrayBuffer
              ? Buffer.from(ev.data)
              : Buffer.from(await ev.data.arrayBuffer());
        emitEvent('signal:message-received', {
          receivedAt: Date.now(),
          size: rawBuf.length,
          payloadBase64: rawBuf.toString('base64'),
        });
      } catch (err) {
        emitEvent('signal:messaging-error', {
          message: err instanceof Error ? err.message : String(err),
        });
      }
    });

    ws.addEventListener('close', ev => {
      messagingState.socket = null;
      messagingState.connectedAt = null;
      setMessagingState('disconnected', { code: ev.code, reason: ev.reason });
    });

    ws.addEventListener('error', () => {
      emitEvent('signal:messaging-error', {
        message: 'Signal messaging WebSocket error',
      });
    });

    return { connecting: true, url: baseUrl };
  },

  async 'messaging.send'(params = {}) {
    if (!messagingState.socket || messagingState.state !== 'connected') {
      throw new Error('Signal messaging WebSocket is not connected');
    }
    if (!params.payloadBase64) {
      throw new Error('messaging.send requires payloadBase64');
    }
    const bytes = Buffer.from(String(params.payloadBase64), 'base64');
    messagingState.socket.send(bytes);
    messagingState.messagesSent += 1;
    return {
      sent: true,
      bytes: bytes.length,
      messagesSent: messagingState.messagesSent,
    };
  },

  async 'messaging.disconnect'() {
    disconnectMessagingInternal();
    return { ok: true };
  },

  async shutdown() {
    cancelLinkingInternal();
    disconnectMessagingInternal();
    emitEvent('sidecar:shutdown', { pid: process.pid });
    setImmediate(() => process.exit(0));
    return { ok: true };
  },
};

const rl = readline.createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
});

rl.on('line', async line => {
  const trimmed = line.trim();
  if (!trimmed) {
    return;
  }

  let msg;
  try {
    msg = JSON.parse(trimmed);
  } catch (err) {
    process.stderr.write(`Invalid JSON-RPC input: ${err.message}\n`);
    return;
  }

  const { id, method, params } = msg;
  const handler = handlers[method];
  if (!handler) {
    sendLine({
      id,
      error: {
        code: -32601,
        message: `Unknown sidecar method: ${method}`,
      },
    });
    return;
  }

  try {
    const result = await handler(params);
    sendLine({ id, result });
  } catch (err) {
    sendLine({
      id,
      error: {
        code: -32000,
        message: err instanceof Error ? err.message : String(err),
      },
    });
  }
});

rl.on('close', () => {
  cancelLinkingInternal();
  disconnectMessagingInternal();
  process.exit(0);
});

process.on('SIGTERM', () => process.exit(0));
process.on('SIGINT', () => process.exit(0));
