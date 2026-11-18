// The UI's one state machine (Plan 04 §7, mirroring the original Electron
// renderer) and the small view helpers every module shares.
//
// Shared mutable state lives on the exported object `S`: an ES-module import
// is a read-only binding, so modules that must WRITE shared state (the screen,
// the loaded config, the peer awaiting an answer) do it through `S.<field>`.

import * as api from './api.js';


export const $ = (id) => document.getElementById(id);

// §6 — the computer's state: offline, ready, asked to be viewed, or being
// viewed. What this computer views is the tabs' business (S.tabs, the last
// `tabs-changed` snapshot, plans/08 P4): a tab on screen shows the session
// screen, none the connect page.
const STATES = ['OFFLINE', 'READY', 'INCOMING', 'HOST_ACTIVE'];

export const S = {
  state: 'OFFLINE',
  config: null,
  incomingFrom: null, // peer awaiting our approve/deny
  // The tab strip as the app last described it (src/tabs.rs snapshot).
  tabs: { tabs: [], detached: [], active: null, connected: 0, hosting: false, signedOut: false },
  cancelCountdown: null,
  // The ID the name field currently belongs to, so typing a name is not
  // overwritten until the ID itself changes.
  nameFieldFor: null,
  // Each automatic permission prompt is shown at most once per run: a user who
  // closed the modal on purpose should not be nagged on every later connection.
  hostPreflightDone: false,
  viewerPreflightDone: false
};

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
export const validUuid = (s) => typeof s === 'string' && UUID_RE.test(s);

// A detached tab window (plans/09) loads this page as `index.html?window=<remote>`:
// it shows that one tab, with no strip and no home page.
export const windowRemote = new URLSearchParams(location.search).get('window');

/** The tab this page shows: its window's, or the main window's tab on screen. */
export const activeTab = () => {
  if (windowRemote) {
    return (S.tabs.detached || []).find((t) => t.remoteId === windowRemote) || null;
  }
  return (S.tabs.active && S.tabs.tabs.find((t) => t.remoteId === S.tabs.active)) || null;
};

/** The remote of the tab this page shows, if any. */
export const currentRemote = () => (activeTab() || {}).remoteId || null;

/** Something is connected: this computer is viewed, or views another. */
export const sessionLive = () => S.state === 'HOST_ACTIVE' || S.tabs.connected > 0;

export function setState(next) {
  if (!STATES.includes(next)) throw new Error('unknown state ' + next);
  S.state = next;
  render();
}

export function render() {
  const state = S.state;
  // A detached window never shows the host screen: that is the main
  // window's.
  const hosting = state === 'HOST_ACTIVE' && !windowRemote;
  const tab = hosting ? null : activeTab();

  $('screen-home').classList.toggle('hidden', hosting || !!tab);
  $('screen-host').classList.toggle('hidden', !hosting);
  $('screen-view').classList.toggle('hidden', !tab);
  // Asked in the main window only.
  $('modal-incoming').classList.toggle('hidden', state !== 'INCOMING' || !!windowRemote);

  // The "connection lost" panel belongs to a session in trouble: being
  // viewed (shown by the host events), or the tab on screen.
  if (!hosting && !(tab && tab.troubleSecs != null)) {
    $('modal-link').classList.add('hidden');
  }

  // Connecting is possible from a registered app that is not being viewed
  // (D1); other tabs may be connected.
  // Nor while the selected account is signed out (its computers are hidden).
  const locked = !!(S.config && S.config.account && S.config.account.state === 'locked');
  $('btn-connect').disabled = state !== 'READY' || locked || !validUuid($('remote-id').value.trim());
}

// ---- status / banner / toasts ----------------------------------------------
export function setStatus(color, text) {
  $('status-dot').className = 'dot dot-' + color;
  $('status-text').textContent = text;
}

export function setBanner(text) {
  const el = $('banner');
  if (!text) {
    el.classList.add('hidden');
    el.textContent = '';
  } else {
    el.textContent = text;
    el.classList.remove('hidden');
  }
}

export function toast(msg, kind) {
  const el = document.createElement('div');
  el.className = 'toast' + (kind === 'error' ? ' toast-error' : '');
  el.textContent = msg;
  $('toasts').appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

/** The message of a rejected command, for a toast or a status line. */
export const errorText = (err) => String((err && err.message) || err);

// ---- countdown helper (the incoming-request prompt) --------------------------
export function countdown(elId, seconds, onDone) {
  let left = seconds;
  $(elId).textContent = String(left);
  const iv = setInterval(() => {
    left -= 1;
    $(elId).textContent = String(Math.max(0, left));
    if (left <= 0) {
      clearInterval(iv);
      if (onDone) onDone();
    }
  }, 1000);
  return () => clearInterval(iv);
}

export function stopCountdown() {
  if (S.cancelCountdown) S.cancelCountdown();
  S.cancelCountdown = null;
}


// ---- web dialogs over the native video (08a §6.18) ------------------------------
// The native video sits above this page, so a dialog would be painted under
// it. While any dialog opened through here is up the app hides every video;
// when the last one closes it shows the tab on screen again. Always cleared
// when the last closes, whatever happened to the sessions meanwhile — the
// app-wide flag must never be left set.
const overlays = new Set();

/** Open dialog `id` over the video: the video goes first, then the dialog. */
export async function overlayOpen(id) {
  overlays.add(id);
  try {
    await api.setVideoVisible(false);
  } catch (_) {
    /* the dialog is still better than nothing */
  }
  if (overlays.has(id)) $(id).classList.remove('hidden');
}

/** Close dialog `id`; the last one closed gives the video back. */
export function overlayClose(id) {
  $(id).classList.add('hidden');
  if (!overlays.delete(id) || overlays.size > 0) return;
  api.setVideoVisible(true).catch(() => {});
}

/** A fresh page has no dialog open (it may have reloaded with one). */
export function overlaysReset() {
  overlays.clear();
  api.setVideoVisible(true).catch(() => {});
}
