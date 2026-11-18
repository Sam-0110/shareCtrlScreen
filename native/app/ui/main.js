// UI entry point for the Tauri shell (Plan 04 §7). Mirrors the original
// app/renderer/ui.js state machine and markup 1:1 so the look and behaviour are
// unchanged; only the plumbing differs:
//   ipcRenderer.invoke(...) -> window.__TAURI__.core.invoke(...)  (api.js)
//   ipcRenderer.on(...)     -> window.__TAURI__.event.listen(...) (api.js)
// WebRTC/signaling/capture all live in the Rust engine now.
//
// Module layout (plans/08a §2.4): state.js (state machine, shared state, view
// helpers), api.js (every command and event), connect.js (home screen,
// recents, names, viewer prompts), accounts.js (sign-in and sync),
// settings.js (settings, permissions, OpenH264, updates), host.js (approval
// and host controls), session.js (the session bar and the host screen),
// tabs.js (the tab strip and the tab on screen). Modules only define
// functions at load time; every listener is attached here, in one order, once
// all modules have loaded — so the import cycles between them are harmless.

import * as api from './api.js';
import { refreshAccount, wireAccounts } from './accounts.js';
import { reloadConfig, syncNameField, wireConnect } from './connect.js';
import { wireHost } from './host.js';
import { wireSession } from './session.js';
import { applyPlatformInfo, checkForUpdates, preflight, wireSettings } from './settings.js';
import {
  S,
  overlaysReset,
  render,
  setBanner,
  setState,
  setStatus,
  toast,
  windowRemote
} from './state.js';
import { loadTabs, wireTabs } from './tabs.js';

// The server link is up: green dot, no banner, and an offline app becomes ready.
function serverConnected() {
  setStatus('green', 'server: connected');
  setBanner('');
  if (S.state === 'OFFLINE') setState('READY');
}

// A detached tab window (plans/09): the same page, showing one tab.
if (windowRemote) document.body.classList.add('window-mode');

wireAccounts();
wireSettings();
wireSession();
wireConnect();
wireHost();
wireTabs();

api.on('server-status', (e) => {
  const s = e.payload.status;
  if (s === 'connected') {
    serverConnected();
  } else if (s === 'reconnecting') {
    // A live session keeps running on its own WebRTC leg — yellow, not offline.
    if (S.state === 'HOST_ACTIVE' || S.tabs.connected > 0) {
      setStatus('yellow', 'server: reconnecting (session active)');
    } else {
      setStatus('red', 'server: disconnected (retrying)');
      if (S.state !== 'OFFLINE') setState('OFFLINE');
    }
  }
});

// The computer's own messages show in the main window only.
api.on('toast', (e) => {
  if (!windowRemote) toast(e.payload.message || '', 'error');
});

// ---- boot ------------------------------------------------------------------
(async () => {
  // A fresh page has no dialog open: clear the engine's app-wide "a dialog
  // covers the video" flag in case the page reloaded with one open.
  overlaysReset();
  await reloadConfig();
  // The tab strip as the app has it (empty on launch: nothing auto-connects).
  await loadTabs();
  syncNameField();
  // The account's state, including a bring-in question still open after a reload.
  refreshAccount();
  await applyPlatformInfo();
  render();
  // Back-fill the server status: the engine registers ~0.5 s after launch and
  // emits a one-shot 'server-status' event that can fire before this WebView
  // subscribed above. Without this pull the dot would stay stuck on OFFLINE and
  // the Connect button (gated on state === 'READY') would never enable.
  try {
    const s = await api.getServerStatus();
    if (s === 'connected') serverConnected();
  } catch (_) {
    /* command unavailable (older engine) — the event path still covers it */
  }
  // The rest is the main window's (a detached window shows one tab).
  if (windowRemote) return;
  // Before the first host session: sharing the screen needs the OS grant, and
  // a connection arriving later is better met with it already in place.
  if (!S.hostPreflightDone) {
    S.hostPreflightDone = true;
    await preflight(
      ['screenCapture', 'inputInjection'],
      'Grant these so others can see and control this computer when you let them in.'
    );
  }
  // Non-blocking update check at startup (task U1): never hold up boot or hosting.
  checkForUpdates(true).catch(() => {});
})();
