// Copyright 2026 Signal Messenger, LLC / my-sd Rust Backend
// SPDX-License-Identifier: AGPL-3.0-only

/**
 * Lightweight Raspberry Pi 4B-optimized Signal UI backed directly by
 * Tauri v2 Rust SQLCipher (`sql_read`/`sql_write`), `attachment://`,
 * and the headless Node.js protocol sidecar (`sidecar_invoke`).
 */

const invoke = (cmd, args = {}) => {
  const fn = window.__TAURI__?.core?.invoke;
  if (!fn) {
    throw new Error(`Tauri IPC unavailable for command: ${cmd}`);
  }
  return fn(cmd, args);
};

async function sqlRead(callName, args = []) {
  const res = await invoke('sql_read', { callName, args });
  if (!res.ok) {
    throw new Error(res.error || `SQL read failed: ${callName}`);
  }
  return res.value;
}

async function sqlWrite(callName, args = []) {
  const res = await invoke('sql_write', { callName, args });
  if (!res.ok) {
    throw new Error(res.error || `SQL write failed: ${callName}`);
  }
  return res.value;
}

const LONG_TEXT_CHAR_LIMIT = 500;
const LONG_TEXT_LINE_LIMIT = 10;

const state = {
  conversations: [],
  convById: new Map(),
  activeConvId: null,
  activeMessages: [],
  expandedMessages: new Set(),
  oldestTimestamp: null,
  searchTerm: '',
  config: null,
};

const els = {
  appShell: document.getElementById('app'),
  convList: document.getElementById('conversation-list'),
  msgList: document.getElementById('message-list'),
  chatTitle: document.getElementById('chat-title'),
  chatSubtitle: document.getElementById('chat-subtitle'),
  chatAvatar: document.getElementById('chat-avatar'),
  btnLoadOlder: document.getElementById('btn-load-older'),
  composer: document.getElementById('composer'),
  composerInput: document.getElementById('composer-input'),
  searchInput: document.getElementById('search-input'),
  dbFooter: document.getElementById('db-footer'),
  welcomeStatus: document.getElementById('welcome-status'),
  sidecarDot: document.getElementById('sidecar-dot'),
  sidecarModal: document.getElementById('sidecar-modal'),
  sidecarInfo: document.getElementById('sidecar-info'),
  contextMenu: document.getElementById('context-menu'),
  toast: document.getElementById('toast'),
};

const AVATAR_COLORS = [
  '#3a76f0',
  '#7c4dff',
  '#00897b',
  '#e67c00',
  '#d81b60',
  '#2e7d32',
  '#5e35b1',
  '#00838f',
];

function hashColor(str) {
  let h = 0;
  for (let i = 0; i < str.length; i += 1) {
    h = (h * 31 + str.charCodeAt(i)) | 0;
  }
  return AVATAR_COLORS[Math.abs(h) % AVATAR_COLORS.length];
}

function getDisplayName(conv) {
  if (!conv) return 'Unknown';
  const full = [conv.profileName, conv.profileFamilyName]
    .filter(Boolean)
    .join(' ')
    .trim();
  return (
    conv.name ||
    conv.profileFullName ||
    full ||
    conv.e164 ||
    conv.username ||
    conv.serviceId ||
    conv.id ||
    'Unknown'
  );
}

function getInitials(name) {
  const parts = String(name).trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return '?';
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[parts.length - 1][0]).toUpperCase();
}

function formatTime(ts) {
  if (!ts) return '';
  const d = new Date(ts);
  if (Number.isNaN(d.getTime())) return '';
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  if (sameDay) {
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  }
  return d.toLocaleDateString([], { month: 'short', day: 'numeric' });
}

function buildAvatarUrl(conv) {
  const av = conv?.avatar || conv?.profileAvatar;
  if (!av || !av.path) return null;
  const cleanPath = String(av.path).replace(/^\/+/, '');
  const keys = av.localKey ? `?keys=${encodeURIComponent(av.localKey)}` : '';
  return `attachment://avatars/${cleanPath}${keys}`;
}

function buildAttachmentUrl(att) {
  if (!att || !att.path) return null;
  const cleanPath = String(att.path).replace(/^\/+/, '');
  const keys = att.localKey ? `?keys=${encodeURIComponent(att.localKey)}` : '';
  return `attachment://attachments/${cleanPath}${keys}`;
}

