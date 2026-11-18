// Settings and everything reached from it: the OS-specific wording, the
// settings modal, the permissions modal (and the automatic preflight prompts),
// the Cisco OpenH264 codec panel and the updates panel, plus the native menu's
// Settings / Check-for-updates items.

import * as api from './api.js';
import { renderAccount, setAccountStatus } from './accounts.js';
import { reloadConfig } from './connect.js';
import {
  $,
  S,
  errorText,
  overlayClose,
  overlayOpen,
  sessionLive,
  toast,
  windowRemote
} from './state.js';

// ---- platform wording + displays -------------------------------------------
// The OS facts the UI words itself around (see `get_platform_info`). The HTML
// ships neutral defaults, so a failed call leaves a correct, if generic, page.
export async function applyPlatformInfo() {
  let info;
  try {
    info = await api.getPlatformInfo();
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
  const config = S.config;
  $('set-server').value = config.serverUrl;
  $('set-mode-approve').checked = config.mode === 'approve';
  $('set-mode-password').checked = config.mode === 'password';
  $('set-password').value = '';
  $('set-password').placeholder = config.hasPassword ? '(unchanged)' : '(none set)';
  $('set-password-clear').checked = false;
  $('set-password-perm').value = config.passwordPermission;
  $('set-share-audio').checked = config.shareAudio !== false;
  $('set-share-display').value = config.shareDisplayId || '';
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
  // Over a live session the video goes first (it sits above this page; the
  // dialog would otherwise paint under it).
  overlayOpen('modal-settings');
}

function closeSettings() {
  overlayClose('modal-settings');
}

async function saveSettings() {
  const patch = {
    serverUrl: $('set-server').value.trim(),
    mode: $('set-mode-password').checked ? 'password' : 'approve',
    passwordPermission: $('set-password-perm').value,
    shareAudio: $('set-share-audio').checked,
    shareDisplayId: $('set-share-display').value || null,
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
    await api.saveSettings(patch);
  } catch (err) {
    toast(errorText(err), 'error');
    return;
  }

  closeSettings();
  toast('Settings saved');
  await reloadConfig();
}

// The session bar's gear: Settings over the video (openSettings hides it first).
function openSettingsOverVideo() {
  openSettings();
}

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
    report = await api.getPermissions();
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
    await api.requestPermission(key);
  } catch (err) {
    toast(errorText(err), 'error');
    return;
  }
  // The user answers in an OS dialog or settings pane; watch for the result.
  startPermPoll();
  refreshPermissions();
}

async function openPermissions(intro) {
  let report;
  try {
    report = await api.getPermissions();
  } catch (err) {
    toast(errorText(err), 'error');
    return;
  }
  $('perm-intro').textContent = intro || '';
  renderPermissions(report);
  // Same rule as Settings: the video goes before the dialog paints.
  overlayOpen('modal-permissions');
  if (PERMISSIONS.some((p) => permNeedsGrant(report[p.key]))) startPermPoll();
}

function closePermissions() {
  stopPermPoll();
  overlayClose('modal-permissions');
}

// Show the modal on its own when `keys` still need granting. Returns whether it
// was shown, so a caller can hold back the action the permission is for.
export async function preflight(keys, intro) {
  let report;
  try {
    report = await api.getPermissions();
  } catch (_) {
    return false;
  }
  if (!keys.some((k) => permNeedsGrant(report[k]))) return false;
  await openPermissions(intro);
  return true;
}

// ---- OpenH264 ------------------------------------------------------------------
// Settings → Advanced: the Cisco OpenH264 software codec. The notice and the
// licence text come from the backend (Cisco's licence requires both to be
// shown where the user controls the codec).
async function refreshOpenH264() {
  try {
    const info = await api.openh264Info();
    $('openh264-notice').textContent = info.notice;
    $('openh264-license').textContent = info.license;
    // The "no H.264 at all" warning leads: it is the one line the user must act on.
    $('openh264-status').textContent = info.problem ? `${info.problem} ${info.status}` : info.status;
    $('btn-openh264-download').disabled = !!info.ready;
  } catch (err) {
    $('openh264-status').textContent = errorText(err);
  }
}

