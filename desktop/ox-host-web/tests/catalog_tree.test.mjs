// Runs the phone sidebar's tree reducer without a browser. The asset stays an
// ES module for the browser; a data URL lets Node evaluate the same source.
import { readFileSync } from "node:fs";
import { test } from "node:test";
import assert from "node:assert/strict";

const source = readFileSync(new URL("../assets/model.js", import.meta.url), "utf8");
const { catalogTree, sessionParent, scopeKey } = await import(
  "data:text/javascript;base64," + Buffer.from(source).toString("base64")
);

const scope = (project, runtime = "codex", billing = "subscription") => ({
  project,
  route: { runtime, billing },
});
const entry = (project, id, detail = {}, parent = null) => ({
  scope: scope(project),
  session: { id, title: id, parent, detail },
});
const keyOf = (scope, id) => scopeKey(scope) + " " + id;
const worker = (parentScope, parentId, extra = {}) => ({
  worker: true,
  origin: {
    relation: "worker",
    job_id: 7,
    ceiling: "read",
    parent: { project: parentScope.project, scope: parentScope, session: parentId },
  },
  ...extra,
});

test("a worker nests under its recorded parent in another project and route", () => {
  const parent = entry("/work/app", "parent");
  const childScope = scope("/work/review", "codex", { api: "fixture/demo" });
  const child = {
    scope: childScope,
    session: {
      id: "child",
      title: "child",
      parent: null,
      detail: worker(parent.scope, "parent"),
    },
  };
  const tree = catalogTree([child, parent]);
  const parentKey = keyOf(parent.scope, "parent");
  assert.deepEqual(tree.children.get(parentKey)?.map((e) => e.session.id), ["child"]);
  assert.equal(tree.roots.length, 1);
  assert.equal(tree.roots[0].session.id, "parent");
  assert.equal(tree.links.get(keyOf(childScope, "child")).project, "/work/app");
});

test("a filtered-out parent leaves the worker navigable instead of nesting it", () => {
  const parentScope = scope("/work/app");
  const child = entry("/work/review", "child", worker(parentScope, "parent"));
  const tree = catalogTree([child]);
  assert.equal(tree.roots.length, 1);
  assert.equal(tree.roots[0].session.id, "child");
  const link = tree.links.get(keyOf(child.scope, "child"));
  assert.ok(link);
  assert.equal(link.id, "parent");
  assert.equal(link.project, "/work/app");
});

test("a terminal parent resolves by its recorded project and native id", () => {
  const parentScope = scope("/work/app");
  const parent = entry("/work/app", "native-7");
  const detail = worker(parentScope, "native-7");
  detail.origin.parent.scope = null;
  const child = entry("/work/app", "child", detail);
  const tree = catalogTree([parent, child]);
  assert.equal(tree.roots.length, 1);
  assert.equal(tree.roots[0].session.id, "native-7");
  assert.deepEqual(
    tree.children.get(keyOf(parent.scope, "native-7"))?.map((e) => e.session.id),
    ["child"],
  );
});

test("a handoff keeps its own row rather than claiming a worker parent", () => {
  const parent = entry("/work/app", "parent");
  const target = entry(
    "/work/app",
    "target",
    { origin: { relation: "handoff", parent: { project: "/work/app", scope: parent.scope, session: "parent" } } },
    "parent",
  );
  const tree = catalogTree([parent, target]);
  assert.equal(sessionParent(target), null);
  assert.equal(tree.roots.length, 2);
  assert.equal(tree.children.size, 0);
});

test("a native subagent with only a parent id still nests in its project", () => {
  const parent = entry("/work/app", "parent");
  const child = entry("/work/app", "child", {}, "parent");
  const tree = catalogTree([parent, child]);
  assert.deepEqual(
    tree.children.get(keyOf(parent.scope, "parent"))?.map((e) => e.session.id),
    ["child"],
  );
});
