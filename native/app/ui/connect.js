// The home screen's connect side: the remote ID and its name, the recents, the
// "clear recents" confirmation and the copy-my-ID button. Connecting opens a
// tab (tabs.js), which shows the connection's progress and password prompt.

import * as api from './api.js';
import { preflight } from './settings.js';
import { $, S, render, toast, validUuid } from './state.js';
import { startConnecting } from './tabs.js';

// ---- config / recents ------------------------------------------------------
export async function reloadConfig() {
  S.config = await api.getConfig();
  $('my-uuid').textContent = S.config.uuid;
  renderRecents();
  return S.config;
}

const shortId = (id) => (id.length > 14 ? id.slice(0, 8) + '…' : id);

// Friendly names the user gave remote IDs (config.remoteNames, id -> name).
export const nameOf = (id) =>
  (S.config && S.config.remoteNames && id && S.config.remoteNames[id.trim().toLowerCase()]) || '';
// "Office PC (0f8fad5b-…)" when named, else the bare ID.
export const peerLabel = (id) => {
  const n = nameOf(id);
  return n ? `${n} (${id})` : id || '';
};

async function saveRemoteName(id, name) {
  if (!validUuid(id) || nameOf(id) === name.trim()) return;
  try {
    S.config.remoteNames = await api.setRemoteName(id, name);
  } catch (_) {
    toast('Could not save the name', 'error');
    return;
  }
  renderRecents();
}

export function renderRecents() {
  const config = S.config;
  const recent = (config && config.recentIds) || [];
  const names = (config && config.remoteNames) || {};
  // Recent IDs first, then named IDs that have dropped off the recents, so a
  // machine the user named stays one click away.
  const ids = recent.concat(Object.keys(names).filter((id) => !recent.includes(id)));

  const dl = $('recent-ids');
  dl.innerHTML = '';
  for (const id of ids) {
    const o = document.createElement('option');
    o.value = id;
    if (nameOf(id)) o.label = nameOf(id);
    dl.appendChild(o);
  }

  const row = $('recents-row');
  row.innerHTML = '';
  if (!ids.length) return;

  const label = document.createElement('span');
  label.className = 'recents-label';
  label.textContent = 'Recent:';
  row.appendChild(label);

  for (const id of ids) {
    const chip = document.createElement('button');
    chip.type = 'button';
    const name = nameOf(id);
    chip.className = 'recent-chip' + (name ? ' named' : '');
    chip.textContent = name || shortId(id);
    chip.title = name ? `${name}\n${id}` : id;
    chip.addEventListener('click', () => {
      const input = $('remote-id');
      input.value = id;
      input.dispatchEvent(new Event('input'));
      input.focus();
    });
    row.appendChild(chip);
  }

  // "Clear" empties the recents only; named IDs are kept until un-named.
  if (!recent.length) return;
  const clear = document.createElement('button');
  clear.type = 'button';
  clear.className = 'recent-clear';
  clear.textContent = 'Clear';
  clear.title = 'Clear recent IDs (named IDs are kept)';
  clear.addEventListener('click', openClearRecents);
  row.appendChild(clear);
}

// ---- clear-recents confirmation --------------------------------------------
// Clearing can't be undone and, when signed in, syncs to the other computers,
// so it asks first. Cancel is focused, so Enter or Escape never clears by accident.
function openClearRecents() {
  $('modal-clear-recents').classList.remove('hidden');
  $('btn-clear-recents-cancel').focus();
}

function closeClearRecents() {
  $('modal-clear-recents').classList.add('hidden');
}

async function confirmClearRecents() {
  closeClearRecents();
  try {
    S.config.recentIds = await api.clearRecents();
  } catch (_) {
    S.config.recentIds = [];
  }
  renderRecents();
}

// ---- the remote ID and its name -----------------------------------------------
export function syncNameField() {
  const v = $('remote-id').value.trim();
  const field = $('remote-name');
  const id = validUuid(v) ? v : null;
  field.disabled = !id;
  if (id !== S.nameFieldFor) {
    S.nameFieldFor = id;
    field.value = id ? nameOf(id) : '';
  }
}

async function connect() {
  const id = $('remote-id').value.trim();
  if (!validUuid(id)) return;
  // A name typed but not yet committed (no blur) is kept too.
  if (S.nameFieldFor === id) await saveRemoteName(id, $('remote-name').value);
  // Input monitoring only matters for the optional shortcut capture, so a plain
  // connection is never held up by it. When capture is on, sort the grant out
  // once before connecting; the user presses Connect again.
  if (S.config && S.config.captureShortcuts && !S.viewerPreflightDone) {
    S.viewerPreflightDone = true;
    const intro =
      'Shortcut capture is on in Settings; it needs this permission to forward shortcuts to the other computer.';
    if (await preflight(['inputMonitoring'], intro)) return;
  }
  // A tab for it (or the open one), connected: its progress, password
  // prompt and any refusal show in the tab (tabs.js). The engine owns the
  // wait for the host's answer (contract §3.3).
  await startConnecting((confirmed) => api.tabOpen(id, confirmed));
  $('remote-id').value = '';
  $('remote-id').dispatchEvent(new Event('input'));
}

export function wireConnect() {
  $('btn-clear-recents-cancel').addEventListener('click', closeClearRecents);
  $('btn-clear-recents-ok').addEventListener('click', confirmClearRecents);
  $('modal-clear-recents').addEventListener('click', (e) => {
    if (e.target === e.currentTarget) closeClearRecents(); // backdrop click
  });
  $('modal-clear-recents').addEventListener('keydown', (e) => {
    if (e.key === 'Escape') closeClearRecents();
  });

  $('btn-copy').addEventListener('click', async () => {
    try {
      await navigator.clipboard.writeText(S.config.uuid);
      toast('ID copied');
    } catch (_) {
      toast('Could not copy', 'error');
    }
  });

  $('remote-id').addEventListener('input', () => {
    const v = $('remote-id').value.trim();
    $('connect-hint').textContent = v && !validUuid(v) ? 'That does not look like a valid ID.' : '';
    syncNameField();
    render();
  });

  // Saved when the field loses focus or on Enter; clearing it removes the name.
  $('remote-name').addEventListener('change', () => {
    if (S.nameFieldFor) saveRemoteName(S.nameFieldFor, $('remote-name').value);
  });
  $('remote-name').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      $('remote-name').blur();
    }
  });

  $('btn-connect').addEventListener('click', connect);
  // Enter in the ID field connects, as the button does.
  $('remote-id').addEventListener('keydown', (e) => {
    if (e.key === 'Enter' && !$('btn-connect').disabled) {
      e.preventDefault();
      connect();
    }
  });
}

