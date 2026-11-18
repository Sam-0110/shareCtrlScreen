// Accounts (plans/08 P1): the switcher, signing in / up / out, the locked form
// of a signed-out account, "bring this computer's tabs and names in?", and
// the account status line. Rust does everything that matters (src/accounts.rs:
// HTTP, when to sync; the engine: the data, the merge, the tokens); this page
// shows the state Rust reports in `account-state` and sends the user's
// choices back.
//
// Account states: `local` (no account), `signedIn`, `locked` (selected but
// signed out: only its sign-in form is shown, none of its data), `resolving`
// (just signed in; waiting for the bring-in answer).

import * as api from './api.js';
import { nameOf, peerLabel, reloadConfig } from './connect.js';
import { $, S, errorText, render, toast } from './state.js';

const LOCAL = 'local';

const view = () => (S.config && S.config.account) || { state: LOCAL, accounts: [] };

/** Whether this computer is viewing (or connecting to) another one: a
 *  switch or sign-out disconnects every tab (D6). */
function viewerBusy() {
  return S.tabs.tabs.some((t) => t.phase !== 'disconnected');
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
  const v = view();
  const signedIn = v.state === 'signedIn' || v.state === 'resolving';
  const locked = v.state === 'locked';

  // The switcher: this computer alone, then each account used on it.
  const select = $('account-switch');
  select.textContent = '';
  const local = document.createElement('option');
  local.value = LOCAL;
  local.textContent = 'This computer only (no account)';
  select.appendChild(local);
  for (const a of v.accounts || []) {
    const o = document.createElement('option');
    o.value = a.key;
    o.textContent = a.username;
    o.title = a.server;
    select.appendChild(o);
  }
  select.value = (v.active && v.active.key) || LOCAL;
  $('account-switch-row').classList.toggle('hidden', !(v.accounts && v.accounts.length));

  $('account-signed-out').classList.toggle('hidden', signedIn);
  $('account-signed-in').classList.toggle('hidden', !signedIn);
  $('account-intro-local').classList.toggle('hidden', locked);
  $('account-intro-locked').classList.toggle('hidden', !locked);
  $('btn-account-create').classList.toggle('hidden', locked);
  $('btn-account-use-local').classList.toggle('hidden', !locked);
  $('btn-account-remove-locked').classList.toggle('hidden', !locked);
  // A signed-out account's settings cannot change until it is signed in
  // again; they still apply, so they are shown, read-only.
  for (const id of ['set-clipboard-sync', 'set-paste-dropped', 'set-translate-modifiers']) {
    $(id).disabled = locked;
    $(id).title = locked ? 'Sign in to the account to change this' : '';
  }
  const user = $('account-username');
  user.readOnly = locked;
  if (locked && v.active) {
    user.value = v.active.username;
    $('account-locked-name').textContent = v.active.username;
  } else if (!locked && user.readOnly === false && signedIn) {
    user.value = '';
  }
  if (signedIn && v.active) {
    $('account-name').textContent = v.active.username;
    $('account-last-sync').textContent = ago(v.active.lastSync);
  }

  // Home screen: a signed-out account's computers are not shown (I13).
  $('account-locked-note').classList.toggle('hidden', !locked);
  if (locked && v.active) $('account-locked-note-name').textContent = v.active.username;
}

/** Show a new account state from Rust. */
function applyAccountState(v) {
  if (!S.config) return;
  S.config.account = v;
  renderAccount();
  if (v.error && v.state === 'signedIn') setAccountStatus(v.error, true);
  // The bring-in question is part of the state, so it also comes back after
  // the page reloads.
  if (v.bringIn && $('modal-bring-in').classList.contains('hidden')) showBringIn(v.bringIn);
  else if (!v.bringIn) $('modal-bring-in').classList.add('hidden');
  render();
}

/** Reload what the active workspace holds (names, recents, settings). */
async function reloadWorkspace() {
  await reloadConfig();
  // A name synced from elsewhere shows at once, unless the user is typing one.
  if (S.nameFieldFor && document.activeElement !== $('remote-name')) {
    $('remote-name').value = nameOf(S.nameFieldFor);
  }
  renderAccount();
  render();
}

// ---- confirm an account change that ends the session (D6) --------------------
let confirmResolve = null;

/**
 * Ask before an account change. `detail` explains it; when this computer is
 * viewing another one, the session it ends is named too. Resolves to whether
 * the user went ahead. Nothing to say → no question.
 */
function confirmAccountChange(title, detail) {
  const ending = viewerBusy();
  if (!ending && !detail) return Promise.resolve(true);
  $('account-confirm-title').textContent = title;
  $('account-confirm-detail').textContent = detail || '';
  $('account-confirm-detail').classList.toggle('hidden', !detail);
  $('account-confirm-session').classList.toggle('hidden', !ending);
  $('account-confirm-peer').textContent = $('view-peer').textContent || peerLabel('');
  $('modal-account-confirm').classList.remove('hidden');
  $('btn-account-confirm-cancel').focus();
  return new Promise((resolve) => {
    confirmResolve = resolve;
  });
}

function answerConfirm(ok) {
  $('modal-account-confirm').classList.add('hidden');
  const resolve = confirmResolve;
  confirmResolve = null;
  if (resolve) resolve(ok);
}

// ---- actions ------------------------------------------------------------------
async function run(busyText, call, doneText) {
  setAccountStatus(busyText);
  try {
    const v = await call();
    applyAccountState(v);
    await reloadWorkspace();
    if (v && v.syncError) setAccountStatus('Signed in, but the first sync failed: ' + v.syncError, true);
    else if (v && v.warning) setAccountStatus(v.warning, true);
    else if (v && v.state === 'resolving') setAccountStatus('');
    else setAccountStatus(doneText || '');
    return true;
  } catch (err) {
    setAccountStatus(errorText(err), true);
    return false;
  }
}

