// Accounts + remote-ID sync (HTTP, under /api). Separate from signaling: the
// SignalingRoom never sees any of this, and accounts are not needed to connect.
//
// One AccountStore Durable Object per username holds that account's password
// hash, its sign-in tokens (hashes only) and its synced document. The document
// is the same last-writer-wins value the app merges (native/crates/protocol/
// src/sync.rs): per remote ID a name + nameAt (an empty name with a non-zero
// nameAt is a removed name) and usedAt, plus one recentsClearedAt. Merging
// keeps the newer name and the later times, so every device converges no
// matter the order of syncs. mergeDocs/normalizeDoc below must keep the same
// rules as the Rust side.
//
//   POST /api/account/signup  {username, password}  → {username, token}
//   POST /api/account/login   {username, password}  → {username, token}
//   POST /api/account/logout  (Bearer)              → {ok: true}
//   POST /api/sync            (Bearer) {doc?}       → {doc}   (merged)
//
// Errors are {error: "<code>"} with a 4xx status.

import { DurableObject } from "cloudflare:workers";

const UUID_RE =
  /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const USERNAME_RE = /^[a-z0-9][a-z0-9._-]{2,31}$/;
const MIN_PASSWORD = 8;
const MAX_PASSWORD = 256;
// The most Workers' WebCrypto allows for PBKDF2.
const PBKDF2_ITERATIONS = 100000;
const MAX_BODY = 262144; // 256 KB
const TOKEN_PREFIX = "sct1";
const MAX_TOKENS = 20; // per account: oldest-used dropped beyond this
const TOKEN_IDLE_MS = 180 * 24 * 3600 * 1000; // unused this long → expired
const TOKEN_TOUCH_MS = 24 * 3600 * 1000; // lastUsedAt write granularity
const LOGIN_WINDOW_MS = 15 * 60 * 1000;
const LOGIN_MAX_FAILS = 10; // per account per window
const MAX_ENTRIES = 500; // same as MAX_SYNC_ENTRIES in sync.rs
const MAX_RAW_ENTRIES = 2000; // read at most this many from one request
const MAX_NAME_CHARS = 40; // same as MAX_REMOTE_NAME_CHARS in config.rs

// ---- helpers ----------------------------------------------------------------

function json(status, body) {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      "content-type": "application/json",
      "cache-control": "no-store",
    },
  });
}

const enc = new TextEncoder();

function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

