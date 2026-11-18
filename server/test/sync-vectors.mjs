// The shared merge cases (native/crates/protocol/testdata/sync-vectors.json),
// run against the server's merge (src/sync.js). The Rust side runs the same
// file in its unit tests, so the two merges cannot drift apart unnoticed.
//
//   npm run test:vectors

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { mergeDocs, normalizeDoc } from "../src/sync.js";

const file = new URL("../../native/crates/protocol/testdata/sync-vectors.json", import.meta.url);
const { now, cases } = JSON.parse(readFileSync(file, "utf8"));

let failed = 0;
for (const c of cases) {
  try {
    const a = normalizeDoc(c.a, now);
    const b = normalizeDoc(c.b, now);
    assert.deepEqual(mergeDocs(a, b), c.merged, "a then b");
    assert.deepEqual(mergeDocs(b, a), c.merged, "b then a");
    // Idempotent: merging the result again changes nothing.
    assert.deepEqual(mergeDocs(c.merged, a), c.merged, "merged again");
    console.log("ok   " + c.name);
  } catch (e) {
    failed++;
    console.log("FAIL " + c.name + "\n     " + (e && e.message));
  }
}
console.log(failed ? `${failed} failed` : `all ${cases.length} passed`);
process.exit(failed ? 1 : 0);