// ---- updates (task U1) -----------------------------------------------------
// The flow lives in Rust (updater.rs): `updater_check` reports whether a newer
// release exists; `updater_install` ends any live session, downloads, installs
// and restarts (service-aware on Windows). This panel is the thin surface, plus
// a quiet non-blocking check at startup.
let lastUpdate = null;
let updating = false;

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
    await api.openReleasesPage();
  } catch (err) {
    toast(errorText(err), 'error');
  }
}

export async function checkForUpdates(silent) {
  const statusEl = $('update-status');
  if (!silent) statusEl.textContent = 'Checking…';
  try {
    lastUpdate = await api.updaterCheck();
  } catch (err) {
    lastUpdate = null;
    if (!silent) statusEl.textContent = 'Could not check: ' + errorText(err);
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
    await api.updaterInstall();
    prog.textContent = 'Installed — restarting…';
  } catch (err) {
    updating = false;
    btn.disabled = false;
    prog.classList.add('hidden');
    prog.textContent = '';
    toast(errorText(err), 'error');
  }
}

export function wireSettings() {
  $('btn-openh264-download').addEventListener('click', async () => {
    const btn = $('btn-openh264-download');
    if (!$('set-openh264').checked) {
      toast('Turn on "Use Cisco OpenH264" and save first.', 'error');
      return;
    }
    btn.disabled = true;
    $('openh264-status').textContent = 'Downloading from Cisco…';
    try {
      await api.openh264Download();
    } catch (err) {
      toast(errorText(err), 'error');
    }
    await refreshOpenH264();
  });

  $('btn-openh264-license').addEventListener('click', () => {
    $('openh264-license').classList.toggle('hidden');
  });

  api.on('updater-progress', (e) => {
    const prog = $('update-progress');
    const { downloaded, total } = e.payload || {};
    if (total) {
      const pct = Math.min(100, Math.round((downloaded / total) * 100));
      prog.textContent = 'Downloading… ' + pct + '%';
    } else if (downloaded) {
      prog.textContent = 'Downloading… ' + Math.round(downloaded / 1024) + ' KB';
    }
  });

  $('btn-update-check').addEventListener('click', () => checkForUpdates(false));
  $('btn-update-install').addEventListener('click', installUpdate);
  $('btn-update-download').addEventListener('click', downloadUpdate);
  $('btn-settings-permissions').addEventListener('click', () => openPermissions(''));
  $('btn-permissions-close').addEventListener('click', closePermissions);
  $('btn-settings').addEventListener('click', openSettings);
  // Home screen, signed-out account: its sign-in form is in Settings.
  $('btn-locked-sign-in').addEventListener('click', openSettings);
  $('btn-settings-cancel').addEventListener('click', closeSettings);
  $('btn-settings-save').addEventListener('click', saveSettings);

  $('btn-view-settings').addEventListener('click', openSettingsOverVideo);
  $('btn-host-settings').addEventListener('click', openSettingsOverVideo);

  // Native menu (macOS + Linux; task M15). The menu bar replaces the top-edge
  // drawer there — Settings/Disconnect/Check-for-updates are wired to the SAME
  // flows the toolbar buttons use. (Disconnect is handled in Rust straight on the
  // engine; the role-changed event then updates the UI, exactly like the
  // end-session buttons.) No menu exists on Windows, so these never fire there.
  // The native menu's Settings / updates are the main window's (plans/09).
  api.on('menu://settings', () => {
    if (windowRemote) return;
    openSettingsOverVideo();
  });
  api.on('menu://check-updates', () => {
    if (windowRemote) return;
    openSettingsOverVideo().then(() => {
      checkForUpdates(false);
      // Bring the Updates panel into view inside the settings modal.
      const el = $('btn-update-check');
      if (el) el.scrollIntoView({ block: 'center' });
    });
  });
}
