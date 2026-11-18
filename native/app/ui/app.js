'use strict';

// UI controller for the Tauri shell (Plan 04 §7). Mirrors the original
// app/renderer/ui.js state machine and markup 1:1 so the look and behaviour are
// unchanged; only the plumbing differs:
//   ipcRenderer.invoke(...) -> window.__TAURI__.core.invoke(...)
//   ipcRenderer.on(...)     -> window.__TAURI__.event.listen(...)
// WebRTC/signaling/capture all live in the Rust engine now.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

// §6 — one state variable; every state maps to exactly one screen/modal.
const STATES = [
  'OFFLINE',
  'READY',
  'REQUESTING',
  'PASSWORD_PROMPT',
  'INCOMING',
  'HOST_ACTIVE',
  'VIEW_ACTIVE'
];

let state = 'OFFLINE';
let config = null;
let incomingFrom = null;   // peer awaiting our approve/deny
let passwordFrom = null;   // host that demanded a password
let cancelCountdown = null;

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const validUuid = (s) => typeof s === 'string' && UUID_RE.test(s);

function setState(next) {
  if (!STATES.includes(next)) throw new Error('unknown state ' + next);
  state = next;
  render();
}

function render() {
  const home =
    state === 'READY' || state === 'OFFLINE' || state === 'REQUESTING' ||
    state === 'PASSWORD_PROMPT' || state === 'INCOMING';

  $('screen-home').classList.toggle('hidden', !home);
  $('screen-host').classList.toggle('hidden', state !== 'HOST_ACTIVE');
  $('screen-view').classList.toggle('hidden', state !== 'VIEW_ACTIVE');

  $('modal-requesting').classList.toggle('hidden', state !== 'REQUESTING');
  $('modal-password').classList.toggle('hidden', state !== 'PASSWORD_PROMPT');
  $('modal-incoming').classList.toggle('hidden', state !== 'INCOMING');

  // The "connection lost" panel only belongs to a live session — never leave it
  // up on the home screen. (Only ever hides; showing it is the engine's call.)
  if (state !== 'VIEW_ACTIVE' && state !== 'HOST_ACTIVE') {
    $('modal-link').classList.add('hidden');
  }

  // Connect is only possible from a fully idle, registered app.
  $('btn-connect').disabled = state !== 'READY' || !validUuid($('remote-id').value.trim());
}

// ---- status / banner / toasts ----------------------------------------------
function setStatus(color, text) {
  $('status-dot').className = 'dot dot-' + color;
  $('status-text').textContent = text;
}

function setBanner(text) {
  const el = $('banner');
  if (!text) {
    el.classList.add('hidden');
    el.textContent = '';
  } else {
    el.textContent = text;
    el.classList.remove('hidden');
  }
}