function renderAvatar(el, conv) {
  el.textContent = '';
  const name = getDisplayName(conv);
  el.style.background = hashColor(conv?.id || name);
  const url = buildAvatarUrl(conv);
  if (url) {
    const img = document.createElement('img');
    img.src = url;
    img.alt = name;
    img.onerror = () => {
      img.remove();
      el.textContent = getInitials(name);
    };
    el.appendChild(img);
  } else {
    el.textContent = getInitials(name);
  }
}

function renderConversations() {
  const q = state.searchTerm.trim().toLowerCase();
  const list = state.conversations.filter(c => {
    if (!c || c.isArchived) return false;
    if (!q) return true;
    const hay = [
      getDisplayName(c),
      c.e164,
      c.lastMessage,
      c.id,
    ]
      .filter(Boolean)
      .join(' ')
      .toLowerCase();
    return hay.includes(q);
  });

  els.convList.textContent = '';
  if (list.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = q
      ? 'No matching conversations.'
      : 'No conversations found in database.';
    els.convList.appendChild(empty);
    return;
  }

  const frag = document.createDocumentFragment();
  for (const conv of list.slice(0, 250)) {
    const item = document.createElement('div');
    item.className =
      'conv-item' + (conv.id === state.activeConvId ? ' active' : '');
    item.onclick = () => selectConversation(conv.id);

    const av = document.createElement('div');
    av.className = 'avatar';
    renderAvatar(av, conv);

    const body = document.createElement('div');
    body.className = 'conv-body';

    const top = document.createElement('div');
    top.className = 'conv-top';

    const nameEl = document.createElement('div');
    nameEl.className = 'conv-name';
    nameEl.textContent = getDisplayName(conv);

    const timeEl = document.createElement('div');
    timeEl.className = 'conv-time';
    timeEl.textContent = formatTime(conv.active_at);

    top.appendChild(nameEl);
    top.appendChild(timeEl);

    const preview = document.createElement('div');
    preview.className = 'conv-preview';
    preview.textContent =
      conv.lastMessage ||
      (conv.type === 'group' ? 'Group conversation' : conv.e164 || '');

    body.appendChild(top);
    body.appendChild(preview);

    item.appendChild(av);
    item.appendChild(body);
    frag.appendChild(item);
  }
  els.convList.appendChild(frag);
}

function renderMessageElement(msg, isGroup) {
  const row = document.createElement('div');
  const type = msg.type;
  const isOutgoing = type === 'outgoing';
  const isIncoming = type === 'incoming';
  row.className = `msg-row ${
    isOutgoing ? 'outgoing' : isIncoming ? 'incoming' : 'system'
  }`;

  const bubble = document.createElement('div');
  bubble.className = 'msg-bubble';

  if (isGroup && isIncoming && (msg.sourceServiceId || msg.source)) {
    const sender = document.createElement('div');
    sender.className = 'msg-sender';
    const senderConv = state.conversations.find(
      c =>
        (msg.sourceServiceId && c.serviceId === msg.sourceServiceId) ||
        (msg.source && c.e164 === msg.source)
    );
    sender.textContent = senderConv
      ? getDisplayName(senderConv)
      : msg.source || msg.sourceServiceId;
    bubble.appendChild(sender);
  }

  if (msg.quote?.text) {
    const q = document.createElement('div');
    q.className = 'msg-quote';
    q.textContent = msg.quote.text;
    bubble.appendChild(q);
  }

  if (Array.isArray(msg.attachments) && msg.attachments.length > 0) {
    const attWrap = document.createElement('div');
    attWrap.className = 'msg-attachments';
    for (const att of msg.attachments) {
      const url = buildAttachmentUrl(att);
      if (url && String(att.contentType || '').startsWith('image/')) {
        const img = document.createElement('img');
        img.className = 'msg-img';
        img.loading = 'lazy';
        img.src = url;
        img.alt = att.fileName || 'Image';
        attWrap.appendChild(img);
      } else {
        const pill = document.createElement('div');
        pill.className = 'msg-file-pill';
        pill.textContent = `📎 ${att.fileName || att.contentType || 'Attachment'}`;
        attWrap.appendChild(pill);
      }
    }
    bubble.appendChild(attWrap);
  }

  const text =
    msg.body ||
    (msg.sticker ? '[Sticker]' : '') ||
    (msg.call_history ? '[Call]' : '') ||
    (!msg.attachments?.length ? `[${type || 'event'}]` : '');

  const msgKey = String(msg.id || msg.sent_at || msg.received_at || '');
  const collapsible = isLongText(text);
  const expanded = state.expandedMessages.has(msgKey);

  if (text) {
    const textNode = document.createElement('div');
    textNode.className = 'msg-text';
    textNode.textContent = collapsible && !expanded ? truncateText(text) : text;
    bubble.appendChild(textNode);

    if (collapsible) {
      const toggleBtn = document.createElement('button');
      toggleBtn.type = 'button';
      toggleBtn.className = 'expand-toggle-btn';
      toggleBtn.textContent = expanded ? 'Show less' : 'Show more';
      toggleBtn.onclick = ev => {
        ev.stopPropagation();
        toggleMessageExpanded(msgKey);
      };
      bubble.appendChild(toggleBtn);
    }
  }

  bubble.addEventListener('contextmenu', ev => {
    ev.preventDefault();
    showMessageContextMenu(ev, msg, text, msgKey, collapsible);
  });

  const meta = document.createElement('div');
  meta.className = 'msg-meta';
  meta.textContent = formatTime(msg.sent_at || msg.received_at || msg.timestamp);
  bubble.appendChild(meta);

  row.appendChild(bubble);

  if (Array.isArray(msg.reactions) && msg.reactions.length > 0) {
    const reactWrap = document.createElement('div');
    reactWrap.className = 'msg-reactions';
    for (const r of msg.reactions) {
      const pill = document.createElement('span');
      pill.className = 'reaction-pill';
      pill.textContent = r.emoji || '👍';
      reactWrap.appendChild(pill);
    }
    row.appendChild(reactWrap);
  }

  return row;
}

