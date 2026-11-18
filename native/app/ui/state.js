// The UI's one state machine (Plan 04 §7, mirroring the original Electron
// renderer) and the small view helpers every module shares.
//
// Shared mutable state lives on the exported object `S`: an ES-module import
// is a read-only binding, so modules that must WRITE shared state (the screen,
// the loaded config, the peer awaiting an answer) do it through `S.<field>`.

export const $ = (id) => document.getElementById(id);

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

export const S = {
  state: 'OFFLINE',
  config: null,
  incomingFrom: null, // peer awaiting our approve/deny
  passwordFrom: null, // host that demanded a password
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

export const sessionLive = () => S.state === 'HOST_ACTIVE' || S.state === 'VIEW_ACTIVE';

export function setState(next) {
  if (!STATES.includes(next)) throw new Error('unknown state ' + next);
  S.state = next;
  render();
}

export function render() {
  const state = S.state;
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

// ---- countdown helper (shared by REQUESTING and INCOMING) ------------------
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

/** Start the viewer's 30 s wait for the host's answer (contract §3.3). */
export function startRequestCountdown() {
  stopCountdown();
  S.cancelCountdown = countdown('requesting-count', 30, () => {
    toast('No answer — timed out', 'error');
    setState('READY');
  });
}
