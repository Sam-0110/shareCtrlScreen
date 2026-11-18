// Accounts + remote-ID sync (HTTP, under /api). Separate from signaling: the
// SignalingRoom never sees any of this, and accounts are not needed to connect.
//
// One AccountStore Durable Object per username holds that account's password
// hash, its sign-in tokens (hashes only) and its synced document. The document
// is the same last-writer-wins value the app merges; its rules live in sync.js,
// the mirror of native/crates/protocol/src/sync/ (names and recents per remote
// ID, and since server 1.2.0 the open tabs and the preferences that follow the
// account).
//
//   POST /api/account/signup  {username, password}  → {username, token}
//   POST /api/account/login   {username, password}  → {username, token}
//   POST /api/account/logout  (Bearer)              → {ok: true}
//   POST /api/sync            (Bearer) {doc?}       → {doc, features, serverTime}
//
// Errors are {error: "<code>"} with a 4xx status.

import { DurableObject } from "cloudflare:workers";
import { FEATURES, emptyDoc, mergeDocs, normalizeDoc, normalizeIncoming } from "./sync.js";

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

  // Merge `doc` (if given) into the account's document; answer the result,
  // what this server stores (`features`) and its clock (`serverTime`, for the
  // client's stamps).
  async sync(secret, doc) {
    if (!(await this.checkToken(secret))) {
      return { status: 401, body: { error: "unauthorized" } };
    }
    const now = Date.now();
    const raw = await this.ctx.storage.get("doc");
    // The stored document is cleaned too: that upgrades a version 1 document to
    // the version 2 shape, and forgets tabs closed long ago (sync.js).
    const stored = normalizeDoc(raw || emptyDoc(), now) || emptyDoc();
    const reply = (merged) => ({
      status: 200,
      body: { doc: merged, features: FEATURES, serverTime: now },
    });
    if (doc === null || doc === undefined) return reply(stored);
    const incoming = normalizeIncoming(doc, now);
    if (!incoming) return { status: 400, body: { error: "bad-doc" } };
    if (incoming.tooLarge) return { status: 413, body: { error: "too-large" } };
    const merged = mergeDocs(stored, incoming.doc);
    if (JSON.stringify(merged) !== JSON.stringify(raw)) {
      await this.ctx.storage.put("doc", merged);
    }
    return reply(merged);
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
