// Optional account that keeps remote IDs, their names and the recents the same
// on every computer signed in to it. Rust does the HTTP and the merging
// (account.rs, protocol::sync); this only decides when to sync and shows it.

import * as api from './api.js';
import { nameOf, reloadConfig, renderRecents } from './connect.js';
import { $, S, errorText, toast } from './state.js';

const SYNC_EVERY_MS = 5 * 60 * 1000;
const SYNC_ON_FOCUS_AFTER_MS = 60 * 1000;
let syncing = null; // the sync in flight, if any
let syncAgain = false; // something changed while it was in flight
let syncTimer = null;
let lastSyncAttempt = 0;

const signedIn = () => !!(S.config && S.config.account);

// Redraw from a sync / sign-in / sign-out answer.
function applyAccountResult(r) {
  S.config.account = r.account;
  S.config.recentIds = r.recentIds;
  S.config.remoteNames = r.remoteNames;
  renderRecents();
  // A name synced from elsewhere shows at once, unless the user is typing one.
  if (S.nameFieldFor && document.activeElement !== $('remote-name')) {
    $('remote-name').value = nameOf(S.nameFieldFor);
  }
  renderAccount();
}

export async function syncNow(manual) {
  if (!signedIn()) return;
  if (syncing) {
    syncAgain = true;
    return syncing;
  }
  syncing = (async () => {
    try {
      applyAccountResult(await api.accountSync());
      if (manual) setAccountStatus('Synced.');
    } catch (err) {
      const msg = errorText(err);
      const wasSignedIn = signedIn();
      await reloadConfig();
      renderAccount();
      // Background failures (offline, server down) stay quiet and retry
      // later; being signed out, or a sync the user asked for, is reported.
      if (manual) setAccountStatus(msg, true);
      else if (wasSignedIn && !signedIn()) toast(msg, 'error');
    } finally {
      lastSyncAttempt = Date.now();
      syncing = null;
      if (syncAgain) {
        syncAgain = false;
        scheduleSync();
      }
    }
  })();
  return syncing;
}

// Sync shortly after a local change, coalescing a burst of them.
export function scheduleSync(delayMs) {
  if (!signedIn()) return;
  clearTimeout(syncTimer);
  syncTimer = setTimeout(() => syncNow(false), delayMs === undefined ? 1500 : delayMs);
}

function ago(ms) {
  if (!ms) return 'not yet';
  const s = Math.round((Date.now() - ms) / 1000);
  if (s < 45) return 'just now';
  const m = Math.round(s / 60);
  if (m < 60) return `${m} min ago`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h} h ago`;
  return new Date(ms).toLocaleString();
}

export function setAccountStatus(text, error) {
  const el = $('account-status');
  el.textContent = text || '';
  el.classList.toggle('hidden', !text);
  el.classList.toggle('hint-error', !!error);
}

export function renderAccount() {
  const a = S.config && S.config.account;
  $('account-signed-out').classList.toggle('hidden', !!a);
  $('account-signed-in').classList.toggle('hidden', !a);
  if (a) {
    $('account-name').textContent = a.username;
    $('account-last-sync').textContent = ago(a.lastSync);
  }
}

async function accountSignIn(create) {
  const username = $('account-username').value.trim();
  const password = $('account-password').value;
  if (!username || !password) {
    setAccountStatus('Enter a username and a password.', true);
    return;
  }
  const buttons = [$('btn-account-sign-in'), $('btn-account-create')];
  buttons.forEach((b) => (b.disabled = true));
  setAccountStatus(create ? 'Creating the account…' : 'Signing in…');
  try {
    const r = await api.accountSignIn(username, password, create);
    $('account-password').value = '';
    applyAccountResult(r);
    lastSyncAttempt = Date.now();
    if (r.syncError) {
      setAccountStatus('Signed in, but the first sync failed: ' + r.syncError, true);
    } else {
      setAccountStatus(
        create ? 'Account created. This computer\'s IDs are saved to it.' : 'Signed in and synced.'
      );
    }
  } catch (err) {
    setAccountStatus(errorText(err), true);
  } finally {
    buttons.forEach((b) => (b.disabled = false));
  }
}

async function accountSignOut() {
  clearTimeout(syncTimer);
  try {
    applyAccountResult(await api.accountSignOut());
    setAccountStatus('Signed out. The IDs stay on this computer.');
  } catch (err) {
    setAccountStatus(errorText(err), true);
  }
}

export function wireAccounts() {
  $('btn-account-sign-in').addEventListener('click', () => accountSignIn(false));
  $('btn-account-create').addEventListener('click', () => accountSignIn(true));
  $('btn-account-sign-out').addEventListener('click', accountSignOut);
  $('btn-account-sync').addEventListener('click', async () => {
    $('btn-account-sync').disabled = true;
    setAccountStatus('Syncing…');
    await syncNow(true);
    $('btn-account-sync').disabled = false;
  });
  $('account-password').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      accountSignIn(false);
    }
  });

  // Pick up changes made on other computers: when the window comes back to the
  // front, and every few minutes.
  window.addEventListener('focus', () => {
    if (Date.now() - lastSyncAttempt > SYNC_ON_FOCUS_AFTER_MS) syncNow(false);
  });
  setInterval(() => syncNow(false), SYNC_EVERY_MS);
}
