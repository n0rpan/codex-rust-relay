import { readFileSync } from "node:fs";
import { test } from "node:test";
import assert from "node:assert/strict";
const source = readFileSync(new URL("../assets/pair-camera.js", import.meta.url), "utf8");
const { pairingInvitation } = await import(
  "data:text/javascript;base64," + Buffer.from(source).toString("base64")
);
const origin = "https://studio.fixture.ts.net";
const code = "x".repeat(43);
const thread = "claude:subscription:session~" + "a".repeat(43);

test("pairing preserves a scoped session target and accepts legacy invites", () => {
  assert.deepEqual(pairingInvitation(origin + "/#pair=" + code, origin), {code, thread: null});
  const url = origin + "/#" + new URLSearchParams({pair: code, thread});
  assert.deepEqual(pairingInvitation(url, origin), {code, thread});
  assert.throws(() => pairingInvitation(url, "https://other.fixture.ts.net"));
  assert.throws(() => pairingInvitation(url + "&pair=" + code, origin));
  assert.throws(() => pairingInvitation(origin + "/#pair=" + code + "&thread=ambiguous", origin));
});
