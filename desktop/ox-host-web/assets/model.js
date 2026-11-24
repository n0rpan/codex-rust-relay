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
