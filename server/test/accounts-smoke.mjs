// End-to-end check of the accounts + sync API against a running server.
//
//   npm run dev                      # in another terminal (wrangler dev :8787)
//   npm run test:accounts            # or: node test/accounts-smoke.mjs <base-url>
//
// Uses a fresh random username each run, so it can be repeated against the
// same local state. Do not point it at production: it creates accounts.

import assert from "node:assert/strict";

const BASE = process.argv[2] || "http://127.0.0.1:8787";
const user = "t" + Math.random().toString(36).slice(2, 12);
const X = "0f8fad5b-d9cb-469f-a165-70867728950e";
const Y = "7c9e6679-7425-40de-944b-e07fc1f90ae7";

async function call(path, body, token) {
  const headers = { "content-type": "application/json" };
  if (token) headers.authorization = `Bearer ${token}`;
  const r = await fetch(BASE + path, { method: "POST", headers, body: JSON.stringify(body) });
  return { status: r.status, body: await r.json() };
}

const steps = [];
const step = (name, fn) => steps.push([name, fn]);
let tokA, tokB;

step("sign-up validates the username and password", async () => {
  let r = await call("/api/account/signup", { username: "x", password: "longenough" });
  assert.deepEqual([r.status, r.body.error], [400, "invalid-username"]);
  r = await call("/api/account/signup", { username: user, password: "short" });
  assert.deepEqual([r.status, r.body.error], [400, "weak-password"]);
});

step("sign-up returns a token; the name is then taken", async () => {
  const r = await call("/api/account/signup", { username: user.toUpperCase(), password: "correct horse" });
  assert.equal(r.status, 200);
  assert.equal(r.body.username, user);
  assert.match(r.body.token, new RegExp(`^sct1\\.${user}\\.[A-Za-z0-9_-]{43}$`));
  tokA = r.body.token;
  const again = await call("/api/account/signup", { username: user, password: "whatever123" });
  assert.deepEqual([again.status, again.body.error], [409, "username-taken"]);
});

step("sign-in checks the password", async () => {
  let r = await call("/api/account/login", { username: user, password: "wrong password" });
  assert.deepEqual([r.status, r.body.error], [401, "bad-credentials"]);
  r = await call("/api/account/login", { username: "nobody-" + user, password: "correct horse" });
  assert.deepEqual([r.status, r.body.error], [401, "bad-credentials"]);
  r = await call("/api/account/login", { username: ` ${user} `, password: "correct horse" });
  assert.equal(r.status, 200);
  tokB = r.body.token;
  assert.notEqual(tokA, tokB);
});

step("sync without a valid token is refused", async () => {
  let r = await call("/api/sync", { doc: {} });
  assert.equal(r.status, 401);
  r = await call("/api/sync", { doc: {} }, tokA.slice(0, -1) + (tokA.endsWith("A") ? "B" : "A"));
  assert.equal(r.status, 401);
});

step("two devices merge: newer name wins, uses take the later time", async () => {
  const t = Date.now() - 10_000;
  let r = await call(
    "/api/sync",
    {
      doc: {
        v: 1,
        entries: { [X]: { name: "Office", nameAt: t, usedAt: t }, "not-a-uuid": { name: "x" } },
        recentsClearedAt: 0,
      },
    },
    tokA
  );
  assert.equal(r.status, 200);
  assert.deepEqual(Object.keys(r.body.doc.entries), [X]);

  r = await call(
    "/api/sync",
    {
      doc: {
        entries: {
          [X.toUpperCase()]: { name: "  Work \n PC ", nameAt: t + 1000, usedAt: t - 5 },
          [Y]: { name: "", nameAt: 0, usedAt: t + 2000 },
        },
      },
    },
    tokB
  );
  assert.equal(r.status, 200);
  assert.deepEqual(r.body.doc.entries[X], { name: "Work PC", nameAt: t + 1000, usedAt: t });
  assert.deepEqual(r.body.doc.entries[Y], { name: "", nameAt: 0, usedAt: t + 2000 });

  // Device A sees B's changes on its next sync; a stale copy changes nothing.
  r = await call(
    "/api/sync",
    { doc: { entries: { [X]: { name: "Office", nameAt: t, usedAt: t } } } },
    tokA
  );
  assert.equal(r.body.doc.entries[X].name, "Work PC");
});

step("a removed name beats the older name; clears are kept", async () => {
  const t = Date.now() - 1000;
  const r = await call(
    "/api/sync",
    { doc: { entries: { [X]: { name: "", nameAt: t, usedAt: 0 } }, recentsClearedAt: t } },
    tokA
  );
  assert.equal(r.body.doc.entries[X].name, "");
  assert.equal(r.body.doc.recentsClearedAt, t);
});

step("future times are clamped to the server's clock", async () => {
  const r = await call(
    "/api/sync",
    { doc: { entries: { [Y]: { name: "Future", nameAt: Date.now() + 1e9 } } } },
    tokA
  );
  assert.ok(r.body.doc.entries[Y].nameAt <= Date.now());
});

step("sync without a doc just reads", async () => {
  const r = await call("/api/sync", {}, tokB);
  assert.equal(r.status, 200);
  assert.equal(r.body.doc.entries[Y].name, "Future");
});

step("bad bodies are refused", async () => {
  let r = await call("/api/sync", { doc: [] }, tokA);
  assert.deepEqual([r.status, r.body.error], [400, "bad-doc"]);
  const big = await fetch(BASE + "/api/sync", {
    method: "POST",
    headers: { authorization: `Bearer ${tokA}`, "content-type": "application/json" },
    body: "x".repeat(300_000),
  });
  assert.equal(big.status, 413);
  r = await fetch(BASE + "/api/sync", { method: "GET" });
  assert.equal(r.status, 405);
});

step("sign-out revokes only that device's token", async () => {
  let r = await call("/api/account/logout", {}, tokB);
  assert.equal(r.status, 200);
  r = await call("/api/sync", {}, tokB);
  assert.equal(r.status, 401);
  r = await call("/api/sync", {}, tokA);
  assert.equal(r.status, 200);
});

step("repeated wrong passwords lock sign-in for a while", async () => {
  let last;
  for (let i = 0; i < 11; i++) {
    last = await call("/api/account/login", { username: user, password: "nope nope " + i });
  }
  assert.deepEqual([last.status, last.body.error], [429, "too-many-attempts"]);
  const right = await call("/api/account/login", { username: user, password: "correct horse" });
  assert.equal(right.status, 429);
});

let failed = 0;
for (const [name, fn] of steps) {
  try {
    await fn();
    console.log("ok   " + name);
  } catch (e) {
    failed++;
    console.log("FAIL " + name + "\n     " + (e && e.message));
  }
}
console.log(failed ? `${failed} failed` : `all ${steps.length} passed`);
process.exit(failed ? 1 : 0);