function toast(msg, kind) {
  const el = document.createElement('div');
  el.className = 'toast' + (kind === 'error' ? ' toast-error' : '');
  el.textContent = msg;
  $('toasts').appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

// ---- countdown helper (shared by REQUESTING and INCOMING) ------------------
function countdown(elId, seconds, onDone) {
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

function stopCountdown() {
  if (cancelCountdown) cancelCountdown();
  cancelCountdown = null;
}

// ---- config / recents ------------------------------------------------------
async function reloadConfig() {
  config = await invoke('get_config');
  $('my-uuid').textContent = config.uuid;
  renderRecents();
  return config;
}

const shortId = (id) => (id.length > 14 ? id.slice(0, 8) + '…' : id);

// Friendly names the user gave remote IDs (config.remoteNames, id -> name).
const nameOf = (id) =>
  (config && config.remoteNames && id && config.remoteNames[id.trim().toLowerCase()]) || '';
// "Office PC (0f8fad5b-…)" when named, else the bare ID.
const peerLabel = (id) => {
  const n = nameOf(id);
  return n ? `${n} (${id})` : id || '';
};

async function saveRemoteName(id, name) {
  if (!validUuid(id) || nameOf(id) === name.trim()) return;
  try {
    config.remoteNames = await invoke('set_remote_name', { id, name });
  } catch (_) {
    toast('Could not save the name', 'error');
    return;
  }
  renderRecents();
  scheduleSync();
}

function renderRecents() {
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
    config.recentIds = await invoke('clear_recents');
  } catch (_) {
    config.recentIds = [];
  }
  renderRecents();
  scheduleSync();
}

// ---- platform wording + displays -------------------------------------------
// The OS facts the UI words itself around (see `get_platform_info`). The HTML
// ships neutral defaults, so a failed call leaves a correct, if generic, page.
async function applyPlatformInfo() {
  let info;
  try {
    info = await invoke('get_platform_info');
  } catch (_) {
    return;
  }
  $('start-at-login-label').textContent = info.startAtLoginLabel;
  $('shortcut-capture-hint').textContent = info.shortcutCaptureHint;
  $('downloads-path').textContent = info.downloadsDir;
  $('login-screen-note').classList.toggle('hidden', info.loginScreenSupported);

  // Keep the first entry ("Primary monitor", empty id) and list the rest.
  const select = $('set-share-display');
  while (select.options.length > 1) select.remove(1);
  for (const d of info.displays) {
    const o = document.createElement('option');
    o.value = d.id;
    o.textContent = `${d.name} (${d.width}×${d.height})${d.primary ? ' – primary' : ''}`;
    select.appendChild(o);
  }
}

// ---- settings modal --------------------------------------------------------
function openSettings() {
  $('set-server').value = config.serverUrl;
  $('set-mode-approve').checked = config.mode === 'approve';
  $('set-mode-password').checked = config.mode === 'password';
  $('set-password').value = '';
  $('set-password').placeholder = config.hasPassword ? '(unchanged)' : '(none set)';
  $('set-password-clear').checked = false;
  $('set-password-perm').value = config.passwordPermission;
  $('set-share-audio').checked = config.shareAudio !== false;
  $('set-share-display').value = config.shareDisplayId || '';
  $('set-auto-login').checked = !!config.autoLogin;
  $('set-clipboard-sync').checked = config.clipboardSync !== false;
  $('set-paste-dropped').checked = config.pasteDroppedFiles !== false;
  $('set-start-with-windows').checked = !!config.startWithWindows;
  $('set-openh264').checked = config.useOpenH264 !== false;
  $('set-translate-modifiers').checked = config.translateModifiers !== false;
  $('openh264-license').classList.add('hidden');
  renderAccount();
  setAccountStatus('');
  $('account-password').value = '';
  refreshOpenH264();
  // Updates panel: reset transient state, show what the last check found, and —
  // if we have not checked yet — fill the current version with a quiet check.
  $('update-status').textContent = '';
  $('update-progress').classList.add('hidden');
  $('update-progress').textContent = '';
  renderUpdate();
  if (!lastUpdate) checkForUpdates(true);
  const saved = config.savedLoginCount || 0;
  const clear = $('set-clear-logins');
  clear.checked = false;
  clear.parentElement.lastChild.textContent =
    saved > 0
      ? ` Forget ${saved} saved password${saved === 1 ? '' : 's'}`
      : ' Forget all saved passwords';
  clear.disabled = saved === 0;
  $('modal-settings').classList.remove('hidden');
}

function closeSettings() {
  $('modal-settings').classList.add('hidden');
  // If settings were opened mid-session (viewer OR host), the native video
  // surface was hidden so the modal could be seen (it sits above the WebView) —
  // bring it back. On the host the surface is absent, so this is a no-op.
  if (sessionLive()) invoke('set_video_visible', { visible: true });
}

async function saveSettings() {
  const patch = {
    serverUrl: $('set-server').value.trim(),
    mode: $('set-mode-password').checked ? 'password' : 'approve',
    passwordPermission: $('set-password-perm').value,
    shareAudio: $('set-share-audio').checked,
    shareDisplayId: $('set-share-display').value || null,
    autoLogin: $('set-auto-login').checked,
    clipboardSync: $('set-clipboard-sync').checked,
    pasteDroppedFiles: $('set-paste-dropped').checked,
    startWithWindows: $('set-start-with-windows').checked,
    useOpenH264: $('set-openh264').checked,
    translateModifiers: $('set-translate-modifiers').checked,
    clearSavedLogins: $('set-clear-logins').checked
  };

  // Only touch the password when the user actually asked to.
  if ($('set-password-clear').checked) {
    patch.password = '';
  } else if ($('set-password').value !== '') {
    patch.password = $('set-password').value;
  }

  try {
    await invoke('save_settings', { patch });
  } catch (err) {
    toast(String((err && err.message) || err), 'error');
    return;
  }

  closeSettings();
  toast('Settings saved');
  await reloadConfig();
}

// ---- account sync ------------------------------------------------------------
// Optional account that keeps remote IDs, their names and the recents the same
// on every computer signed in to it. Rust does the HTTP and the merging
// (account.rs, protocol::sync); this only decides when to sync and shows it.
const SYNC_EVERY_MS = 5 * 60 * 1000;
const SYNC_ON_FOCUS_AFTER_MS = 60 * 1000;
let syncing = null; // the sync in flight, if any
let syncAgain = false; // something changed while it was in flight
let syncTimer = null;
let lastSyncAttempt = 0;

const signedIn = () => !!(config && config.account);

// Redraw from a sync / sign-in / sign-out answer.
function applyAccountResult(r) {
  config.account = r.account;
  config.recentIds = r.recentIds;
  config.remoteNames = r.remoteNames;
  renderRecents();
  // A name synced from elsewhere shows at once, unless the user is typing one.
  if (nameFieldFor && document.activeElement !== $('remote-name')) {
    $('remote-name').value = nameOf(nameFieldFor);
  }
  renderAccount();
}

async function syncNow(manual) {
  if (!signedIn()) return;
  if (syncing) {
    syncAgain = true;
    return syncing;
  }
  syncing = (async () => {
    try {
      applyAccountResult(await invoke('account_sync'));
      if (manual) setAccountStatus('Synced.');
    } catch (err) {
      const msg = String((err && err.message) || err);
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
function scheduleSync(delayMs) {
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

function setAccountStatus(text, error) {
  const el = $('account-status');
  el.textContent = text || '';
  el.classList.toggle('hidden', !text);
  el.classList.toggle('hint-error', !!error);
}

function renderAccount() {
  const a = config && config.account;
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
    const r = await invoke('account_sign_in', { username, password, create });
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
    setAccountStatus(String((err && err.message) || err), true);
  } finally {
    buttons.forEach((b) => (b.disabled = false));
  }
}

async function accountSignOut() {
  clearTimeout(syncTimer);
  try {
    applyAccountResult(await invoke('account_sign_out'));
    setAccountStatus('Signed out. The IDs stay on this computer.');
  } catch (err) {
    setAccountStatus(String((err && err.message) || err), true);
  }
}

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

// ---- permissions modal -----------------------------------------------------
// What each OS permission is for, in words that hold on every OS. Order is the
// order shown.
const PERMISSIONS = [
  {
    key: 'screenCapture',
    title: 'Screen recording',
    desc: 'Lets this computer share its screen with someone who connects.'
  },
  {
    key: 'inputInjection',
    title: 'Remote control',
    desc: 'Lets a connected person move the mouse and type on this computer.'
  },
  {
    key: 'inputMonitoring',
    title: 'Keyboard shortcuts',
    desc: 'Lets shortcuts you press while controlling another computer reach it.'
  }
];

const PERM_POLL_MS = 2000;
let permPoll = null;
// Each automatic prompt is shown at most once per run: a user who closed the
// modal on purpose should not be nagged on every later connection.
let hostPreflightDone = false;
let viewerPreflightDone = false;

const permNeedsGrant = (s) => s === 'denied' || s === 'notDetermined';

function renderPermissions(report) {
  const list = $('perm-list');
  list.innerHTML = '';
  let listed = 0;
  for (const p of PERMISSIONS) {
    const st = report[p.key];
    // This OS has no such permission (e.g. everything on Windows) — say nothing.
    if (st === 'notApplicable') continue;
    listed += 1;

    const row = document.createElement('li');
    row.className = 'perm-row';

    const text = document.createElement('div');
    text.className = 'perm-text';
    const title = document.createElement('div');
    title.className = 'perm-title';
    title.textContent = p.title;
    const desc = document.createElement('div');
    desc.className = 'perm-desc';
    desc.textContent = p.desc;
    text.append(title, desc);

    const badge = document.createElement('span');
    const granted = st === 'granted';
    badge.className = 'badge ' + (granted ? 'badge-granted' : st === 'denied' ? 'badge-denied' : 'badge-pending');
    badge.textContent = granted ? 'GRANTED' : st === 'denied' ? 'DENIED' : 'NOT ASKED';

    row.append(text, badge);
    if (!granted) {
      const grant = document.createElement('button');
      grant.type = 'button';
      grant.className = 'btn btn-sm';
      grant.textContent = 'Grant';
      grant.addEventListener('click', () => grantPermission(p.key));
      row.appendChild(grant);
    }
    list.appendChild(row);
  }
  $('perm-none').classList.toggle('hidden', listed !== 0);

  const notes = $('perm-notes');
  notes.innerHTML = '';
  for (const n of report.notes || []) {
    const li = document.createElement('li');
    li.textContent = n;
    notes.appendChild(li);
  }
}

function stopPermPoll() {
  if (permPoll) clearInterval(permPoll);
  permPoll = null;
}

// Refresh the rows; stop polling once nothing is left to grant, so an idle
// modal does not keep asking the OS.
async function refreshPermissions() {
  let report;
  try {
    report = await invoke('get_permissions');
  } catch (_) {
    return null;
  }
  // The modal may have been closed while the call was in flight.
  if ($('modal-permissions').classList.contains('hidden')) return null;
  renderPermissions(report);
  if (!PERMISSIONS.some((p) => permNeedsGrant(report[p.key]))) stopPermPoll();
  return report;
}

function startPermPoll() {
  stopPermPoll();
  permPoll = setInterval(refreshPermissions, PERM_POLL_MS);
}

async function grantPermission(key) {
  try {
    await invoke('request_permission', { name: key });
  } catch (err) {
    toast(String((err && err.message) || err), 'error');
    return;
  }
  // The user answers in an OS dialog or settings pane; watch for the result.
  startPermPoll();
  refreshPermissions();
}

async function openPermissions(intro) {
  // Same ordering rule as Settings: the native video must be down before the
  // modal paints, or the modal appears underneath it.
  if (state === 'VIEW_ACTIVE') {
    try {
      await invoke('set_video_visible', { visible: false });
    } catch (_) {
      /* fall through — the dialog is still better than nothing */
    }
  }
  let report;
  try {
    report = await invoke('get_permissions');
  } catch (err) {
    toast(String((err && err.message) || err), 'error');
    if (state === 'VIEW_ACTIVE' && $('modal-settings').classList.contains('hidden')) {
      invoke('set_video_visible', { visible: true }).catch(() => {});
    }
    return;
  }
  $('perm-intro').textContent = intro || '';
  renderPermissions(report);
  $('modal-permissions').classList.remove('hidden');
  if (PERMISSIONS.some((p) => permNeedsGrant(report[p.key]))) startPermPoll();
}

function closePermissions() {
  stopPermPoll();
  $('modal-permissions').classList.add('hidden');
  // Settings (if it is what opened us) restores the video when it closes.
  if (state === 'VIEW_ACTIVE' && $('modal-settings').classList.contains('hidden')) {
    invoke('set_video_visible', { visible: true }).catch(() => {});
  }
}

// Show the modal on its own when `keys` still need granting. Returns whether it
// was shown, so a caller can hold back the action the permission is for.
async function preflight(keys, intro) {
  let report;
  try {
    report = await invoke('get_permissions');
  } catch (_) {
    return false;
  }
  if (!keys.some((k) => permNeedsGrant(report[k]))) return false;
  await openPermissions(intro);
  return true;
}

// Settings → Advanced: the Cisco OpenH264 software codec. The notice and the
// licence text come from the backend (Cisco's licence requires both to be
// shown where the user controls the codec).
async function refreshOpenH264() {
  try {
    const info = await invoke('openh264_info');
    $('openh264-notice').textContent = info.notice;
    $('openh264-license').textContent = info.license;
    // The "no H.264 at all" warning leads: it is the one line the user must act on.
    $('openh264-status').textContent = info.problem ? `${info.problem} ${info.status}` : info.status;
    $('btn-openh264-download').disabled = !!info.ready;
  } catch (err) {
    $('openh264-status').textContent = String((err && err.message) || err);
  }
}

$('btn-openh264-download').addEventListener('click', async () => {
  const btn = $('btn-openh264-download');
  if (!$('set-openh264').checked) {
    toast('Turn on "Use Cisco OpenH264" and save first.', 'error');
    return;
  }
  btn.disabled = true;
  $('openh264-status').textContent = 'Downloading from Cisco…';
  try {
    await invoke('openh264_download');
  } catch (err) {
    toast(String((err && err.message) || err), 'error');
  }
  await refreshOpenH264();
});

$('btn-openh264-license').addEventListener('click', () => {
  $('openh264-license').classList.toggle('hidden');
});

// ---- updates (task U1) -----------------------------------------------------
// The flow lives in Rust (updater.rs): `updater_check` reports whether a newer
// release exists; `updater_install` ends any live session, downloads, installs
// and restarts (service-aware on Windows). This panel is the thin surface, plus
// a quiet non-blocking check at startup.
let lastUpdate = null;
let updating = false;

const sessionLive = () => state === 'HOST_ACTIVE' || state === 'VIEW_ACTIVE';

function renderUpdate() {
  if (lastUpdate && lastUpdate.currentVersion) {
    $('update-current-version').textContent = lastUpdate.currentVersion;
  }
  const available = !!(lastUpdate && lastUpdate.available);
  $('update-available').classList.toggle('hidden', !available);
  if (!available) return;
  $('update-new-version').textContent = lastUpdate.version;
  const notes = (lastUpdate.notes || '').trim();
  $('update-notes').textContent = notes;
  $('update-notes').classList.toggle('hidden', notes === '');
  // A .deb/.rpm Linux install cannot self-update (the feed serves an AppImage, so
  // the updater rejects the payload). Offer a manual download instead; every other
  // install keeps the in-app "Update & restart". (task U3)
  const selfUpdatable = lastUpdate.selfUpdatable !== false;
  $('update-selfinstall').classList.toggle('hidden', !selfUpdatable);
  $('update-manual').classList.toggle('hidden', selfUpdatable);
  if (!selfUpdatable) return;
  // Warn (and relabel the button) before the click when a session is live, so
  // the consequence is explicit — updating closes the app and ends the session.
  $('update-session-warning').classList.toggle('hidden', !sessionLive());
  $('btn-update-install').textContent = sessionLive()
    ? 'End session & update'
    : 'Update & restart';
}

async function downloadUpdate() {
  try {
    await invoke('open_releases_page');
  } catch (err) {
    toast(String((err && err.message) || err), 'error');
  }
}

async function checkForUpdates(silent) {
  const statusEl = $('update-status');
  if (!silent) statusEl.textContent = 'Checking…';
  try {
    lastUpdate = await invoke('updater_check');
  } catch (err) {
    lastUpdate = null;
    if (!silent) statusEl.textContent = 'Could not check: ' + String((err && err.message) || err);
    return null;
  }
  renderUpdate();
  if (lastUpdate.available) {
    statusEl.textContent = '';
    if (silent) toast('Update available: ' + lastUpdate.version + ' — open Settings to install');
  } else if (!silent) {
    statusEl.textContent = 'You are up to date.';
  }
  return lastUpdate;
}

async function installUpdate() {
  if (updating || !(lastUpdate && lastUpdate.available)) return;
  updating = true;
  const btn = $('btn-update-install');
  btn.disabled = true;
  const prog = $('update-progress');
  prog.classList.remove('hidden');
  prog.textContent = 'Starting…';
  try {
    // Returns only on macOS/Linux (the app relaunches itself) or on error; on
    // Windows the process has already exited into the installer by now.
    await invoke('updater_install');
    prog.textContent = 'Installed — restarting…';
  } catch (err) {
    updating = false;
    btn.disabled = false;
    prog.classList.add('hidden');
    prog.textContent = '';
    toast(String((err && err.message) || err), 'error');
  }
}

listen('updater-progress', (e) => {
  const prog = $('update-progress');
  const { downloaded, total } = e.payload || {};
  if (total) {
    const pct = Math.min(100, Math.round((downloaded / total) * 100));
    prog.textContent = 'Downloading… ' + pct + '%';
  } else if (downloaded) {
    prog.textContent = 'Downloading… ' + Math.round(downloaded / 1024) + ' KB';
  }
});

// ---- wiring ----------------------------------------------------------------
$('btn-update-check').addEventListener('click', () => checkForUpdates(false));
$('btn-update-install').addEventListener('click', installUpdate);
$('btn-update-download').addEventListener('click', downloadUpdate);
$('btn-settings-permissions').addEventListener('click', () => openPermissions(''));
$('btn-permissions-close').addEventListener('click', closePermissions);
$('btn-settings').addEventListener('click', openSettings);
$('btn-settings-cancel').addEventListener('click', closeSettings);
$('btn-settings-save').addEventListener('click', saveSettings);
$('btn-clear-recents-cancel').addEventListener('click', closeClearRecents);
$('btn-clear-recents-ok').addEventListener('click', confirmClearRecents);
$('modal-clear-recents').addEventListener('click', (e) => {
  if (e.target === e.currentTarget) closeClearRecents(); // backdrop click
});
$('modal-clear-recents').addEventListener('keydown', (e) => {
  if (e.key === 'Escape') closeClearRecents();
});

// Session menu bar (revealed by hovering the top edge of the video).
$('btn-view-refresh').addEventListener('click', () => {
  invoke('request_refresh');
  toast('Requested a fresh frame');
});
// Opening Settings mid-session (viewer or host): take the native video down
// BEFORE painting the modal — it sits above the web UI, so opening the dialog
// first means it appears underneath the video (and, if the hover bar was open,
// sliced to that strip). On the host there is no video surface, so the invoke
// is a harmless no-op. closeSettings() restores it.
async function openSettingsOverVideo() {
  try {
    await invoke('set_video_visible', { visible: false });
  } catch (_) {
    /* fall through — the dialog is still better than nothing */
  }
  openSettings();
}
$('btn-view-settings').addEventListener('click', openSettingsOverVideo);
$('btn-host-settings').addEventListener('click', openSettingsOverVideo);

// Native menu (macOS + Linux; task M15). The menu bar replaces the top-edge
// drawer there — Settings/Disconnect/Check-for-updates are wired to the SAME
// flows the toolbar buttons use. (Disconnect is handled in Rust straight on the
// engine; the role-changed event then updates the UI, exactly like the
// end-session buttons.) No menu exists on Windows, so these never fire there.
listen('menu://settings', () => {
  openSettingsOverVideo();
});
listen('menu://check-updates', () => {
  openSettingsOverVideo().then(() => {
    checkForUpdates(false);
    // Bring the Updates panel into view inside the settings modal.
    const el = $('btn-update-check');
    if (el) el.scrollIntoView({ block: 'center' });
  });
});

$('btn-copy').addEventListener('click', async () => {
  try {
    await navigator.clipboard.writeText(config.uuid);
    toast('ID copied');
  } catch (_) {
    toast('Could not copy', 'error');
  }
});

// The ID the name field currently belongs to, so typing a name is not
// overwritten until the ID itself changes.
let nameFieldFor = null;
function syncNameField() {
  const v = $('remote-id').value.trim();
  const field = $('remote-name');
  const id = validUuid(v) ? v : null;
  field.disabled = !id;
  if (id !== nameFieldFor) {
    nameFieldFor = id;
    field.value = id ? nameOf(id) : '';
  }
}

$('remote-id').addEventListener('input', () => {
  const v = $('remote-id').value.trim();
  $('connect-hint').textContent = v && !validUuid(v) ? 'That does not look like a valid ID.' : '';
  syncNameField();
  render();
});

// Saved when the field loses focus or on Enter; clearing it removes the name.
$('remote-name').addEventListener('change', () => {
  if (nameFieldFor) saveRemoteName(nameFieldFor, $('remote-name').value);
});
$('remote-name').addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault();
    $('remote-name').blur();
  }
});

$('btn-connect').addEventListener('click', async () => {
  const id = $('remote-id').value.trim();
  if (!validUuid(id)) return;
  // A name typed but not yet committed (no blur) is kept too.
  if (nameFieldFor === id) await saveRemoteName(id, $('remote-name').value);
  // Input monitoring only matters for the optional shortcut capture, so a plain
  // connection is never held up by it. When capture is on, sort the grant out
  // once before connecting; the user presses Connect again.
  if (config && config.captureShortcuts && !viewerPreflightDone) {
    viewerPreflightDone = true;
    const intro =
      'Shortcut capture is on in Settings; it needs this permission to forward shortcuts to the other computer.';
    if (await preflight(['inputMonitoring'], intro)) return;
  }
  $('requesting-peer').textContent = peerLabel(id);
  setState('REQUESTING');
  stopCountdown();
  // Viewer owns the 30 s timeout (contract §3.3).
  cancelCountdown = countdown('requesting-count', 30, () => {
    toast('No answer — timed out', 'error');
    setState('READY');
  });
  invoke('connect_to', { id });
  scheduleSync(3000); // connect_to added it to the recents
});

$('btn-cancel-request').addEventListener('click', () => {
  stopCountdown();
  invoke('end_session');
  setState('READY');
});

// password prompt (viewer)
function cancelPassword() {
  passwordFrom = null;
  $('password-input').value = '';
  setState('READY');
}
function submitPassword() {
  const pw = $('password-input').value;
  if (!passwordFrom) return;
  invoke('submit_password', {
    host: passwordFrom,
    password: pw,
    remember: $('password-remember').checked
  });
  $('password-input').value = '';
  setState('REQUESTING');
  stopCountdown();
  cancelCountdown = countdown('requesting-count', 30, () => {
    toast('No answer — timed out', 'error');
    setState('READY');
  });
}
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

// incoming approval (host)
function answerIncoming(decision) {
  stopCountdown();
  if (incomingFrom) invoke('approve', { from: incomingFrom, decision });
  incomingFrom = null;
  if (decision === 'deny') setState('READY');
}
$('btn-incoming-deny').addEventListener('click', () => answerIncoming('deny'));
$('btn-incoming-view').addEventListener('click', () => answerIncoming('view'));
$('btn-incoming-control').addEventListener('click', () => answerIncoming('control'));

// host session controls
$('host-perm').addEventListener('change', () => {
  invoke('set_permission', { value: $('host-perm').value });
});
$('btn-host-end').addEventListener('click', () => invoke('end_session'));
$('btn-view-end').addEventListener('click', () => invoke('end_session'));
// Don't wait out the grace period — give up now.
$('btn-link-end').addEventListener('click', () => {
  $('modal-link').classList.add('hidden');
  invoke('end_session');
});

// ---- engine events ---------------------------------------------------------
listen('server-status', (e) => {
  const s = e.payload.status;
  if (s === 'connected') {
    setStatus('green', 'server: connected');
    setBanner('');
    if (state === 'OFFLINE') setState('READY');
  } else if (s === 'reconnecting') {
    // A live session keeps running on its own WebRTC leg — yellow, not offline.
    if (state === 'HOST_ACTIVE' || state === 'VIEW_ACTIVE') {
      setStatus('yellow', 'server: reconnecting (session active)');
    } else {
      setStatus('red', 'server: disconnected (retrying)');
      if (state !== 'OFFLINE') setState('OFFLINE');
    }
  }
});

listen('approval-request', (e) => {
  incomingFrom = e.payload.from;
  $('incoming-peer').textContent = peerLabel(incomingFrom);
  setState('INCOMING');
  stopCountdown();
  // The Rust decider auto-denies at 30 s too; this just mirrors the countdown.
  cancelCountdown = countdown('incoming-count', 30, () => {
    incomingFrom = null;
    setState('READY');
  });
});

listen('password-required', (e) => {
  passwordFrom = e.payload.from;
  stopCountdown();
  $('password-input').value = '';
  // Default the checkbox to whatever auto-login is currently set to, so ticking
  // it once in Settings makes every later prompt remember by default.
  $('password-remember').checked = !!(config && config.autoLogin);
  setState('PASSWORD_PROMPT');
  $('password-input').focus();
});

listen('role-changed', (e) => {
  const p = e.payload;
  stopCountdown();
  // Whatever happens next (reconnected, or dropped back to the home screen),
  // the "connection lost" panel is stale.
  $('modal-link').classList.add('hidden');
  if (p.role !== 'idle') scheduleSync(); // the peer is now a recent
  if (p.role === 'host') {
    $('host-peer').textContent = peerLabel(p.peer);
    $('host-perm').value = p.permission;
    setState('HOST_ACTIVE');
  } else if (p.role === 'viewer') {
    $('view-peer').textContent = peerLabel(p.peer);
    const control = p.permission === 'control';
    const badge = $('view-badge');
    badge.textContent = control ? 'CONTROL' : 'VIEW ONLY';
    badge.className = 'badge ' + (control ? 'badge-control' : 'badge-view');
    setState('VIEW_ACTIVE');
  } else {
    // The session is over: its monitor list goes with it.
    renderDisplays(null);
    setState('READY');
  }
  reloadConfig();
});

listen('toast', (e) => toast(e.payload.message || '', 'error'));

// ---- monitor switching (mid-session) -----------------------------------------
// The host announces its monitors and the one being shared; both sides get a
// `displays` event (the host from its own capture thread, the viewer from the
// host). The pickers show only with two or more monitors. The viewer's is
// disabled without control — the host would ignore the request anyway. On
// macOS/Linux the viewer's picker is the native "Monitor" menu (the drawer
// holding this one is not shown there).
//
// Pop-out windows (D14): the viewer can also open other monitors in separate
// windows ("Pop out…" here, "Open in New Window" in the native menu). The
// windows themselves are opened and closed by the app's Rust side, following
// the host's list; this page only asks.
function displayLabel(d) {
  const name = d.name || d.id;
  return `${name} (${d.width}\u00d7${d.height}${d.primary ? ', primary' : ''})`;
}

function renderDisplayPicker(select, info) {
  const list = (info && info.displays) || [];
  select.textContent = '';
  for (const d of list) {
    const opt = document.createElement('option');
    opt.value = d.id;
    opt.textContent = displayLabel(d);
    select.appendChild(opt);
  }
  select.value = (info && info.current) || '';
  return list.length >= 2;
}

function renderPopoutPicker(info) {
  const select = $('view-popout');
  const list = (info && info.displays) || [];
  const popped = new Set(((info && info.popouts) || []).map((p) => p.id));
  const others = list.filter((d) => d.id !== info.current && !popped.has(d.id));
  select.textContent = '';
  const head = document.createElement('option');
  head.value = '';
  head.textContent = 'Pop out\u2026';
  select.appendChild(head);
  for (const d of others) {
    const opt = document.createElement('option');
    opt.value = d.id;
    opt.textContent = displayLabel(d);
    select.appendChild(opt);
  }
  select.value = '';
  select.classList.toggle('hidden', others.length === 0);
  select.disabled = !info.canSwitch;
  select.title = select.disabled
    ? 'Separate windows need control'
    : 'Show another monitor in a separate window';
}

function renderDisplays(info) {
  const hostShown = renderDisplayPicker($('host-display'), info);
  $('host-display-row').classList.toggle('hidden', !hostShown);
  // Host: say which monitors the viewer ALSO sees, in separate windows.
  const list = (info && info.displays) || [];
  const popouts = (info && info.popouts) || [];
  const names = popouts.map((p) => {
    const d = list.find((x) => x.id === p.id);
    return d ? displayLabel(d) : p.id;
  });
  const note = $('host-popouts');
  note.textContent = names.length
    ? 'Also shown in separate windows: ' + names.join(', ')
    : '';
  note.classList.toggle('hidden', names.length === 0);
  if (info) {
    renderPopoutPicker(info);
  } else {
    $('view-popout').classList.add('hidden');
  }
  const view = $('view-display');
  const viewShown = renderDisplayPicker(view, info);
  view.classList.toggle('hidden', !viewShown);
  view.disabled = !(info && info.canSwitch);
  view.title = view.disabled
    ? 'Switching monitors needs control'
    : 'Monitor to show';
}

listen('displays', (e) => renderDisplays(e.payload));

for (const id of ['host-display', 'view-display']) {
  $(id).addEventListener('change', (e) => {
    invoke('select_display', { id: e.target.value });
  });
}
$('view-popout').addEventListener('change', (e) => {
  const id = e.target.value;
  e.target.value = '';
  if (id) invoke('popout_display', { id });
});

// The peer went quiet. The engine has already hidden the native video (a frozen
// frame is indistinguishable from a live one), so this panel is what the user
// sees while the grace period runs.
listen('link-trouble', (e) => {
  if (state !== 'VIEW_ACTIVE' && state !== 'HOST_ACTIVE') return;
  $('link-count').textContent = String(e.payload.secsLeft ?? 0);
  $('modal-link').classList.remove('hidden');
});

listen('link-restored', () => {
  $('modal-link').classList.add('hidden');
  toast('Connection restored');
});

// ---- boot ------------------------------------------------------------------
(async () => {
  await reloadConfig();
  syncNameField();
  syncNow(false); // signed in: pick up what other computers changed meanwhile
  await applyPlatformInfo();
  render();
  // Back-fill the server status: the engine registers ~0.5 s after launch and
  // emits a one-shot 'server-status' event that can fire before this WebView
  // subscribed above. Without this pull the dot would stay stuck on OFFLINE and
  // the Connect button (gated on state === 'READY') would never enable.
  try {
    const s = await invoke('get_server_status');
    if (s === 'connected') {
      setStatus('green', 'server: connected');
      setBanner('');
      if (state === 'OFFLINE') setState('READY');
    }
  } catch (_) {
    /* command unavailable (older engine) — the event path still covers it */
  }
  // Before the first host session: sharing the screen needs the OS grant, and
  // a connection arriving later is better met with it already in place.
  if (!hostPreflightDone) {
    hostPreflightDone = true;
    await preflight(
      ['screenCapture', 'inputInjection'],
      'Grant these so others can see and control this computer when you let them in.'
    );
  }
  // Non-blocking update check at startup (task U1): never hold up boot or hosting.
  checkForUpdates(true).catch(() => {});
})();