async function accountSignIn(create) {
  const v = view();
  const locked = v.state === 'locked';
  const username = $('account-username').value.trim();
  const password = $('account-password').value;
  if (!username || !password) {
    setAccountStatus('Enter a username and a password.', true);
    return;
  }
  const keep = $('account-keep').checked;
  // Signing in to another account from a signed-in one is a switch (D6).
  if (v.state === 'signedIn' && !(await confirmAccountChange('Sign in to ' + username + '?'))) {
    return;
  }
  const buttons = [$('btn-account-sign-in'), $('btn-account-create')];
  buttons.forEach((b) => (b.disabled = true));
  const ok = await run(
    create ? 'Creating the account…' : 'Signing in…',
    () =>
      create
        ? api.accountSignUp(username, password, keep)
        : api.accountSignIn(username, password, keep, locked && v.active ? v.active.key : null),
    create ? 'Account created. This computer\'s IDs and names are in it now.' : 'Signed in and synced.'
  );
  if (ok) $('account-password').value = '';
  buttons.forEach((b) => (b.disabled = false));
}

async function accountSignOut() {
  const v = view();
  const who = (v.active && v.active.username) || 'this account';
  if (!(await confirmAccountChange('Sign out of ' + who + '?'))) return;
  await run('Signing out…', () => api.accountSignOut(), 'Signed out. ' + who + "'s data stays on this computer, locked.");
}

async function accountRemove() {
  const v = view();
  if (!v.active) return;
  const who = v.active.username;
  const ok = await confirmAccountChange(
    'Remove ' + who + ' from this computer?',
    "Its names, recents and saved passwords on this computer are deleted. The account itself stays; sign in again to get its names back."
  );
  if (!ok) return;
  await run('Removing…', () => api.accountRemove(v.active.key), who + ' was removed from this computer.');
}

async function accountSwitch(target) {
  const v = view();
  const current = (v.active && v.active.key) || LOCAL;
  if (target === current) return;
  const label =
    target === LOCAL
      ? 'this computer only'
      : ((v.accounts || []).find((a) => a.key === target) || { username: target }).username;
  if (!(await confirmAccountChange('Switch to ' + label + '?'))) {
    renderAccount(); // put the switcher back
    return;
  }
  await run('Switching…', () => api.accountSwitch(target), '');
}

async function answerBringIn(choice) {
  $('modal-bring-in').classList.add('hidden');
  await run('Signing in…', () => api.accountResolveLocal(choice), 'Signed in and synced.');
}

function showBringIn(p) {
  $('bring-in-account').textContent = p.account;
  const parts = [];
  if (p.tabs) parts.push(p.tabs + ' open tab' + (p.tabs === 1 ? '' : 's'));
  if (p.names) parts.push(p.names + ' named computer' + (p.names === 1 ? '' : 's'));
  $('bring-in-detail').textContent = parts.length
    ? 'This computer has ' + parts.join(' and ') + ', and its recent connections.'
    : 'This computer has recent connections of its own.';
  $('modal-bring-in').classList.remove('hidden');
  $('btn-bring-in-add').focus();
}

async function syncNowButton() {
  $('btn-account-sync').disabled = true;
  await run('Syncing…', () => api.accountSync(), 'Synced.');
  $('btn-account-sync').disabled = false;
}

export function wireAccounts() {
  $('btn-account-sign-in').addEventListener('click', () => accountSignIn(false));
  $('btn-account-create').addEventListener('click', () => accountSignIn(true));
  $('btn-account-sign-out').addEventListener('click', accountSignOut);
  $('btn-account-remove').addEventListener('click', accountRemove);
  $('btn-account-remove-locked').addEventListener('click', accountRemove);
  $('btn-account-use-local').addEventListener('click', () => accountSwitch(LOCAL));
  $('btn-account-sync').addEventListener('click', syncNowButton);
  $('account-switch').addEventListener('change', (e) => accountSwitch(e.target.value));
  $('account-password').addEventListener('keydown', (e) => {
    if (e.key === 'Enter') {
      e.preventDefault();
      accountSignIn(false);
    }
  });

  $('btn-account-confirm-cancel').addEventListener('click', () => answerConfirm(false));
  $('btn-account-confirm-ok').addEventListener('click', () => answerConfirm(true));
  $('modal-account-confirm').addEventListener('keydown', (e) => {
    if (e.key === 'Escape') answerConfirm(false);
  });

  // Closing the question any way but "Add" keeps the data separate.
  $('btn-bring-in-add').addEventListener('click', () => answerBringIn('add'));
  $('btn-bring-in-separate').addEventListener('click', () => answerBringIn('separate'));
  $('modal-bring-in').addEventListener('keydown', (e) => {
    if (e.key === 'Escape') answerBringIn('separate');
  });

  api.on('account-state', (e) => applyAccountState(e.payload));
  // A sync merged changes from another computer, or the account changed.
  api.on('workspace-changed', () => {
    reloadWorkspace().catch((err) => toast(errorText(err), 'error'));
  });

  // Coming back to the window picks up changes made on other computers.
  window.addEventListener('focus', () => {
    api.accountWindowFocused().catch(() => {});
  });
}

/** Show the account state loaded with the config (boot, page reload). */
export function refreshAccount() {
  applyAccountState(view());
}
