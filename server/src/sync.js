// The synced document: the server's half of native/crates/protocol/src/sync/
// (mod.rs, tabs.rs, prefs.rs, clock.rs). Pure functions, no cloudflare:*
// imports, so Node runs them directly against the shared merge cases in
// native/crates/protocol/testdata/sync-vectors.json (test/sync-vectors.mjs).
// Any rule changed here must change in the Rust code too, and the reverse.
//
// The document is last-writer-wins per field:
//   entries          per remote ID: name + nameAt (an empty name with a
//                    non-zero nameAt is a removed name) and usedAt
//   recentsClearedAt one account-wide time; an ID is a recent if used after it
//   tabs (v2)        per remote ID: order + orderAt (fractional index key),
//                    openedAt, closedAt; open ⇔ openedAt > closedAt
//   prefs (v2)       per whitelisted preference: value (boolean) + at
// Merging keeps the newer value of each, so every computer converges no matter
// the order of syncs. Every stamp from a client is clamped to at most a few
// minutes past the server's clock, so a fast clock cannot win forever.

export const SYNC_DOC_VERSION = 2;
export const MAX_ENTRIES = 500; // MAX_SYNC_ENTRIES in sync/mod.rs
export const MAX_TABS = 1000; // MAX_SYNC_TABS in sync/tabs.rs
export const MAX_RAW_ENTRIES = 2000; // read at most this many from one request
export const MAX_RAW_TABS = 4000;
export const MAX_NAME_CHARS = 40; // MAX_REMOTE_NAME_CHARS in config/mod.rs
export const MAX_ORDER_KEY_CHARS = 24; // sync/tabs.rs
export const FUTURE_SLACK_MS = 5 * 60 * 1000; // sync/clock.rs
export const CLOSED_TAB_TTL_MS = 90 * 24 * 3600 * 1000; // sync/tabs.rs
// sync/prefs.rs SYNCED_PREFS: the only preferences that follow the account.
export const SYNCED_PREFS = [
  "translateModifiers",
  "captureShortcuts",
  "clipboardSync",
  "pasteDroppedFiles",
];
// What this server stores, advertised in every sync reply. A client sends tabs
// and preferences only to a server that lists them.
export const FEATURES = ["prefs", "tabs"];

const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const ORDER_KEY_RE = new RegExp(`^[0-9a-z]{1,${MAX_ORDER_KEY_CHARS}}$`);

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);

// A name as stored: control characters dropped, whitespace runs folded to one
// space, trimmed, capped at MAX_NAME_CHARS code points. "" when empty.
export function cleanName(name) {
  if (typeof name !== "string") return "";
  const folded = name
    .split(/[\s\p{Cc}]+/u)
    .filter((w) => w)
    .join(" ");
  return Array.from(folded).slice(0, MAX_NAME_CHARS).join("").trimEnd();
}

// Code-point order, which is the byte order Rust compares Strings in, so a
// tie is broken the same way on both sides.
function cmpCodePoints(a, b) {
  const x = Array.from(a);
  const y = Array.from(b);
  for (let i = 0; i < Math.min(x.length, y.length); i++) {
    const d = x[i].codePointAt(0) - y[i].codePointAt(0);
    if (d) return d;
  }
  return x.length - y.length;
}

export function emptyDoc() {
  return { v: SYNC_DOC_VERSION, entries: {}, recentsClearedAt: 0, tabs: {}, prefs: {} };
}

function mergeEntry(into, other) {
  if (
    other.nameAt > into.nameAt ||
    (other.nameAt === into.nameAt && cmpCodePoints(other.name, into.name) > 0)
  ) {
    into.name = other.name;
    into.nameAt = other.nameAt;
  }
  into.usedAt = Math.max(into.usedAt, other.usedAt);
}

function mergeTab(into, other) {
  if (
    other.orderAt > into.orderAt ||
    (other.orderAt === into.orderAt && cmpCodePoints(other.order, into.order) > 0)
  ) {
    into.order = other.order;
    into.orderAt = other.orderAt;
  }
  into.openedAt = Math.max(into.openedAt, other.openedAt);
  into.closedAt = Math.max(into.closedAt, other.closedAt);
}

// The later change wins; the same stamp goes to true (false < true).
function mergePref(into, other) {
  if (other.at > into.at || (other.at === into.at && other.value && !into.value)) {
    into.value = other.value;
    into.at = other.at;
  }
}

const isOpen = (t) => t.openedAt > t.closedAt;