function renderMessages(scrollToBottom = true) {
  const conv = state.convById.get(state.activeConvId);
  const isGroup = conv?.type === 'group';

  els.msgList.textContent = '';
  if (state.activeMessages.length === 0) {
    const empty = document.createElement('div');
    empty.className = 'empty-state';
    empty.textContent = 'No messages in this conversation yet.';
    els.msgList.appendChild(empty);
    return;
  }

  const frag = document.createDocumentFragment();
  for (const msg of state.activeMessages) {
    frag.appendChild(renderMessageElement(msg, isGroup));
  }
  els.msgList.appendChild(frag);

  if (scrollToBottom) {
    els.msgList.scrollTop = els.msgList.scrollHeight;
  }
}

async function selectConversation(convId) {
  state.activeConvId = convId;
  renderConversations();

  const conv = state.convById.get(convId);
  els.chatTitle.textContent = getDisplayName(conv);
  els.chatSubtitle.textContent =
    conv?.type === 'group'
      ? `Group · ${conv.members?.length || 0} members`
      : conv?.e164 || conv?.serviceId || conv?.id || '';
  els.chatAvatar.classList.remove('hidden');
  renderAvatar(els.chatAvatar, conv);
  els.composer.classList.remove('hidden');

  try {
    const rows = await sqlRead('getMessagesByConversation', [convId, 80]);
    const ordered = Array.isArray(rows) ? rows.slice().reverse() : [];
    state.activeMessages = ordered;
    state.oldestTimestamp =
      ordered.length > 0
        ? ordered[0].received_at || ordered[0].sent_at || null
        : null;
    els.btnLoadOlder.classList.toggle('hidden', ordered.length < 80);
    renderMessages(true);
  } catch (err) {
    els.msgList.textContent = '';
    const box = document.createElement('div');
    box.className = 'empty-state';
    box.textContent = `Error loading messages: ${err.message}`;
    els.msgList.appendChild(box);
  }
}

async function loadOlderMessages() {
  if (!state.activeConvId || !state.oldestTimestamp) return;
  try {
    const older = await sqlRead('getOlderMessagesByConversation', [
      state.activeConvId,
      state.oldestTimestamp,
      80,
    ]);
    if (!Array.isArray(older) || older.length === 0) {
      els.btnLoadOlder.classList.add('hidden');
      return;
    }
    const ordered = older.slice().reverse();
    state.activeMessages = [...ordered, ...state.activeMessages];
    state.oldestTimestamp =
      ordered[0].received_at || ordered[0].sent_at || state.oldestTimestamp;
    els.btnLoadOlder.classList.toggle('hidden', older.length < 80);
    renderMessages(false);
  } catch (err) {
    // oxlint-disable-next-line no-console
    console.error('loadOlderMessages failed:', err);
  }
}

