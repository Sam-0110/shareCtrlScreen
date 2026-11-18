// The host side: the incoming-connection approval modal and the host screen's
// own controls (permission switch, end session).

import * as api from './api.js';
import { peerLabel } from './connect.js';
import { $, S, countdown, setState, stopCountdown, windowRemote } from './state.js';

// incoming approval (host)
function answerIncoming(decision) {
  stopCountdown();
  if (S.incomingFrom) api.approve(S.incomingFrom, decision);
  S.incomingFrom = null;
  if (decision === 'deny') setState('READY');
}

export function wireHost() {
  $('btn-incoming-deny').addEventListener('click', () => answerIncoming('deny'));
  $('btn-incoming-view').addEventListener('click', () => answerIncoming('view'));
  $('btn-incoming-control').addEventListener('click', () => answerIncoming('control'));

  // host session controls
  $('host-perm').addEventListener('change', () => {
    api.setPermission($('host-perm').value);
  });
  $('btn-host-end').addEventListener('click', () => api.endSession());

  api.on('approval-request', (e) => {
    // Asked in the main window only.
    if (windowRemote) return;
    S.incomingFrom = e.payload.from;
    $('incoming-peer').textContent = peerLabel(S.incomingFrom);
    setState('INCOMING');
    stopCountdown();
    // The Rust decider auto-denies at 30 s too; this just mirrors the countdown.
    S.cancelCountdown = countdown('incoming-count', 30, () => {
      S.incomingFrom = null;
      setState('READY');
    });
  });
}