function b64urlDecode(s) {
  const b = atob(s.replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(b, (c) => c.charCodeAt(0));
}

function randomBytes(n) {
  return crypto.getRandomValues(new Uint8Array(n));
}

async function sha256Hex(text) {
  const d = new Uint8Array(await crypto.subtle.digest("SHA-256", enc.encode(text)));
  return Array.from(d, (b) => b.toString(16).padStart(2, "0")).join("");
}

async function pbkdf2(password, salt, iterations) {
  const key = await crypto.subtle.importKey("raw", enc.encode(password), "PBKDF2", false, [
    "deriveBits",
  ]);
  const bits = await crypto.subtle.deriveBits(
    { name: "PBKDF2", hash: "SHA-256", salt, iterations },
    key,
    256
  );
  return new Uint8Array(bits);
}

function sameBytes(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  return crypto.subtle.timingSafeEqual(a, b);
}

export function normalizeUsername(u) {
  if (typeof u !== "string") return null;
  const n = u.trim().toLowerCase();
  return USERNAME_RE.test(n) ? n : null;
}

// "sct1.<username>.<secret>" → {username, secret}, or null.
export function parseToken(header) {
  if (typeof header !== "string") return null;
  const m = /^Bearer\s+(\S+)$/i.exec(header.trim());
  if (!m) return null;
  const t = m[1];
  if (!t.startsWith(TOKEN_PREFIX + ".")) return null;
  const last = t.lastIndexOf(".");
  const username = t.slice(TOKEN_PREFIX.length + 1, last);
  const secret = t.slice(last + 1);
  if (!USERNAME_RE.test(username) || !/^[A-Za-z0-9_-]{43}$/.test(secret)) return null;
  return { username, secret };
}

// ---- the synced document (mirror of native/crates/protocol/src/sync.rs) -----

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
// tie between two names is broken the same way on both sides.
function cmpCodePoints(a, b) {
  const x = Array.from(a);
  const y = Array.from(b);
  for (let i = 0; i < Math.min(x.length, y.length); i++) {
    const d = x[i].codePointAt(0) - y[i].codePointAt(0);
    if (d) return d;
  }
  return x.length - y.length;
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

function emptyDoc() {
  return { v: 1, entries: {}, recentsClearedAt: 0 };
}

// Over the cap: keep named IDs first, then the most recently touched.
function prune(doc) {
  const ids = Object.keys(doc.entries);
  if (ids.length <= MAX_ENTRIES) return doc;
  const touched = (e) => Math.max(e.nameAt, e.usedAt);
  ids.sort((a, b) => {
    const ea = doc.entries[a];
    const eb = doc.entries[b];
    const named = (eb.name !== "") - (ea.name !== "");
    if (named) return named;
    const t = touched(eb) - touched(ea);
    if (t) return t;
    return a < b ? -1 : a > b ? 1 : 0;
  });
  for (const id of ids.slice(MAX_ENTRIES)) delete doc.entries[id];
  return doc;
}

export function mergeDocs(a, b) {
  const out = emptyDoc();
  for (const doc of [a, b]) {
    for (const [id, e] of Object.entries(doc.entries)) {
      const cur = out.entries[id] || (out.entries[id] = { name: "", nameAt: 0, usedAt: 0 });
      mergeEntry(cur, e);
    }
  }
  out.recentsClearedAt = Math.max(a.recentsClearedAt, b.recentsClearedAt);
  return prune(out);
}

// A time from a client: a non-negative integer, never later than `now` (a
// device whose clock runs ahead must not win every future edit).
function stamp(v, now) {
  return Number.isSafeInteger(v) && v > 0 ? Math.min(v, now) : 0;
}

// Validate and clean an untrusted document. null if it is not one at all.
export function normalizeDoc(raw, now) {
  if (typeof raw !== "object" || raw === null || Array.isArray(raw)) return null;
  const out = emptyDoc();
  const entries =
    typeof raw.entries === "object" && raw.entries !== null && !Array.isArray(raw.entries)
      ? raw.entries
      : {};
  let seen = 0;
  for (const [rawId, e] of Object.entries(entries)) {
    if (++seen > MAX_RAW_ENTRIES) break;
    const id = rawId.trim().toLowerCase();
    if (!UUID_RE.test(id)) continue;
    if (typeof e !== "object" || e === null || Array.isArray(e)) continue;
    const clean = {
      name: cleanName(e.name),
      nameAt: stamp(e.nameAt, now),
      usedAt: stamp(e.usedAt, now),
    };
    const cur = out.entries[id] || (out.entries[id] = { name: "", nameAt: 0, usedAt: 0 });
    mergeEntry(cur, clean);
  }
  out.recentsClearedAt = stamp(raw.recentsClearedAt, now);
  return prune(out);
}

// ---- the per-account Durable Object ------------------------------------------

export class AccountStore extends DurableObject {
  async signup(username, password) {
    // Hash first: between the get and the put below there are only storage
    // calls, which keep other requests out, so two signups cannot both win.
    const salt = randomBytes(16);
    const hash = await pbkdf2(password, salt, PBKDF2_ITERATIONS);
    if (await this.ctx.storage.get("auth")) {
      return { status: 409, body: { error: "username-taken" } };
    }
    await this.ctx.storage.put("auth", {
      salt: b64url(salt),
      hash: b64url(hash),
      iterations: PBKDF2_ITERATIONS,
      createdAt: Date.now(),
    });
    return { status: 200, body: { username, token: await this.issueToken(username) } };
  }

  async login(username, password) {
    const now = Date.now();
    const t = await this.ctx.storage.get("throttle");
    if (t && now - t.windowStart < LOGIN_WINDOW_MS && t.fails >= LOGIN_MAX_FAILS) {
      return { status: 429, body: { error: "too-many-attempts" } };
    }
    const auth = await this.ctx.storage.get("auth");
    if (!auth) return { status: 401, body: { error: "bad-credentials" } };
    const got = await pbkdf2(password, b64urlDecode(auth.salt), auth.iterations);
    if (!sameBytes(got, b64urlDecode(auth.hash))) {
      const cur = await this.ctx.storage.get("throttle");
      const fresh = !cur || Date.now() - cur.windowStart >= LOGIN_WINDOW_MS;
      await this.ctx.storage.put(
        "throttle",
        fresh ? { fails: 1, windowStart: Date.now() } : { ...cur, fails: cur.fails + 1 }
      );
      return { status: 401, body: { error: "bad-credentials" } };
    }
    await this.ctx.storage.delete("throttle");
    return { status: 200, body: { username, token: await this.issueToken(username) } };
  }

  async logout(secret) {
    await this.ctx.storage.delete("tok:" + (await sha256Hex(secret)));
    return { status: 200, body: { ok: true } };
  }

  // Merge `doc` (if given) into the account's document; answer the result.
  async sync(secret, doc) {
    if (!(await this.checkToken(secret))) {
      return { status: 401, body: { error: "unauthorized" } };
    }
    const stored = (await this.ctx.storage.get("doc")) || emptyDoc();
    if (doc === null || doc === undefined) return { status: 200, body: { doc: stored } };
    const incoming = normalizeDoc(doc, Date.now());
    if (!incoming) return { status: 400, body: { error: "bad-doc" } };
    const merged = mergeDocs(stored, incoming);
    if (JSON.stringify(merged) !== JSON.stringify(stored)) {
      await this.ctx.storage.put("doc", merged);
    }
    return { status: 200, body: { doc: merged } };
  }

  async issueToken(username) {
    const secret = b64url(randomBytes(32));
    const now = Date.now();
    await this.ctx.storage.put("tok:" + (await sha256Hex(secret)), {
      createdAt: now,
      lastUsedAt: now,
    });
    const tokens = await this.ctx.storage.list({ prefix: "tok:" });
    if (tokens.size > MAX_TOKENS) {
      const oldest = [...tokens.entries()]
        .sort((a, b) => a[1].lastUsedAt - b[1].lastUsedAt)
        .slice(0, tokens.size - MAX_TOKENS)
        .map(([k]) => k);
      await this.ctx.storage.delete(oldest);
    }
    return `${TOKEN_PREFIX}.${username}.${secret}`;
  }

  async checkToken(secret) {
    const key = "tok:" + (await sha256Hex(secret));
    const rec = await this.ctx.storage.get(key);
    if (!rec) return false;
    const now = Date.now();
    if (now - rec.lastUsedAt > TOKEN_IDLE_MS) {
      await this.ctx.storage.delete(key);
      return false;
    }
    if (now - rec.lastUsedAt > TOKEN_TOUCH_MS) {
      await this.ctx.storage.put(key, { ...rec, lastUsedAt: now });
    }
    return true;
  }
}

// ---- routing ----------------------------------------------------------------

async function readJson(request) {
  const len = Number(request.headers.get("content-length") || 0);
  if (len > MAX_BODY) return { error: json(413, { error: "too-large" }) };
  const text = await request.text();
  if (text.length > MAX_BODY) return { error: json(413, { error: "too-large" }) };
  if (!text) return { body: {} };
  try {
    const body = JSON.parse(text);
    if (typeof body !== "object" || body === null || Array.isArray(body)) {
      return { error: json(400, { error: "bad-json" }) };
    }
    return { body };
  } catch {
    return { error: json(400, { error: "bad-json" }) };
  }
}

function store(env, username) {
  return env.ACCOUNTS.get(env.ACCOUNTS.idFromName("user:" + username));
}

// Per-IP limit on password attempts and sign-ups (Workers Rate Limiting). The
// binding is optional: without it only the per-account throttle applies.
async function limited(env, request) {
  if (!env.AUTH_LIMITER) return false;
  const ip = request.headers.get("cf-connecting-ip") || "unknown";
  try {
    const { success } = await env.AUTH_LIMITER.limit({ key: ip });
    return !success;
  } catch {
    return false;
  }
}

export async function handleApi(request, env, url) {
  if (!env.ACCOUNTS) return json(404, { error: "not-enabled" });
  if (request.method !== "POST") return json(405, { error: "method" });
  const { body, error } = await readJson(request);
  if (error) return error;

  const path = url.pathname;
  if (path === "/api/account/signup" || path === "/api/account/login") {
    const signup = path.endsWith("/signup");
    if (await limited(env, request)) return json(429, { error: "too-many-attempts" });
    const username = normalizeUsername(body.username);
    if (!username) {
      return json(signup ? 400 : 401, { error: signup ? "invalid-username" : "bad-credentials" });
    }
    const pw = body.password;
    if (typeof pw !== "string" || pw.length > MAX_PASSWORD) {
      return json(signup ? 400 : 401, { error: signup ? "weak-password" : "bad-credentials" });
    }
    if (signup && pw.length < MIN_PASSWORD) return json(400, { error: "weak-password" });
    const s = store(env, username);
    const r = signup ? await s.signup(username, pw) : await s.login(username, pw);
    return json(r.status, r.body);
  }

  if (path === "/api/sync" || path === "/api/account/logout") {
    const auth = parseToken(request.headers.get("authorization"));
    if (!auth) return json(401, { error: "unauthorized" });
    const s = store(env, auth.username);
    const r =
      path === "/api/sync" ? await s.sync(auth.secret, body.doc) : await s.logout(auth.secret);
    return json(r.status, r.body);
  }

  return json(404, { error: "not-found" });
}