async function loadConversations() {
  try {
    const [convs, msgCount] = await Promise.all([
      sqlRead('getAllConversations'),
      sqlRead('getMessageCount'),
    ]);
    state.conversations = Array.isArray(convs)
      ? convs.sort((a, b) => {
          const pinDiff = (b.isPinned ? 1 : 0) - (a.isPinned ? 1 : 0);
          if (pinDiff !== 0) return pinDiff;
          return (b.active_at || 0) - (a.active_at || 0);
        })
      : [];
    state.convById.clear();
    for (const c of state.conversations) {
      if (c?.id) state.convById.set(c.id, c);
    }

    renderConversations();
    const summary = `${state.conversations.length} chats · ${Number(msgCount || 0).toLocaleString()} messages`;
    els.dbFooter.textContent = summary;
    if (els.welcomeStatus) {
      els.welcomeStatus.textContent = `SQLCipher unlocked: ${summary}`;
    }

    if (!state.activeConvId && state.conversations.length > 0) {
      const firstActive = state.conversations.find(c => !c.isArchived);
      if (firstActive) {
        await selectConversation(firstActive.id);
      }
    }
  } catch (err) {
    els.convList.textContent = '';
    const box = document.createElement('div');
    box.className = 'empty-state';
    box.textContent = `Database read error: ${err.message}`;
    els.convList.appendChild(box);
  }
}

async function refreshSignalModal() {
  try {
    const st = await invoke('signal_status');
    els.sidecarDot.className = st.linked ? 'status-dot ok' : 'status-dot warn';

    const qrBox = document.getElementById('qr-box');
    const qrHint = document.getElementById('qr-hint');
    if (st.linked) {
      qrBox.innerHTML = `<span class="qr-placeholder" style="color:#188038;font-weight:600;">✓ Linked to Signal<br/>${st.phone || st.aci || ''}</span>`;
      qrHint.textContent =
        'This Raspberry Pi is linked via pure-Rust Signal (presage). Messages you send will sync across your devices.';
    } else if (st.qrSvg) {
      qrBox.innerHTML = st.qrSvg;
      qrHint.innerHTML =
        'Scan this QR code in <strong>Signal → Settings → Linked Devices → Link New Device</strong>.';
    } else if (st.linking) {
      qrBox.innerHTML =
        '<span class="qr-placeholder">Connecting to Signal servers to fetch QR code...</span>';
    }

    els.sidecarInfo.textContent = JSON.stringify(
      {
        backend: 'pure-rust-presage',
        linked: st.linked,
        linking: st.linking,
        aci: st.aci,
        phone: st.phone,
        deviceName: st.deviceName,
        lastError: st.lastError,
        userDataPath: state.config?.userDataPath,
      },
      null,
      2
    );
  } catch (err) {
    els.sidecarDot.className = 'status-dot warn';
    els.sidecarInfo.textContent = `Signal engine status error: ${err}`;
  }
}

async function startQrLinking() {
  const qrBox = document.getElementById('qr-box');
  qrBox.innerHTML =
    '<span class="qr-placeholder">Requesting real Signal linking QR code...</span>';
  try {
    await invoke('signal_start_linking', { deviceName: 'Signal-Pi4' });
    await refreshSignalModal();
  } catch (err) {
    qrBox.innerHTML = `<span class="qr-placeholder" style="color:#d93025;">Linking failed: ${err}</span>`;
    els.sidecarInfo.textContent = String(err);
  }
}

function setupSignalEventListeners() {
  const listen = window.__TAURI__?.event?.listen;
  if (!listen) return;

  void listen('signal:qr', ev => {
    const payload = ev.payload || {};
    const qrBox = document.getElementById('qr-box');
    if (payload.svg && qrBox) {
      qrBox.innerHTML = payload.svg;
    }
    void refreshSignalModal();
  });

  void listen('signal:linked', () => {
    els.sidecarDot.className = 'status-dot ok';
    void refreshSignalModal();
    void loadConversations();
  });

  void listen('signal:link-error', ev => {
    const err = ev.payload?.error || 'Unknown linking error';
    const qrBox = document.getElementById('qr-box');
    if (qrBox) {
      qrBox.innerHTML = `<span class="qr-placeholder" style="color:#d93025;">${err}</span>`;
    }
    void refreshSignalModal();
  });

  void listen('signal:live-message', ev => {
    const m = ev.payload || {};
    const activeConv = state.convById.get(state.activeConvId);
    if (!activeConv) return;

    const matchesDirect =
      activeConv.type !== 'group' &&
      ((m.senderServiceId &&
        String(activeConv.serviceId || '').includes(m.senderServiceId)) ||
        (m.destinationServiceId &&
          String(activeConv.serviceId || '').includes(m.destinationServiceId)) ||
        (m.destinationE164 && activeConv.e164 === m.destinationE164));

    const matchesGroup =
      activeConv.type === 'group' &&
      m.groupMasterKey &&
      (activeConv.masterKey === m.groupMasterKey ||
        activeConv.groupMasterKey === m.groupMasterKey);

    if (matchesDirect || matchesGroup) {
      state.activeMessages.push({
        id: `live-${m.timestamp || Date.now()}`,
        conversationId: state.activeConvId,
        type: m.direction === 'outgoing' ? 'outgoing' : 'incoming',
        body: m.body,
        sent_at: m.timestamp || Date.now(),
        received_at: m.timestamp || Date.now(),
        sourceServiceId: m.senderServiceId,
      });
      renderMessages(true);
    }
  });
}

