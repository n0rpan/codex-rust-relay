// Verifies the phone activity line detail extractor for long tool chains and thinking turns.
import { readFileSync } from "node:fs";
import { test } from "node:test";
import assert from "node:assert/strict";

const source = readFileSync(new URL("../assets/activity.js", import.meta.url), "utf8");
const { sceneFor, detailFor, LABELS } = await import(
  "data:text/javascript;base64," + Buffer.from(source).toString("base64")
);

test("activity line reports the latest tool action even between completed tool calls", () => {
  const items = [
    { turn: "t1", id: "u1", role: "user", text: "Fix mobile folders", detail: {} },
    {
      turn: "t1",
      id: "c1",
      role: "tool",
      text: "",
      detail: {
        item: {
          CommandExecution: {
            command: "/bin/bash -lc 'rg -n catalog desktop/ox-host-web/assets/app.js'",
            status: "completed",
          },
        },
      },
    },
  ];
  assert.equal(sceneFor(items, true, false), "thinking");
  assert.equal(
    detailFor(items, true, false),
    "rg -n catalog desktop/ox-host-web/assets/app.js",
  );
  assert.equal(LABELS.thinking, "Thinking");
});

test("activity line reports file changes and live shell commands", () => {
  const items = [
    {
      turn: "t2",
      id: "f1",
      role: "tool",
      text: "",
      detail: {
        item: {
          FileChange: {
            status: "inProgress",
            changes: [{ path: "desktop/ox-host-web/assets/settings.js", diff: "+1\n" }],
          },
        },
      },
    },
  ];
  assert.equal(sceneFor(items, true, false), "building");
  assert.equal(
    detailFor(items, true, false),
    "desktop/ox-host-web/assets/settings.js",
  );
});
