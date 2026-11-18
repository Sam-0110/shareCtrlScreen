// The session screens: this computer being viewed (the host screen, its role
// changes and "connection lost" panel), the session bar over a tab's video,
// and the monitor pickers and pop-outs both share. Which tab is on screen,
// and its state, is tabs.js's (plans/08 P4).

import * as api from './api.js';
import { peerLabel, reloadConfig } from './connect.js';
import { $, S, setState, stopCountdown, toast } from './state.js';

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
  return `${name} (${d.width}×${d.height}${d.primary ? ', primary' : ''})`;
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
  head.textContent = 'Pop out…';
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

export function renderDisplays(info) {
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

export function wireSession() {
  // Session menu bar (revealed by hovering the top edge of the video).
  $('btn-view-refresh').addEventListener('click', () => {
    api.requestRefresh();
    toast('Requested a fresh frame');
  });
  $('btn-view-end').addEventListener('click', () => api.endSession());
  // Don't wait out the grace period — give up now.
  $('btn-link-end').addEventListener('click', () => {
    $('modal-link').classList.add('hidden');
    api.endSession();
  });

  // This computer being viewed (the host session; a viewer's are tabs).
  api.on('role-changed', (e) => {
    const p = e.payload;
    stopCountdown();
    // Whatever happens next (reconnected, or dropped back to the home screen),
    // the "connection lost" panel is stale.
    $('modal-link').classList.add('hidden');
    if (p.role === 'host') {
      $('host-peer').textContent = peerLabel(p.peer);
      $('host-perm').value = p.permission;
      setState('HOST_ACTIVE');
    } else {
      // The session is over: its monitor list goes with it.
      renderDisplays(null);
      setState('READY');
    }
    reloadConfig();
  });

  api.on('displays', (e) => renderDisplays(e.payload));

  for (const id of ['host-display', 'view-display']) {
    $(id).addEventListener('change', (e) => {
      api.selectDisplay(e.target.value);
    });
  }
  $('view-popout').addEventListener('change', (e) => {
    const id = e.target.value;
    e.target.value = '';
    if (id) api.popoutDisplay(id);
  });

  // The viewer went quiet while this computer is viewed. (A tab's trouble is
  // in its snapshot: tabs.js shows the same panel.)
  api.on('link-trouble', (e) => {
    if (S.state !== 'HOST_ACTIVE') return;
    $('link-count').textContent = String(e.payload.secsLeft ?? 0);
    $('modal-link').classList.remove('hidden');
  });

  api.on('link-restored', () => {
    $('modal-link').classList.add('hidden');
    toast('Connection restored');
  });
}
