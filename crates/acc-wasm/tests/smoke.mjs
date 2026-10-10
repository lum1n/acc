// Drives a recorded Claude Code session through the real wasm build.
//
//   cargo build -p acc-wasm --release --target wasm32-unknown-unknown
//   wasm-bindgen --target nodejs --out-dir bindings/node \
//     target/wasm32-unknown-unknown/release/acc_wasm.wasm
//   node crates/acc-wasm/tests/smoke.mjs bindings/node

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";

const require = createRequire(import.meta.url);
const acc = require(resolve(process.argv[2], "acc_wasm.js"));
const root = new URL("../../../", import.meta.url);
const fixture = readFileSync(new URL("crates/acc-core/tests/fixtures/claude/permission-deny.ndjson", root));

assert.ok(acc.version());
assert.deepEqual(acc.harnesses().map((h) => h.harness), ["claude", "codex", "cursor", "copilot", "pi", "opencode"]);

const session = new acc.Session({ harness: "claude", cwd: "/home/v/proj" });
assert.equal(session.harness, "claude");
const spec = session.launchSpec();
assert.equal(spec.program, "claude");
assert.equal(spec.protocol, "claude_stream_json");
assert.ok(new TextDecoder().decode(session.start()).includes("initialize"));

const events = [];
let answered = null;
let now = 1000;
let start = 0;
// Uneven chunks, as a socket would deliver them.
for (let end = 0; start < fixture.length; start = end) {
  end = Math.min(fixture.length, start + 997);
  const got = session.receive(fixture.subarray(start, end), ++now);
  assert.ok(got.transmit instanceof Uint8Array);
  events.push(...got.events);
  for (const e of got.events) {
    if (e.type === "permission_requested" && !answered) {
      const deny = e.options.find((o) => /deny|reject/i.test(o.id));
      answered = session.send({ type: "answer_permission", request_id: e.request_id, option_id: deny.id });
      assert.ok(new TextDecoder().decode(answered).includes(e.request_id));
    }
  }
}

const kinds = new Set(events.map((e) => e.type));
for (const k of ["session_started", "state_changed", "permission_requested", "item_delta", "turn_completed", "usage"]) {
  assert.ok(kinds.has(k), `no ${k} event in ${[...kinds]}`);
}
assert.ok(answered);
assert.equal(session.nativeId, "5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11");
assert.ok(session.commands().length > 0);
assert.ok(session.options().some((o) => o.id === "permission"));
const state = session.state();
assert.equal(state.source, "native");
assert.ok(state.observed_at_ms > 1000);

assert.throws(() => session.send({ type: "answer_permission", request_id: "nope", option_id: "x" }), /unknown permission request/);
assert.throws(() => session.send({ type: "bogus" }), /invalid command/);
assert.throws(() => new acc.Session({ harness: "nope", cwd: "/" }), /invalid session config/);

assert.equal(session.observe({ state: { state: "blocked", kind: "question" }, message: null, source: "native", observed_at_ms: now + 1 }), true);
assert.deepEqual(session.state().state, { state: "blocked", kind: "question" });

const scanner = new acc.Osc7501Scanner();
const [status] = scanner.feed(new TextEncoder().encode("\x1b]7501;state=working:progress=40\x07"));
assert.deepEqual(status.state, { state: "working", progress: 40 });

const detached = new acc.DetachedSession("s1");
assert.ok(detached.launchScript(spec, "/tmp").includes("mkfifo"));
assert.ok(detached.attachScript(10).includes("tail -c +11 -f"));
assert.equal(acc.shellQuote("a b"), "'a b'");

console.log(`ok: ${events.length} events through wasm`);
