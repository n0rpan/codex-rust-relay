// Reducer for Rust's versioned host projection. A reconnect without retained state starts with a snapshot.
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Server} Server */
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Scope} Scope */
export const scopeKey = (s) =>
  JSON.stringify([s.project, s.route.runtime, s.route.billing]);
export const emptyScope = (scope) => ({
  scope,
  connection: "connecting",
  capabilities: [],
  sessions: [],
  asks: [],
  transcripts: Object.create(null),
  detail: {},
});
const sessionKey = (scope, id) => scopeKey(scope) + " " + id;
/**
 * The recorded parent of a session, from the immutable launch stamp when there
 * is one. The stamp carries the parent's full scope, so a worker in another
 * checkout or billing route still points at the conversation that started it.
 * @param {{scope: Scope, session: any}} entry
 * @returns {{project: string, scope: Scope|null, id: string}|null}
 */
export function sessionParent(entry) {
  const origin = entry?.session?.detail?.origin;
  if (origin && origin.relation === "handoff") return null;
  const parent = origin?.parent;
  if (parent?.session)
    return {
      project: parent.project ?? entry.scope.project,
      scope: parent.scope ?? null,
      id: parent.session,
    };
  if (entry?.session?.parent)
    return { project: entry.scope.project, scope: entry.scope, id: entry.session.parent };
  return null;
}
/**
 * Group catalog entries into a worker tree. A parent may live in another
 * project or route; a filter that hides it keeps the child as a root with a
 * navigable recorded parent instead of dropping the relationship.
 * @param {Array<{scope: Scope, session: any}>} entries
 */
export function catalogTree(entries) {
  const byKey = new Map(entries.map((entry) => [sessionKey(entry.scope, entry.session.id), entry]));
  const links = new Map();
  const children = new Map();
  const roots = [];
  for (const entry of entries) {
    const own = sessionKey(entry.scope, entry.session.id);
    const recorded = sessionParent(entry);
    let target = null;
    if (recorded) {
      let scope = recorded.scope;
      if (!scope) {
        // A terminal parent has no host scope. Its native id still matches a
        // catalog row in the recorded project.
        const hit =
          entries.find(
            (candidate) =>
              candidate.session.id === recorded.id && candidate.scope.project === recorded.project,
          ) ?? entries.find((candidate) => candidate.session.id === recorded.id);
        scope = hit?.scope ?? null;
      }
      if (scope) {
        const key = sessionKey(scope, recorded.id);
        target = {
          key,
          scope,
          project: scope.project,
          id: recorded.id,
          title: byKey.get(key)?.session.title ?? null,
        };
        links.set(own, target);
      }
    }
    if (target && target.key !== own && byKey.has(target.key)) {
      const group = children.get(target.key) ?? [];
      group.push(entry);
      children.set(target.key, group);
    } else {
      roots.push(entry);
    }
  }
  return { roots, children, links, byKey };
}
const itemKey = (i) => JSON.stringify([i.turn, i.id]);
const safeMap = (entries) => Object.assign(Object.create(null), entries);
export class Projection {
  constructor(global = false) {
    this.global = global;
    this.scopes = new Map();
    this.cursor = null;
  }
  /** @param {Server} event */
  apply(event) {
    if (event.type === "snapshot") {
      this.scopes.clear();
      for (const scope of event.scopes) {
        scope.transcripts = this.global
          ? Object.create(null)
          : safeMap(scope.transcripts);
        this.scopes.set(scopeKey(scope.scope), scope);
      }
      this.cursor = event.cursor;
      return true;
    }
    if (event.type !== "delta") return false;
    if (this.cursor && event.cursor.epoch !== this.cursor.epoch)
      throw new Error("Host restarted");
    if (this.cursor && event.cursor.seq <= this.cursor.seq) return false;
    const key = scopeKey(event.scope);
    const s = this.scopes.get(key) ?? emptyScope(event.scope);
    this.scopes.set(key, s);
    let dirty = false;
    for (const c of event.changes) {
      switch (c.kind) {
        case "state":
          Object.assign(s, {
            connection: c.connection,
            capabilities: c.capabilities,
            detail: c.detail,
          });
          dirty = true;
          break;
        case "asks":
          s.asks = c.asks;
          dirty = true;
          break;
        case "sessions":
          s.sessions = c.sessions;
          dirty = true;
          break;
        case "session": {
          const i = s.sessions.findIndex((x) => x.id === c.session.id);
          if (i < 0) s.sessions.push(c.session);
          else s.sessions[i] = c.session;
          dirty = true;
          break;
        }
        case "retired":
          delete s.transcripts[c.thread];
          dirty = true;
          break;
        case "transcript":
          if (!this.global) {
            s.transcripts[c.thread] = c.timeline;
            dirty = true;
          }
          break;
        case "timeline_state":
          if (!this.global) {
            (s.transcripts[c.thread] ??= { items: [], detail: {} }).detail =
              c.detail;
            dirty = true;
          }
          break;
        case "items":
          if (!this.global) {
            const t = (s.transcripts[c.thread] ??= { items: [], detail: {} });
            const ix = new Map(t.items.map((v, i) => [itemKey(v), i]));
            for (const value of c.items) {
              const k = itemKey(value),
                i = ix.get(k);
              if (i === undefined) {
                ix.set(k, t.items.length);
                t.items.push(value);
              } else if (value.revision >= t.items[i].revision)
                t.items[i] = value;
            }
            dirty = true;
          }
          break;
        case "append":
          if (!this.global) {
            const row = s.transcripts[c.thread]?.items.findLast(
              (x) => x.turn === c.turn && x.id === c.id,
            );
            if (!row) throw new Error("Missing transcript item");
            if (c.revision > row.revision) {
              row.text += c.text;
              row.revision = c.revision;
              dirty = true;
            }
          }
          break;
      }
    }
    this.cursor = event.cursor;
    return dirty;
  }
}