// Over a cap: keep named IDs first, then the most recently touched; keep open
// tabs first, then the most recently touched. Ties by ID.
function prune(doc) {
  const byId = (a, b) => (a < b ? -1 : a > b ? 1 : 0);
  const ids = Object.keys(doc.entries);
  if (ids.length > MAX_ENTRIES) {
    const touched = (e) => Math.max(e.nameAt, e.usedAt);
    ids.sort((a, b) => {
      const ea = doc.entries[a];
      const eb = doc.entries[b];
      const named = (eb.name !== "") - (ea.name !== "");
      if (named) return named;
      const t = touched(eb) - touched(ea);
      return t || byId(a, b);
    });
    for (const id of ids.slice(MAX_ENTRIES)) delete doc.entries[id];
  }
  const tabIds = Object.keys(doc.tabs);
  if (tabIds.length > MAX_TABS) {
    const touched = (t) => Math.max(t.orderAt, t.openedAt, t.closedAt);
    tabIds.sort((a, b) => {
      const ta = doc.tabs[a];
      const tb = doc.tabs[b];
      const open = isOpen(tb) - isOpen(ta);
      if (open) return open;
      const t = touched(tb) - touched(ta);
      return t || byId(a, b);
    });
    for (const id of tabIds.slice(MAX_TABS)) delete doc.tabs[id];
  }
  return doc;
}

// Merge two normalized documents into a new one.
export function mergeDocs(a, b) {
  const out = emptyDoc();
  for (const doc of [a, b]) {
    for (const [id, e] of Object.entries(doc.entries)) {
      const cur = out.entries[id] || (out.entries[id] = { name: "", nameAt: 0, usedAt: 0 });
      mergeEntry(cur, e);
    }
    for (const [id, t] of Object.entries(doc.tabs || {})) {
      const cur = out.tabs[id];
      if (cur) mergeTab(cur, t);
      else out.tabs[id] = { ...t };
    }
    for (const [key, p] of Object.entries(doc.prefs || {})) {
      const cur = out.prefs[key] || (out.prefs[key] = { value: false, at: 0 });
      mergePref(cur, p);
    }
  }
  out.recentsClearedAt = Math.max(a.recentsClearedAt, b.recentsClearedAt);
  return prune(out);
}

// A time from a client: a non-negative integer, never later than
// `now + FUTURE_SLACK_MS` (a computer whose clock runs ahead must not win every
// future edit).
function stamp(v, now) {
  return Number.isSafeInteger(v) && v > 0 ? Math.min(v, now + FUTURE_SLACK_MS) : 0;
}

// Validate and clean an untrusted document: {doc, tooLarge}, or null if it is
// not one at all. `tooLarge`: it carries more tabs than the cap, and the caller
// refuses it rather than silently dropping tabs.
export function normalizeIncoming(raw, now) {
  if (!isObject(raw)) return null;
  const out = emptyDoc();

  const entries = isObject(raw.entries) ? raw.entries : {};
  let seen = 0;
  for (const [rawId, e] of Object.entries(entries)) {
    if (++seen > MAX_RAW_ENTRIES) break;
    const id = rawId.trim().toLowerCase();
    if (!UUID_RE.test(id) || !isObject(e)) continue;
    const clean = {
      name: cleanName(e.name),
      nameAt: stamp(e.nameAt, now),
      usedAt: stamp(e.usedAt, now),
    };
    const cur = out.entries[id] || (out.entries[id] = { name: "", nameAt: 0, usedAt: 0 });
    mergeEntry(cur, clean);
  }
  out.recentsClearedAt = stamp(raw.recentsClearedAt, now);

  const tabs = isObject(raw.tabs) ? raw.tabs : {};
  const forgetBefore = now - CLOSED_TAB_TTL_MS;
  seen = 0;
  for (const [rawId, t] of Object.entries(tabs)) {
    if (++seen > MAX_RAW_TABS) break;
    const id = rawId.trim().toLowerCase();
    if (!UUID_RE.test(id) || !isObject(t)) continue;
    if (typeof t.order !== "string" || !ORDER_KEY_RE.test(t.order)) continue;
    const clean = {
      order: t.order,
      orderAt: stamp(t.orderAt, now),
      openedAt: stamp(t.openedAt, now),
      closedAt: stamp(t.closedAt, now),
    };
    const cur = out.tabs[id];
    if (cur) mergeTab(cur, clean);
    else out.tabs[id] = clean;
  }
  for (const [id, t] of Object.entries(out.tabs)) {
    if (!isOpen(t) && t.closedAt < forgetBefore) delete out.tabs[id];
  }
  const tooLarge = Object.keys(out.tabs).length > MAX_TABS;

  const prefs = isObject(raw.prefs) ? raw.prefs : {};
  for (const key of SYNCED_PREFS) {
    const p = prefs[key];
    if (!isObject(p) || typeof p.value !== "boolean") continue;
    out.prefs[key] = { value: p.value, at: stamp(p.at, now) };
  }

  prune(out);
  return { doc: out, tooLarge };
}

// Validate and clean an untrusted document; null if it is not one at all.
export function normalizeDoc(raw, now) {
  const r = normalizeIncoming(raw, now);
  return r && r.doc;
}