let toastTimer = null;

function showToast(message) {
  if (!els.toast) return;
  els.toast.textContent = message;
  els.toast.classList.remove('hidden');
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    els.toast.classList.add('hidden');
  }, 1800);
}

async function copyToClipboard(text) {
  if (!text) return;
  try {
    await invoke('clipboard_write_text', { text });
    showToast('Copied to clipboard');
    return;
  } catch {
    // fallback to browser clipboard API
  }
  try {
    await navigator.clipboard.writeText(text);
    showToast('Copied to clipboard');
  } catch (err) {
    showToast(`Copy failed: ${err}`);
  }
}

function isLongText(text) {
  if (!text) return false;
  if (text.length > LONG_TEXT_CHAR_LIMIT) return true;
  return text.split('\n').length > LONG_TEXT_LINE_LIMIT;
}

function truncateText(text) {
  const lines = text.split('\n');
  let out =
    lines.length > LONG_TEXT_LINE_LIMIT
      ? lines.slice(0, LONG_TEXT_LINE_LIMIT).join('\n')
      : text;
  if (out.length > LONG_TEXT_CHAR_LIMIT) {
    out = out.slice(0, LONG_TEXT_CHAR_LIMIT).trimEnd();
  }
  return `${out}…`;
}

function toggleMessageExpanded(msgKey) {
  if (state.expandedMessages.has(msgKey)) {
    state.expandedMessages.delete(msgKey);
  } else {
    state.expandedMessages.add(msgKey);
  }
  const prevScroll = els.msgList.scrollTop;
  renderMessages(false);
  els.msgList.scrollTop = prevScroll;
}

function hideContextMenu() {
  if (els.contextMenu) {
    els.contextMenu.classList.add('hidden');
  }
}

function showMessageContextMenu(ev, msg, fullText, msgKey, collapsible) {
  const menu = els.contextMenu;
  if (!menu) return;
  menu.textContent = '';

  const selected = String(window.getSelection?.()?.toString() || '').trim();
  const items = [];

  if (selected) {
    items.push({
      label: 'Copy selected text',
      action: () => copyToClipboard(selected),
    });
  }

  if (fullText) {
    items.push({
      label: 'Copy message',
      action: () => copyToClipboard(fullText),
    });
  }

  const ts = msg.sent_at || msg.received_at || msg.timestamp;
  if (ts) {
    items.push({
      label: 'Copy timestamp',
      action: () => copyToClipboard(new Date(ts).toLocaleString()),
    });
  }

  if (collapsible) {
    const expanded = state.expandedMessages.has(msgKey);
    items.push({
      label: expanded ? 'Collapse message' : 'Expand message',
      action: () => toggleMessageExpanded(msgKey),
    });
  }

  if (items.length === 0) return;

  for (const item of items) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'context-menu-item';
    btn.textContent = item.label;
    btn.onclick = () => {
      hideContextMenu();
      void item.action();
    };
    menu.appendChild(btn);
  }

  menu.classList.remove('hidden');
  const rect = menu.getBoundingClientRect();
  const x = Math.min(ev.clientX, window.innerWidth - rect.width - 8);
  const y = Math.min(ev.clientY, window.innerHeight - rect.height - 8);
  menu.style.left = `${Math.max(8, x)}px`;
  menu.style.top = `${Math.max(8, y)}px`;
}

