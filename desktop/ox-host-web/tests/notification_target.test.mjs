// Exercise the phone's notification resolver against progressively discovered scopes.
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const source = readFileSync(new URL("../assets/model.js", import.meta.url));
const { NotificationTarget } = await import("data:text/javascript;base64," + source.toString("base64"));
const row = (project, runtime, billing, id) => ({
  scope: { project, route: { runtime, billing } },
  session: { id },
});
const first = row("/work/one", "codex", "subscription", "duplicate");
const second = row("/work/two", "claude", { api: "demo/alias" }, "duplicate");
function link({ scope: { project, route }, session: { id } }) {
  const encoded = JSON.stringify([project, route.runtime, route.billing.api ?? null, id]);
  return id + "~" + createHash("sha256").update(encoded).digest("base64url");
}

test("a scoped link survives a staggered duplicate and resolves once after discovery", async () => {
  const target = new NotificationTarget();
  const done = { complete: true };
  const shown = [];
  target.request(link(second));
  await target.resolve([first], null, (value) => shown.push(value));
  assert.deepEqual(shown, []);
  await Promise.all([1, 2].map(() =>
    target.resolve([first, second], done, (value) => shown.push(value))));
  assert.deepEqual(shown, [second]);
});

test("a wrong or missing scoped link never falls back to a matching native id", async () => {
  const target = new NotificationTarget();
  for (const ref of [link(second), "duplicate~broken", "missing"]) {
    let result;
    target.request(ref);
    await target.resolve([first], { complete: true }, (value) => { result = value; });
    assert.equal(result, "missing");
  }
});

test("bare links require a unique result across the finished discovery", async () => {
  const target = new NotificationTarget();
  for (const [rows, complete, expected] of [
    [[first], true, first],
    [[first, second], true, "ambiguous"],
    [[first], false, "incomplete"],
    [[], true, "missing"],
  ]) {
    const shown = [];
    target.request("duplicate");
    await target.resolve([first], null, (value) => shown.push(value));
    await target.resolve(rows, { complete }, (value) => shown.push(value));
    assert.deepEqual(shown, [expected]);
  }
  target.request(link(first));
  await target.resolve([first], { complete: false }, (value) => assert.equal(value, first));
});

test("manual navigation, a newer tap, and a reconnect invalidate an unfinished digest", async () => {
  const target = new NotificationTarget();
  const shown = [];
  const show = (value) => shown.push(value);
  for (const interrupt of [() => target.request(null), () => target.request(link(second)), () => target.reconnect()]) {
    target.request(link(first));
    const inFlight = target.resolve([first, second], { complete: true }, show);
    interrupt();
    await inFlight;
    assert.deepEqual(shown, []);
  }
  await target.resolve([first, second], { complete: true }, show);
  assert.deepEqual(shown, [first]);
});
