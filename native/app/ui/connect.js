// The home screen's connect side: the remote ID and its name, the recents, the
// "clear recents" confirmation, the copy-my-ID button, and the viewer's
// requesting / password-prompt modals.

import * as api from './api.js';
import { scheduleSync } from './accounts.js';
import { preflight } from './settings.js';
import {
  $,
  S,
  errorText,
  render,
  setState,
  startRequestCountdown,
  stopCountdown,
  toast,
  validUuid
} from './state.js';

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
  scheduleSync();
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
  scheduleSync();
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
  $('requesting-peer').textContent = peerLabel(id);
  setState('REQUESTING');
  // Viewer owns the 30 s timeout (contract §3.3).
  startRequestCountdown();
  try {
    await api.connectTo(id);
  } catch (err) {
    // Refused before anything was sent (someone is connected or connecting to
    // this computer): back to the home screen with the reason.
    stopCountdown();
    setState('READY');
    toast(errorText(err), 'error');
    return;
  }
  scheduleSync(3000); // connect_to added it to the recents
}

// ---- password prompt (viewer) ---------------------------------------------------
// Cancel ends the pending request in the engine too: until then this computer
// counts as connecting and answers incoming requests "busy" (D1).
function cancelPassword() {
  S.passwordFrom = null;
  $('password-input').value = '';
  api.endSession();
  setState('READY');
}

function submitPassword() {
  const pw = $('password-input').value;
  if (!S.passwordFrom) return;
  api.submitPassword(S.passwordFrom, pw, $('password-remember').checked);
  $('password-input').value = '';
  setState('REQUESTING');
  startRequestCountdown();
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

  $('btn-cancel-request').addEventListener('click', () => {
    stopCountdown();
    api.endSession();
    setState('READY');
  });

  $('btn-password-cancel').addEventListener('click', cancelPassword);
  $('btn-password-ok').addEventListener('click', submitPassword);
  // Enter submits, Escape cancels — no reaching for the mouse.
  $('password-input').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      submitPassword();
    } else if (e.key === 'Escape') {
      e.preventDefault();
      cancelPassword();
    }
  });

  api.on('password-required', (e) => {
    S.passwordFrom = e.payload.from;
    stopCountdown();
    $('password-input').value = '';
    // Default the checkbox to whatever auto-login is currently set to, so ticking
    // it once in Settings makes every later prompt remember by default.
    $('password-remember').checked = !!(S.config && S.config.autoLogin);
    setState('PASSWORD_PROMPT');
    $('password-input').focus();
  });
}