function setSidebarCollapsed(collapsed) {
  if (!els.appShell) return;
  els.appShell.classList.toggle('sidebar-collapsed', collapsed);
  try {
    localStorage.setItem('signal_sidebar_collapsed', collapsed ? '1' : '0');
  } catch {
    // ignore storage errors
  }
}

function toggleSidebar() {
  const collapsed = !els.appShell?.classList.contains('sidebar-collapsed');
  setSidebarCollapsed(collapsed);
}

function applyTheme(theme) {
  const next = theme === 'light' ? 'light' : 'dark';
  document.documentElement.setAttribute('data-theme', next);
  return next;
}

async function init() {
  try {
    state.config = await invoke('get_config');
    if (state.config?.theme) {
      applyTheme(state.config.theme);
    }
  } catch (err) {
    // oxlint-disable-next-line no-console
    console.warn('get_config failed:', err);
  }

  try {
    if (localStorage.getItem('signal_sidebar_collapsed') === '1') {
      setSidebarCollapsed(true);
    }
  } catch {
    // ignore
  }

  setupSignalEventListeners();

  document.getElementById('btn-toggle-sidebar').onclick = () => toggleSidebar();
  document.addEventListener('keydown', ev => {
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === 'b') {
      ev.preventDefault();
      toggleSidebar();
    } else if (ev.key === 'Escape') {
      hideContextMenu();
    }
  });
  document.addEventListener('click', ev => {
    if (!els.contextMenu?.contains(ev.target)) {
      hideContextMenu();
    }
  });
  els.msgList.addEventListener('scroll', () => hideContextMenu());

  document.getElementById('btn-refresh').onclick = () => loadConversations();
  document.getElementById('btn-theme').onclick = async () => {
    const cur = document.documentElement.getAttribute('data-theme');
    const next = applyTheme(cur === 'dark' ? 'light' : 'dark');
    try {
      await invoke('settings_set', { key: 'theme', value: next });
    } catch {
      // ignore persistence error
    }
  };
  document.getElementById('btn-sidecar').onclick = () => {
    els.sidecarModal.classList.remove('hidden');
    void refreshSignalModal();
  };
  document.getElementById('btn-close-modal').onclick = () => {
    els.sidecarModal.classList.add('hidden');
  };
  document.getElementById('btn-link-qr').onclick = () => {
    void startQrLinking();
  };
  document.getElementById('btn-wal-checkpoint').onclick = async () => {
    const res = await sqlWrite('walCheckpoint');
    els.sidecarInfo.textContent = JSON.stringify({ walCheckpoint: res }, null, 2);
  };

  els.btnLoadOlder.onclick = () => loadOlderMessages();
  els.searchInput.oninput = e => {
    state.searchTerm = e.target.value;
    renderConversations();
  };

  els.composer.onsubmit = async e => {
    e.preventDefault();
    const text = els.composerInput.value.trim();
    if (!text || !state.activeConvId) return;

    const conv = state.convById.get(state.activeConvId);
    const st = await invoke('signal_status');
    if (!st.linked) {
      els.sidecarModal.classList.remove('hidden');
      void startQrLinking();
      return;
    }

    const isGroup = conv?.type === 'group';
    const recipientId = isGroup
      ? conv.masterKey || conv.groupMasterKey || conv.groupId || conv.id
      : conv?.serviceId || conv?.uuid || conv?.id;

    els.composerInput.value = '';
    const now = Date.now();
    const msg = {
      id: `local-${now}`,
      conversationId: state.activeConvId,
      type: 'outgoing',
      body: text,
      sent_at: now,
      received_at: now,
    };
    state.activeMessages.push(msg);
    renderMessages(true);

    try {
      await invoke('signal_send_message', {
        recipientType: isGroup ? 'group' : 'contact',
        recipientId: String(recipientId),
        body: text,
      });
    } catch (err) {
      msg.body = `${text}\n[Send failed: ${err}]`;
      renderMessages(true);
    }
  };

  els.composerInput.onkeydown = e => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      els.composer.requestSubmit();
    }
  };

  await loadConversations();

  // Check pure-Rust Signal link status on boot; auto-open QR modal if not linked yet
  try {
    const st = await invoke('signal_status');
    els.sidecarDot.className = st.linked ? 'status-dot ok' : 'status-dot warn';
    if (!st.linked) {
      els.sidecarModal.classList.remove('hidden');
      void startQrLinking();
    }
  } catch {
    els.sidecarDot.className = 'status-dot warn';
  }
}

void init();
