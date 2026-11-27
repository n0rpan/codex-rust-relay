import { openPhoneTerminal } from "./operator.js";
import {
  copyButton,
  formatPatchNode,
  messageText,
  showReview,
  imagesFor,
  clearImages,
  moveImages,
  setComposerOwner,
  syncDraftHeight,
  updateExtras,
  mentionInput,
  forgetComposer,
} from "./composer-tools.js";
import { pairingCode, setupCamera } from "./pair-camera.js";
import { ActivityLine } from "./activity.js";
import * as wire from "./wire.js";
import { SessionControls } from "./controls.js";
import { setupSettings } from "./settings.js";
import {
  scopeKey,
  Projection,
  NotificationTarget,
  emptyScope,
  catalogTree,
  catalogParentResolver,
} from "./model.js";
import {
  deviceKey,
  pair,
  proof,
  notifications,
  settingsRequest,
} from "./device.js";
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Client} Client */
/** @typedef {import('../../ox-host-protocol/generated/host-v1').Server} Server */
const $ = (id) => document.getElementById(id);
const el = (tag, cls, text) => {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined) node.textContent = text;
  return node;
};
const button = (text, fn) => {
  const b = el("button", "", text);
  b.type = "button";
  b.onclick = fn;
  return b;
};
// Same wording as the desktop's Route::label (ox-desk-core runtime.rs).
const routesLabel = (r) =>
  r.billing === "subscription"
    ? r.runtime === "codex"
      ? "ChatGPT subscription"
      : "Claude subscription"
    : r.runtime.replace(/^./, (x) => x.toUpperCase()) + " · " + r.billing.api;
// Desktop status words (views/host/discovery.rs route_status, views/host.rs).
// Same wording as the desktop (ox-desk-core store/claude/native.rs); `app` is the host's
// `detail.native_live`: the stock client that has the session open.
const nativeLiveText = (app) =>
  `This conversation is running in ${app} right now. OX shows it read-only so the two don't collide. When it's closed there, you can continue it here.`;
const statusText = (session, needsInput) =>
  session.capabilities.includes("native_live")
    ? `Live in ${session.detail?.native_live ?? "Claude Code"}`
    : needsInput
      ? "Needs you"
      : session.capabilities.includes("ended")
        ? "Ended"
        : ({
            running: "Running",
            needs_input: "Needs you",
            failed: "Error",
            interrupted: "Interrupted",
          })[session.status] ?? "";
const leaf = (path) => path.split("/").filter(Boolean).pop() || path;
const keyOf = (scope, id) => scopeKey(scope) + " " + id;
// The model the runtime itself answered with, published beside the launch stamp.
const actualModel = (session) =>
  session?.detail?.worker_model?.actual_model ??
  session?.detail?.worker_marks?.actual_model ??
  null;
function notice(s = "") {
  $("message").hidden = !s;
  $("message").textContent = s;
}
function closeActionMenus(except) {
  for (const menu of document.querySelectorAll(".action-menu[open]")) {
    if (menu !== except) menu.open = false;
  }
}
document.addEventListener("click", (event) => {
  closeActionMenus(
    event.target.closest(".action-menu > summary")?.parentElement,
  );
});
document.addEventListener("keydown", (event) => {
  const open = document.querySelector(".action-menu[open]");
  if (event.key === "Escape" && open) {
    closeActionMenus();
    open.querySelector("summary").focus();
  }
});
function page(id) {
  if (id !== "catalog" && id !== "pair") notificationTarget.request(null);
  if (id !== "settings") phoneSettings?.reset?.();
  if (id !== "running") stopRunning();
  clearConfirm($("confirm-slot"));
  clearConfirm($("running-confirm"));
  const returningToCatalog = id === "catalog" && $("catalog").hidden;
  for (const view of [
    "pair",
    "catalog",
    "create",
    "conversation",
    "running",
    "settings",
  ])
    $(view).hidden = view !== id;
  if (returningToCatalog) {
    // Move focus out of the outgoing composer before iOS collapses the keyboard.
    $("sessions-heading").focus({ preventScroll: true });
    window.scrollTo({ top: 0, behavior: "instant" });
  }
  if (id !== "pair" && !$("camera-panel").hidden) closeCamera();
  $("location").textContent = location.hostname.split(".")[0];
  $("location").title = location.host;
}
const hash = new URLSearchParams(location.hash.slice(1));
const notificationTarget = new NotificationTarget();
notificationTarget.request(hash.get("thread"));
let code = hash.get("pair");
if (location.hash) history.replaceState(null, "", location.pathname);
// Whether the host has an OX API provider: then every catalog alias is a route for both
// runtimes, even when the first Hello (before discovery finished) listed none.
let apiProvider = false;
let key,
  globalLink,
  detailLink,
  selected = null,
  filter = "all",
  routes = [],
  projects = [],
  scheduled = false;
let runningTimer = null,
  processes = null,
  endBusy = false,
  endChecking = false;
let cold = new Map(),
  drafts = new Map(),
  timelineRows = new Map(),
  askKeys = "",
  pendingNew = null,
  handoffStamp = "",
  // Message and images sent from a composer whose Ack has not come back, by owner.
  unacked = new Map(),
  // The last `restored_draft.message` applied per owner, so each host restore lands once.
  restoredSeen = new Map(),
  foldedProjects = new Set(),
  pinnedThreads = new Map(),
  threadColors = new Map(),
  visibleRows = 200;

const SIDEBAR_META_KEY = "ox-phone-sidebar-v1";
const ACCENT_COLORS = ["cobalt", "amber", "emerald", "coral", "violet", "cyan"];
function loadSidebarMeta() {
  try {
    const raw = localStorage.getItem(SIDEBAR_META_KEY);
    if (!raw) return;
    const parsed = JSON.parse(raw);
    for (const [k, ts] of Object.entries(parsed.pinned || {}))
      if (typeof ts === "number") pinnedThreads.set(k, ts);
    for (const [k, c] of Object.entries(parsed.colors || {}))
      if (ACCENT_COLORS.includes(c)) threadColors.set(k, c);
    for (const p of parsed.foldedProjects || [])
      if (typeof p === "string") foldedProjects.add(p);
  } catch {}
}
function saveSidebarMeta() {
  try {
    localStorage.setItem(
      SIDEBAR_META_KEY,
      JSON.stringify({
        pinned: Object.fromEntries(pinnedThreads),
        colors: Object.fromEntries(threadColors),
        foldedProjects: [...foldedProjects],
      }),
    );
  } catch {}
}
loadSidebarMeta();
const global = new Projection(true);
const controls = new SessionControls();
const activityLine = new ActivityLine(
  $("activity"),
  $("activity-sprite"),
  $("activity-label"),
);
const phoneSettings = setupSettings({
  request: (operation) => settingsRequest(key, operation),
  open: () => page("settings"),
  close: home,
  notifications: () => {
    home();
    $("notification-setup").open = true;
    $("notification-summary").focus();
  },
  login: async (runtime) => {
    return openPhoneTerminal(
      runtime === "codex" ? "codex-login" : "claude-login",
    );
  },
});
/** The host took a send and then lost it (a resume that failed after the Ack): its text
 * comes back into the composer once per lost message, before anything typed since, as in
 * the desktop replica. */
function restoreHostDrafts(model) {
  for (const s of model.scopes.values())
    for (const session of s.sessions) {
      const r = session.detail?.restored_draft;
      if (typeof r?.message !== "string" || typeof r.text !== "string") continue;
      const owner = keyOf(s.scope, session.id);
      if (restoredSeen.get(owner) === r.message) continue;
      restoredSeen.set(owner, r.message);
      // An Ack still on its way must not clear the restored text.
      const sent = unacked.get(owner);
      if (sent) sent.restored = true;
      const current = drafts.get(owner) || "";
      const text =
        !current || current === r.text ? r.text : `${r.text}\n${current}`;
      drafts.set(owner, text);
      if (selected && keyOf(selected.scope, selected.id) === owner) {
        $("draft").value = text;
        syncDraftHeight();
      }
    }
}
const repaint = () => {
  if (!scheduled) {
    scheduled = true;
    requestAnimationFrame(() => {
      scheduled = false;
      render();
    });
  }
};
function net(label, live = false) {
  $("network").textContent = label;
  $("network").classList.toggle("live", live);
}
class Link {
  constructor(scope, model) {
    this.scope = scope;
    this.model = model;
    this.pending = new Map();
    this.opens = new Set();
    this.attempt = 0;
    this.generation = 0;
    this.closed = false;
    this.ready = false;
    this.start();
  }
  send(msg) {
    this.socket.send(JSON.stringify(msg));
  }
  async start() {
    if (this.closed) return;
    const generation = ++this.generation;
    try {
      const signed = await proof(key, "connect");
      if (this.closed || this.generation !== generation) return;
      const ws = new WebSocket("wss://" + location.host + "/api/socket");
      this.socket = ws;
      ws.onopen = () => {
        if (this.closed) {
          ws.close();
          return;
        }
        this.send(signed.proof);
        this.send(wire.hello());
      };
      ws.onmessage = (e) => {
        if (this.closed) return;
        try {
          this.receive(JSON.parse(e.data));
        } catch {
          this.model.cursor = null;
          ws.close();
        }
      };
      ws.onclose = () => {
        if (this.generation === generation) this.lost();
      };
    } catch (e) {
      if (this.generation !== generation || this.closed) return;
      if (e.status === 403) {
        this.close();
        if (!this.scope) {
          detailLink?.close();
          detailLink = null;
          key = null;
          selected = null;
          global.scopes.clear();
          global.cursor = null;
          cold.clear();
          drafts.clear();
          restoredSeen.clear();
          timelineRows.clear();
          pickedProjects.clear();
          forgetComposer();
          routes = [];
          projects = [];
          for (const id of ["list", "timeline", "asks", "review-content"])
            $(id).replaceChildren();
          $("draft").value = "";
          syncDraftHeight();
          pairingPage();
        }
      } else this.lost();
    }
  }
  /** @param {Server} msg */
  receive(msg) {
    if (msg.type === "hello") {
      if (msg.version !== 1) {
        this.close();
        notice("This host requires a newer client.");
        return;
      }
      if (!this.scope) {
        routes = msg.routes;
        projects = msg.projects;
      }
      this.placement = msg.capabilities.includes("worker_placement");
      const liveCatalog =
        !this.scope && msg.capabilities.includes("live_catalog");
      this.send(
        liveCatalog
          ? { type: "watch_catalog" }
          : wire.subscribe(this.scope, this.model.cursor),
      );
      if (!this.scope) {
        this.discover();
        // Hello may predate model discovery; the settings catalog supplies the API routes
        // (and, once discovery lands, the rest) without a refresh.
        if (key) loadModelChoices();
      }
      // Replayed deltas may be empty. A subscription without a cursor requires its snapshot.
      if (this.model.cursor && !liveCatalog) this.connected();
      return;
    }
    if (msg.type === "discovered" && msg.id === this.discovery) {
      this.discovering.set(scopeKey(msg.scope), {
        scope: msg.scope,
        sessions: msg.rows,
      });
      cold.set(scopeKey(msg.scope), { scope: msg.scope, sessions: msg.rows });
      repaint();
      return;
    }
    if (msg.type === "discovered_end" && msg.id === this.discovery) {
      cold = this.discovering;
      this.discoveryResult = { complete: !msg.errors.length };
      repaint();
      return;
    }
    if (msg.type === "opened") {
      if (
        this.opens.delete(msg.id) &&
        detailLink === this &&
        scopeKey(this.scope) === scopeKey(msg.scope)
      ) {
        const old = keyOf(selected.scope, null);
        // Opened can land before or after the Ack. What is in the provisional
        // composer now is the user's own newer text unless it is still exactly the
        // message just sent (its Ack has not arrived to clear it).
        const typed = drafts.get(old),
          sent = unacked.get(old);
        drafts.delete(old);
        const fresh = keyOf(msg.scope, msg.thread);
        if (typed?.trim() && typed !== sent?.content) drafts.set(fresh, typed);
        moveImages(old, fresh, sent?.images);
        open(msg.scope, msg.thread);
      }
      return;
    }
    if (msg.type === "processes") {
      const cb = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      cb?.(true, msg.processes);
      return;
    }
    if (msg.type === "worker_reply") {
      if (this.inventoryRequest === msg.id) {
        this.inventoryRequest = null;
        if (this === detailLink && !$("worker-form").hidden) showWorkerHosts(msg.result, msg.error);
        return;
      }
      if (workerSubmission === msg.id) {
        workerSubmission = null;
        $("worker-submit").disabled = false;
        if (!msg.error) {
          $("worker-form").hidden = true;
          $("worker-task").value = "";
        }
      }
      if (!msg.error && msg.result?.relation === "handoff") {
        notice(`Continued in ${routesLabel(msg.result.scope.route)}.`);
        if (pendingGrant?.key === msg.id)
          pendingGrant = { scope: msg.result.scope, id: msg.result.session };
        open(msg.result.scope, msg.result.session);
        return;
      }
      if (!msg.error && msg.result?.status === "preparing") {
        notice(`Preparing worker ${msg.result.job_id} on ${msg.result.worker_host?.name || "the linked host"}.`);
        return;
      }
      notice(
        msg.error ??
          (msg.result?.status === "waiting_for_operator"
            ? `Worker ${msg.result?.job_id} waits for you to accept YOLO.`
            : `Started worker ${msg.result?.job_id} on ${msg.result?.model}. It appears under this session.`),
      );
      return;
    }
    if (msg.type === "ack" || msg.type === "error") {
      if (msg.type === "error") {
        this.opens.delete(msg.id);
        if (msg.id && msg.id === this.discovery) {
          cold = this.discovering;
          this.discoveryResult = { complete: false };
          repaint();
        }
      }
      const cb = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      if (msg.type === "error")
        notice(
          this.scope
            ? routesLabel(this.scope.route) + " · " + msg.message
            : msg.message,
        );
      cb?.(msg.type === "ack", msg.message, msg.id);
      return;
    }
    if (msg.type === "snapshot" || msg.type === "delta") {
      const changed = this.model.apply(msg);
      if (changed && this.scope) restoreHostDrafts(this.model);
      if (!this.ready) this.connected();
      if (changed) repaint();
    }
  }
  discover() {
    this.discoveryResult = null;
    this.discovery = crypto.randomUUID();
    this.discovering = new Map();
    this.send(wire.discover(this.discovery));
  }
  connected() {
    this.ready = true;
    this.attempt = 0;
    if (this === detailLink && this.scope && selected?.id) {
      this.act({ select_thread: selected.id });
    }
    repaint();
  }
  lost() {
    if (this.closed) return;
    this.ready = false;
    this.inventoryRequest = null;
    if (this === detailLink) { workerSubmission = null; $("worker-submit").disabled = false; }
    if (!this.scope) {
      this.discoveryResult = null;
      notificationTarget.reconnect();
    }
    if (this.pending.size) {
      notice(
        "Disconnected before confirmation. Check the conversation before retrying.",
      );
      for (const cb of this.pending.values()) cb?.(false);
      this.pending.clear();
    }
    this.timer = setTimeout(
      () => this.start(),
      Math.min(15000, 500 * 2 ** Math.min(5, this.attempt++)) +
        Math.random() * 250,
    );
    repaint();
  }
  /** Asks the host for its live processes. The callback gets (true, processes) or (false, message). */
  listProcesses(cb) {
    if (!this.ready || this.socket?.readyState !== WebSocket.OPEN) return false;
    const id = crypto.randomUUID();
    this.pending.set(id, cb);
    this.send(wire.listProcesses(id));
    return true;
  }
  act(action, approval = null, cb) {
    if (
      !this.scope ||
      !this.ready ||
      this.socket?.readyState !== WebSocket.OPEN
    ) {
      notice("Reconnect to send this action.");
      return false;
    }
    const id = crypto.randomUUID();
    if (
      action.send_text?.[0] === null ||
      action.send_images?.[0] === null ||
      action.fork_thread ||
      action.native_command?.[0] === null
    )
      this.opens.add(id);
    this.pending.set(id, cb);
    this.send(wire.act(id, this.scope, action, approval));
    // The request ID; a reply for it can arrive before the acknowledgement.
    return id;
  }
  reconnect() {
    if (this.closed) return;
    this.generation++;
    clearTimeout(this.timer);
    if (this.socket) {
      this.socket.onclose = null;
      this.socket.onmessage = null;
      this.socket.close();
    }
    this.lost();
    clearTimeout(this.timer);
    if (navigator.onLine) this.start();
  }
  close() {
    this.closed = true;
    this.ready = false;
    this.inventoryRequest = null;
    if (this === detailLink) { workerSubmission = null; $("worker-submit").disabled = false; }
    if (!this.scope) notificationTarget.reconnect();
    clearTimeout(this.timer);
    this.socket?.close();
    this.pending.clear();
  }
}
// The in-page confirm: a question, Cancel and the action. The app never uses window.confirm.
function clearConfirm(slot) {
  slot.hidden = true;
  slot.replaceChildren();
}
function confirmIn(slot, question, label, work) {
  const cancel = button("Cancel", () => clearConfirm(slot));
  cancel.className = "quiet";
  const go = button(label, () => {
    clearConfirm(slot);
    work();
  });
  go.className = "danger";
  slot.replaceChildren(el("p", "", question), cancel, go);
  slot.hidden = false;
  cancel.focus();
}
// Ends every thread of one scope over a short-lived subscription. True when each end was acknowledged.
function endThreads(scope, threads, force) {
  return new Promise((resolve) => {
    const link = new Link(scope, new Projection());
    const started = Date.now();
    let left = threads.length,
      ok = true;
    const settle = (done) => {
      if (!done) ok = false;
      if (--left === 0) {
        link.close();
        resolve(ok);
      }
    };
    const begin = () => {
      if (!link.ready) {
        if (Date.now() - started > 10000) {
          ok = false;
          link.close();
          resolve(false);
        } else setTimeout(begin, 100);
        return;
      }
      for (const thread of threads)
        if (!link.act(force ? { force_end_session: thread } : { end_session: thread }, null, (done) => settle(done)))
          settle(false);
    };
    if (!threads.length) resolve(true);
    else begin();
  });
}
async function endProcesses(list, force) {
  endBusy = true;
  drawRunning();
  const results = await Promise.all(
    list.map((p) => endThreads(p.scope, p.threads, force)),
  );
  endBusy = false;
  if (results.includes(false))
    notice("Some sessions did not end. Check the host and try again.");
  refreshRunning();
}
const ago = (seconds) => {
  const s = Math.max(0, Math.floor(seconds));
  if (s < 60) return s + "s";
  if (s < 3600) return Math.floor(s / 60) + "m";
  if (s < 86400) return Math.floor(s / 3600) + "h";
  return Math.floor(s / 86400) + "d";
};
const kindLabels = { claude: "Claude", codex: "Codex", worker: "Worker" };
function refreshRunning() {
  globalLink?.listProcesses((ok, value) => {
    if (!ok) {
      if (value) notice(value);
      return;
    }
    processes = value;
    drawRunning();
  });
}
function drawRunning() {
  const list = $("running-list");
  const now = Date.now() / 1000;
  $("running-empty").hidden = !(processes && !processes.length);
  $("running-loading").hidden = processes !== null;
  const idle = (processes ?? []).filter((p) => !p.running && !p.draft);
  $("end-idle").disabled = endBusy || !idle.length;
  const rows = [];
  for (const p of processes ?? []) {
    const row = el("article", "running-row");
    row.dataset.running = String(p.running);
    const head = el("div", "session-top");
    const name = el("span", "session-name");
    name.append(el("span", "run-dot"), leaf(p.project));
    head.append(
      name,
      el("span", "status", p.running ? "Running" : p.draft ? "Ready" : "Idle"),
    );
    if (p.draft) row.title = "Keeps Claude Code warm for new sessions";
    const facts = [
      kindLabels[p.kind] ?? p.kind,
      p.draft ? "ready (idle)" : p.model || p.route,
      p.threads.length > 1 ? p.threads.length + " sessions" : null,
      "age " + ago(now - p.started_at),
      p.draft ? null : "idle " + ago(now - p.last_activity),
    ].filter(Boolean);
    const actions = el("div", "actions");
    const end = button("End", () => endRow(p));
    end.disabled = endBusy;
    actions.append(end);
    row.append(head, el("div", "meta", facts.join(" · ")), actions);
    rows.push(row);
  }
  list.replaceChildren(...rows);
}
// Asks the host for a fresh process list and draws it. Calls done with the list, or null when none came back.
function freshProcesses(done) {
  const sent = globalLink?.listProcesses((ok, value) => {
    if (!ok) {
      if (value) notice(value);
      done(null);
      return;
    }
    processes = value;
    drawRunning();
    done(value);
  });
  if (!sent) {
    notice("Reconnect to see running sessions.");
    done(null);
  }
}
// Ends are decided on a fresh snapshot, never the cached one, which may be seconds old.
function endRow(p) {
  if (endChecking) return;
  endChecking = true;
  freshProcesses((list) => {
    endChecking = false;
    if (!list) return;
    // Several Claude sessions can share a scope, so the row must match its thread set too.
    const key = JSON.stringify(p.scope);
    const threads = [...p.threads].sort().join("\n");
    const fresh = list.find(
      (q) =>
        JSON.stringify(q.scope) === key && [...q.threads].sort().join("\n") === threads,
    );
    if (!fresh) {
      notice("That session has already ended.");
      return;
    }
    if (fresh.running)
      confirmIn(
        $("running-confirm"),
        `End ${leaf(fresh.project)}? Its running turn or pending approval stops. History is kept.`,
        "End session",
        () => endProcesses([fresh], true),
      );
    else endProcesses([fresh], false);
  });
}
function openRunning() {
  if (!globalLink?.ready) {
    notice("Reconnect to see running sessions.");
    return;
  }
  processes = null;
  page("running");
  notice();
  drawRunning();
  refreshRunning();
  runningTimer = setInterval(() => {
    if (!document.hidden) refreshRunning();
  }, 5000);
}
function stopRunning() {
  clearInterval(runningTimer);
  runningTimer = null;
}
$("open-running").onclick = openRunning;
$("end-idle").onclick = () => {
  if (endChecking) return;
  endChecking = true;
  freshProcesses((list) => {
    endChecking = false;
    if (!list) return;
    const idle = list.filter((p) => !p.running && !p.draft);
    if (!idle.length) {
      notice("No idle sessions to end.");
      return;
    }
    confirmIn(
      $("running-confirm"),
      `End ${idle.length} idle session${idle.length === 1 ? "" : "s"}? History is kept.`,
      "End idle",
      () => endProcesses(idle, false),
    );
  });
};
function home() {
  notificationTarget.request(null);
  detailLink?.close();
  detailLink = null;
  selected = null;
  pendingNew = null;
  page("catalog");
  notice();
  repaint();
}
function open(scope, id) {
  $("worker-form").hidden = true;
  controls.reset();
  detailLink?.close();
  selected = { scope, id, revealLatest: Boolean(id) };
  setComposerOwner(keyOf(scope, id), repaint, notice);
  pendingNew = null;
  visibleRows = 200;
  timelineRows.clear();
  askKeys = "";
  handoffStamp = "";
  jumpLatest.reset();
  $("asks").replaceChildren();
  $("timeline").replaceChildren();
  $("draft").value = drafts.get(keyOf(scope, id)) || "";
  page("conversation");
  syncDraftHeight();
  notice();
  detailLink = new Link(scope, new Projection());
  // Clear the prior session's actionable controls before a new frame can accept input.
  net("Connecting");
  conversation();
  repaint();
  scrollTo(0, 0);
}
function entries() {
  const map = new Map();
  for (const source of [cold, global.scopes])
    for (const state of source.values())
      for (const session of state.sessions)
        map.set(keyOf(state.scope, session.id), {
          scope: state.scope,
          session,
        });
  return [...map.values()];
}
function catalog() {
  const rows = entries();
  const drafts = [];
  for (const state of global.scopes.values()) {
    const requests = state.detail?.draft_requests ?? state.asks.filter((a) => a.thread == null).map((a) => a.id);
    for (const id of requests) drafts.push({ scope: state.scope, request: id });
  }
  const nowSec = Date.now() / 1000;
  const counts = { all: 0, active: 0, requests: 0, finished: 0 };
  const filterLabels = {
    all: "All",
    active: "Running",
    requests: "Needs you",
    finished: "Finished",
  };
  if (globalLink?.ready) {
    void notificationTarget.resolve([...rows, ...drafts], globalLink.discoveryResult, (match) => {
      if (match === "ambiguous")
        notice("This notification matches more than one session. Choose the session from the list.");
      else if (match === "missing")
        notice("This notification’s session is no longer in the list.");
      else if (match === "incomplete")
        notice("OX couldn’t load the full session list for this notification. Reconnect and look for the session.");
      else open(match.scope, match.session?.id ?? null);
    });
  }
  const needle = $("find-sessions").value.trim().toLocaleLowerCase();
  const visible = [];
  for (const row of rows) {
    const { scope, session } = row,
      s = global.scopes.get(scopeKey(scope)),
      needsInput =
        s?.detail?.request_threads?.includes(session.id) ||
        s?.asks?.some((a) => a.thread === session.id);
    if (s?.detail?.archived?.includes(session.id)) continue;
    counts.all++;
    if (needsInput) counts.requests++;
    else if (session.status === "running") counts.active++;
    else if (!["running", "needs_input"].includes(session.status))
      counts.finished++;
    if (
      needle &&
      ![
        session.title,
        session.detail?.row?.preview,
        scope.project,
        routesLabel(scope.route),
      ].some((v) => v?.toLocaleLowerCase().includes(needle))
    )
      continue;
    if (
      (filter === "requests" && !needsInput) ||
      // "Running" is the desktop chip: a live turn that is not waiting on you.
      (filter === "active" &&
        (needsInput || session.status !== "running")) ||
      (filter === "finished" &&
        (needsInput || ["running", "needs_input"].includes(session.status)))
    )
      continue;
    visible.push({ ...row, needsInput });
  }
  counts.requests += drafts.length;
  for (const b of $("filters").children) {
    const kind = b.dataset.filter;
    const base = filterLabels[kind] ?? b.textContent;
    const n = counts[kind] ?? 0;
    b.textContent =
      (kind === "active" || kind === "requests") && n > 0 ? `${base} (${n})` : base;
  }
  // Nest by the recorded parent scope, not the child's project: a worker may run
  // in a review checkout on another route. A filtered-out parent still leaves a
  // navigable reference on the child.
  const tree = catalogTree(visible, rows);
  const filtering = Boolean(needle) || filter !== "all";
  const pinnedList = [];
  const byProject = new Map();
  for (const entry of visible) {
    const k = keyOf(entry.scope, entry.session.id);
    if (pinnedThreads.has(k)) pinnedList.push(entry);
  }
  pinnedList.sort(
    (a, b) =>
      (pinnedThreads.get(keyOf(b.scope, b.session.id)) || 0) -
        (pinnedThreads.get(keyOf(a.scope, a.session.id)) || 0) ||
      b.session.updated_at - a.session.updated_at,
  );
  for (const entry of tree.roots) {
    if (pinnedThreads.has(keyOf(entry.scope, entry.session.id))) continue;
    const list = byProject.get(entry.scope.project) ?? [];
    list.push(entry);
    byProject.set(entry.scope.project, list);
  }
  const frag = document.createDocumentFragment();
  if (filter === "all" || filter === "requests") {
    for (const {scope} of drafts) {
      const title = `New session approval · ${leaf(scope.project)} · ${routesLabel(scope.route)}`;
      if (needle && !title.toLocaleLowerCase().includes(needle)) continue;
      const entry = button(title, () => open(scope, null));
      entry.className = "session";
      entry.dataset.needsInput = "true";
      frag.append(entry);
    }
  }
  const names = new Map();
  for (const project of byProject.keys())
    names.set(leaf(project), (names.get(leaf(project)) || 0) + 1);
  const done = new Set();
  const renderEntry = (entry, depth, showProjectTag) => {
    const { scope, session, needsInput } = entry,
      key = keyOf(scope, session.id);
    if (done.has(key)) return;
    done.add(key);
    const recorded = tree.links.get(key),
      elsewhere = recorded && recorded.project !== scope.project,
      model = actualModel(session),
      isPinned = pinnedThreads.has(key),
      accent = threadColors.get(key);
    const b = button("", () => open(scope, session.id));
    b.dataset.source =
      session.parent || session.detail?.worker || session.detail?.origin
        ? "worker"
        : scope.route.billing !== "subscription"
          ? "api"
          : scope.route.runtime;
    b.dataset.needsInput = String(Boolean(needsInput));
    b.dataset.passive = String(
      !needsInput && ["saved", "idle"].includes(session.status),
    );
    b.dataset.threadId = session.id;
    if (isPinned) b.dataset.pinned = "true";
    if (accent) b.dataset.accent = accent;
    b.className = "session" + (depth ? " nested" : "");
    const head = el("div", "session-top");
    const titleText =
      (isPinned ? "★ " : "") + (session.title || "Untitled session");
    head.append(
      el("span", "session-name", titleText),
      el("span", "status", statusText(session, needsInput)),
    );
    let meta = depth ? "↳ " : "";
    if (showProjectTag || (depth && elsewhere))
      meta += leaf(scope.project) + " · ";
    else if (!depth && recorded)
      meta += `↳ ${recorded.title || leaf(recorded.project)} · `;
    meta += routesLabel(scope.route);
    if (model) meta += ` · ${model}`;
    if (session.updated_at > 0 && nowSec >= session.updated_at)
      meta += ` · ${ago(nowSec - session.updated_at)}`;
    b.append(head, el("div", "meta", meta));
    const worker = session.detail?.worker_notice;
    if (worker?.status)
      b.append(
        el(
          "div",
          "worker-outcome",
          `Worker #${worker.job_id} · ${worker.status.replaceAll("_", " ")}${worker.host ? " on " + worker.host : ""}`,
        ),
      );
    frag.append(b);
    for (const child of tree.children.get(key) || [])
      if (!pinnedThreads.has(keyOf(child.scope, child.session.id)))
        renderEntry(child, depth + 1, false);
  };
  if (pinnedList.length) {
    const pinKey = "__pinned__";
    const folded = !filtering && foldedProjects.has(pinKey);
    const heading = el(
      "h2",
      "project pinned-project",
      `${folded ? "▸" : "▾"} ★ Pinned (${pinnedList.length})`,
    );
    heading.onclick = () => {
      if (foldedProjects.has(pinKey)) foldedProjects.delete(pinKey);
      else foldedProjects.add(pinKey);
      saveSidebarMeta();
      repaint();
    };
    frag.append(heading);
    if (!folded) {
      for (const entry of pinnedList) renderEntry(entry, 0, true);
    }
  }
  for (const [project, list] of [...byProject].sort((a, b) =>
    a[0].localeCompare(b[0]),
  )) {
    const name = leaf(project);
    const label = names.get(name) === 1 ? name : project;
    const folded = !filtering && foldedProjects.has(project);
    let runningCount = 0,
      waitingCount = 0;
    for (const entry of list) {
      if (entry.needsInput) waitingCount++;
      else if (entry.session.status === "running") runningCount++;
    }
    let badge = ` (${list.length})`;
    if (waitingCount) badge += ` · ${waitingCount} waiting`;
    else if (runningCount) badge += ` · ${runningCount} running`;
    const heading = el(
      "h2",
      "project",
      `${folded ? "▸" : "▾"} ${label}${badge}`,
    );
    heading.title = project;
    heading.onclick = () => {
      if (foldedProjects.has(project)) foldedProjects.delete(project);
      else foldedProjects.add(project);
      saveSidebarMeta();
      repaint();
    };
    frag.append(heading);
    if (!folded) {
      list.sort((a, b) => b.session.updated_at - a.session.updated_at);
      for (const entry of list) renderEntry(entry, 0, false);
    }
  }
  if (!frag.childNodes.length)
    frag.append(
      el(
        "p",
        "empty",
        filter === "requests"
          ? "Nothing needs you."
          : filter === "active"
            ? "No running sessions."
            : filter === "finished"
              ? "No finished sessions."
            : "No sessions found.",
      ),
    );
  $("list").replaceChildren(frag);
  $("new").disabled = !globalLink?.ready || !(routes.length || apiProvider);
}
function showAsks(asks) {
  const stamp = JSON.stringify(asks);
  if (stamp === askKeys) return;
  askKeys = stamp;
  const root = $("asks");
  root.replaceChildren();
  for (const a of asks) {
    const card = el("article", "ask");
    // A child that would run in YOLO waits here for the operator (ox-host yolo.rs).
    if (String(a.id).startsWith("ox-yolo-")) {
      card.classList.add("yolo");
      card.append(el("p", "ask-kicker", "YOLO approval"));
    }
    card.append(el("h2", "", a.title));
    if (a.detail) card.append(el("p", "", a.detail));
    // The command a shell approval would run, as the desktop card shows it.
    if (a.command?.command) {
      card.append(el("pre", "ask-command", a.command.command));
      if (a.command.cwd) card.append(el("p", "ask-kicker", String(a.command.cwd)));
    }
    const answer = (action) => {
      for (const b of card.querySelectorAll("button")) b.disabled = true;
      if (
        !detailLink.act(action, a.token, (ok) => {
          if (!ok)
            for (const b of card.querySelectorAll("button")) b.disabled = false;
        })
      )
        for (const b of card.querySelectorAll("button")) b.disabled = false;
    };
    if (a.questions?.length) {
      const fields = [];
      for (const q of a.questions) {
        const f = el("fieldset");
        f.append(el("legend", "", q.question));
        const choices = [];
        let other = null;
        for (const option of q.options || []) {
          const l = el("label", "option"),
            input = el("input");
          input.name = q.id;
          input.type = q.multiSelect ? "checkbox" : "radio";
          input.value = option.label;
          l.append(
            input,
            el(
              "span",
              "",
              option.label +
                (option.description ? " — " + option.description : ""),
            ),
          );
          f.append(l);
          choices.push(input);
        }
        if (q.isOther || !choices.length) {
          other = el("input");
          other.type = q.isSecret ? "password" : "text";
          other.setAttribute("aria-label", q.question);
          f.append(other);
        }
        fields.push(() =>
          [
            ...choices.filter((c) => c.checked).map((c) => c.value),
            other?.value.trim(),
          ]
            .filter(Boolean)
            .join(", "),
        );
        card.append(f);
      }
      card.append(
        button("Submit answers", () => {
          const values = fields.map((fn) => fn());
          if (values.every(Boolean))
            answer({ answer_questions: [a.id, values] });
          else notice("Answer each question first.");
        }),
      );
    } else {
      a.choices.forEach((c, i) => {
        const choice = button(c, () => answer({ answer_ask: [a.id, i] }));
        // Permission decisions are deliberately equal-weight choices.
        choice.className = /^(deny|decline|reject|cancel|no\b)/i.test(c)
          ? "decision deny"
          : "decision";
        card.append(choice);
      });
    }
    root.append(card);
  }
}
function syncChildren(root, nodes) {
  let next = root.firstChild;
  for (const node of nodes) {
    if (next === node) next = next.nextSibling;
    else root.insertBefore(node, next);
  }
  while (next) {
    const old = next;
    next = old.nextSibling;
    old.remove();
  }
}

// Mirrors the desktop shell_summary: a wrapped script loses both quotes.
function shellSummary(command) {
  const m = command?.match(/^(?:\S*\/)?(?:bash|zsh|sh)\s+-[a-z]*c\s+(['"]?)([\s\S]*)$/i);
  let text = m ? m[2] : (command ?? "");
  if (m && m[1] && text.endsWith(m[1])) text = text.slice(0, -1);
  return text.slice(0, 120);
}
function fileChanges(data) {
  return Array.isArray(data?.changes) ? data.changes : [];
}
// Added and removed lines of a unified diff, ignoring the ---/+++ headers.
function diffStats(changes) {
  let added = 0,
    removed = 0;
  for (const c of changes)
    for (const line of String(c.diff ?? "").split("\n")) {
      if (line.startsWith("+") && !line.startsWith("+++")) added++;
      else if (line.startsWith("-") && !line.startsWith("---")) removed++;
    }
  return `+${added} −${removed}`;
}
function fileChangeTitle(data) {
  const changes = fileChanges(data);
  if (!changes.length) return "File changes";
  const name =
    changes.length === 1
      ? String(changes[0].path ?? "File")
      : `${changes.length} changed files`;
  return `${name} · ${diffStats(changes)}`;
}

function toolInfo(item) {
  const [kind, data] = Object.entries(item.detail?.item ?? {})[0] ?? [];
  const labels = {
    CommandExecution: shellSummary(data?.command) || "Shell command",
    FileChange: fileChangeTitle(data),
    WebSearch: "Web search",
  };
  const title =
    labels[kind] ?? data?.tool ?? item.text.split("\n", 1)[0] ?? "Tool";
  const status = data?.status ?? item.detail?.status ?? "";
  const failed =
    /fail|error|declin/i.test(status) ||
    Number(data?.exit_code ?? item.detail?.exit ?? 0) !== 0;
  const active = /running|in.?progress|pending|started/i.test(status);
  return { title, data, kind, failed, active };
}

function toolText(item, info) {
  const data = info.data;
  if (info.kind === "CommandExecution") {
    const output = data?.aggregated_output || item.detail?.output;
    if (output) return [data.command, output].filter(Boolean).join("\n\n");
    return item.text;
  }
  if (info.kind === "FileChange" && fileChanges(data).length)
    // Same layout as the desktop write_detail: path, then the patch text.
    return fileChanges(data)
      .map((c) => `${c.path ?? ""}\n${c.diff ?? ""}`)
      .join("\n\n");
  return [
    item.text,
    item.detail?.output,
    data && !item.detail?.output ? JSON.stringify(data, null, 2) : "",
  ]
    .filter(Boolean)
    .join("\n\n");
}

function toolGroup(row, group) {
  if (!row.node) {
    row.node = el("details", "tool-group");
    row.count = el("span", "tool-count");
    row.state = el("span", "tool-state");
    const summary = el("summary");
    summary.append(row.count, row.state);
    row.list = el("div", "tool-list");
    row.node.append(summary, row.list);
    row.children = new Map();
    row.node.ontoggle = () => {
      if (row.node.open) drawToolList(row);
    };
  }
  row.group = group;
  row.count.textContent =
    group.length === 1 ? "1 tool call" : group.length + " tool calls";
  const failed = group.filter((entry) => entry.info.failed).length;
  row.state.textContent = failed
    ? failed + " failed"
    : group.some((entry) => entry.info.active)
      ? "Running"
      : "";
  if (row.node.open) drawToolList(row);
}

function drawToolList(group) {
  const next = new Map();
  for (const { item, info } of group.group) {
    let row = group.children.get(item.id);
    if (!row) {
      const node = el("details", "tool"),
        summary = el("summary"),
        output = el("pre");
      row = { node, summary, output };
      node.append(summary, output);
      node.ontoggle = () => {
        if (!node.open) output.replaceChildren();
        else if (row.info.kind === "FileChange")
          formatPatchNode(output, toolText(row.item, row.info));
        else output.textContent = toolText(row.item, row.info);
      };
    }
    if (row.item !== item || row.revision !== item.revision) {
      row.item = item;
      row.revision = item.revision;
      row.info = info;
      row.summary.textContent = info.title;
      if (row.node.open) {
        if (info.kind === "FileChange")
          formatPatchNode(row.output, toolText(item, info));
        else row.output.textContent = toolText(item, info);
      }
    }
    next.set(item.id, row);
  }
  syncChildren(
    group.list,
    [...next.values()].map((r) => r.node),
  );
  group.children = next;
}

function splitHandoff(text) {
  const open = "<ox-handoff>\n",
    close = "</ox-handoff>\n\n";
  if (!text?.startsWith(open)) return null;
  const end = text.indexOf(close);
  return end < 0 ? null : [text.slice(0, end + close.length), text.slice(end + close.length)];
}

// Only the first user message of a handoff target carries the folded block,
// and only once the first page of history is loaded.
function transcript(items, handoffTarget = false) {
  const foldId = handoffTarget
    ? items.find((i) => i.role === "user")?.id
    : undefined;
  const nodes = [],
    nextRows = new Map(),
    visible = items.slice(-visibleRows);
  if (items.length > visibleRows) {
    const b = button("Show earlier messages", () => {
      visibleRows += 200;
      repaint();
    });
    nodes.push(b);
  }
  for (let n = 0; n < visible.length; n++) {
    const i = visible[n];
    const key = JSON.stringify([i.turn, i.id, i.role]);
    const row = timelineRows.get(key) ?? {};
    if (i.role === "tool") {
      const group = [{ item: i, info: toolInfo(i) }];
      while (
        visible[n + 1]?.role === "tool" &&
        visible[n + 1].turn === i.turn
      ) {
        const item = visible[++n];
        group.push({ item, info: toolInfo(item) });
      }
      toolGroup(row, group);
    } else {
      if (!row.node) {
        const thinking = i.role === "thinking";
        row.node = el(thinking ? "details" : "article", "bubble " + i.role);
        row.label = el(
          thinking ? "summary" : "div",
          "role",
          {
            user: "You",
            assistant: "Assistant",
            thinking: "Thinking",
            plan: "Plan",
            progress: "Progress",
            result: "Result",
            error: "Error",
          }[i.role] ?? i.role,
        );
        row.text = el("div", "body");
        row.node.append(row.label, row.text);
      }
      const fold = foldId !== undefined && i.id === foldId;
      if (row.item !== i || row.revision !== i.revision || row.fold !== fold) {
        row.item = i;
        row.revision = i.revision;
        row.fold = fold;
        const carried =
          i.role === "user" && fold ? splitHandoff(i.text) : null;
        if (["assistant", "result", "plan"].includes(i.role))
          messageText(row.text, i.text);
        else if (carried) {
          // A Claude handoff target's first turn: fold the carried context.
          const fold = el("details", "handoff-context");
          fold.append(
            el("summary", "", "Carried handoff context"),
            el("pre", "", carried[0]),
          );
          row.text.replaceChildren(fold, document.createTextNode(carried[1]));
        } else row.text.textContent = i.text;
        if (["user", "assistant", "result"].includes(i.role)) {
          row.label.replaceChildren(
            document.createTextNode(
              { user: "You", assistant: "Assistant", result: "Result" }[i.role],
            ),
            copyButton("Copy", i.text),
          );
        }
      }
    }
    nextRows.set(key, row);
    nodes.push(row.node);
  }
  syncChildren($("timeline"), nodes);
  timelineRows = nextRows;
}

// "Jump to latest": shown once the reader is well above the bottom, with a count of what
// arrived meanwhile. At the bottom, new content keeps the view pinned there.
const jumpLatest = (() => {
  const gap = () =>
    document.documentElement.scrollHeight - innerHeight - scrollY;
  const reduced = () => matchMedia("(prefers-reduced-motion: reduce)").matches;
  let seen = 0,
    last = 0,
    lastStamp = "";
  const sync = () => {
    const b = $("jump-latest");
    b.onclick ??= () =>
      scrollTo({
        top: document.documentElement.scrollHeight,
        behavior: reduced() ? "instant" : "smooth",
      });
    const far = !$("conversation").hidden && gap() > innerHeight * 1.5;
    if (!far) seen = last;
    b.hidden = !far;
    const fresh = Math.max(0, last - seen);
    b.textContent = fresh ? `↓ ${fresh} new` : "↓";
    b.setAttribute(
      "aria-label",
      fresh ? `Jump to latest, ${fresh} new` : "Jump to latest",
    );
  };
  addEventListener("scroll", sync, { passive: true });
  return {
    pinned: () => scrollY > 2 && gap() < 80,
    reset() {
      seen = 0;
      last = 0;
      lastStamp = "";
    },
    update(count, stamp, pinned) {
      last = count;
      const changed = stamp !== lastStamp;
      lastStamp = stamp;
      if (pinned && changed) scrollTo(0, document.documentElement.scrollHeight);
      sync();
    },
  };
})();

// Keep the session name in reach once the full heading has scrolled past the navigation.
new IntersectionObserver(([entry]) => {
  $("nav-title").hidden = $("conversation").hidden || entry.boundingClientRect.bottom >= 0;
}).observe($("title"));

function updateComposerState() {
  if (!selected) return;
  const { scope, id } = selected,
    k = scopeKey(scope);
  const s = detailLink?.model.scopes.get(k),
    g = global.scopes.get(k);
  const row =
    s?.sessions.find((x) => x.id === id) ??
    g?.sessions.find((x) => x.id === id) ??
    cold.get(k)?.sessions.find((x) => x.id === id);
  const caps = row?.capabilities || [];
  const inputCaps = id ? caps : (s?.capabilities ?? []);
  updateExtras(s, inputCaps, detailLink, id);
  const nativeLive = !!id && caps.includes("native_live");
  $("native-live").hidden = !nativeLive;
  $("native-live-text").textContent = nativeLiveText(
    row?.detail?.native_live ?? "Claude Code",
  );
  $("native-live-fork").hidden = !caps.includes("fork");
  $("native-live-fork").onclick = () => detailLink?.act({ fork_thread: id });
  $("draft").disabled = nativeLive;
  const canSend = id
    ? caps.some((c) => c === "send" || c === "steer")
    : s?.capabilities.includes("start");
  $("send").textContent = caps.includes("steer") ? "Steer" : "Send";
  $("send").disabled =
    !canSend ||
    !detailLink?.ready ||
    (!$("draft").value.trim() && !imagesFor(keyOf(scope, id)).length) ||
    (imagesFor(keyOf(scope, id)).length > 0 &&
      !inputCaps.includes("attach_image")) ||
    Boolean(pendingNew);
  const running = Boolean(detailLink?.ready && caps.includes("stop"));
  $("stop").disabled = !running;
  $("stop").dataset.active = String(running);
  $("stop").onclick = () => detailLink?.act({ stop: id });
  $("menu-stop").disabled = $("stop").disabled;
  $("menu-stop").hidden = !id;
}

function conversation() {
  if (!selected) return;
  const { scope, id } = selected,
    k = scopeKey(scope);
  const s = detailLink?.model.scopes.get(k),
    g = global.scopes.get(k);
  const row =
    s?.sessions.find((x) => x.id === id) ??
    g?.sessions.find((x) => x.id === id) ??
    cold.get(k)?.sessions.find((x) => x.id === id);
  const caps = row?.capabilities || [];
  const origin = row?.detail?.origin;
  controls.update({
    scope,
    id,
    state: s,
    row: s?.sessions.find((x) => x.id === id),
    ready: Boolean(detailLink?.ready),
    act: (action, cb) => detailLink?.act(action, null, cb),
    archived: home,
    confirm: (question, label, work) =>
      confirmIn($("confirm-slot"), question, label, work),
    notice,
    routesLabel,
    pinned: Boolean(id && pinnedThreads.has(keyOf(scope, id))),
    color: id ? threadColors.get(keyOf(scope, id)) || null : null,
    togglePin: () => {
      if (!id) return;
      const tk = keyOf(scope, id);
      if (pinnedThreads.has(tk)) pinnedThreads.delete(tk);
      else pinnedThreads.set(tk, Date.now());
      saveSidebarMeta();
      closeActionMenus();
      repaint();
    },
    cycleColor: () => {
      if (!id) return;
      const tk = keyOf(scope, id);
      const cur = threadColors.get(tk);
      const idx = cur ? ACCENT_COLORS.indexOf(cur) : -1;
      if (idx + 1 < ACCENT_COLORS.length)
        threadColors.set(tk, ACCENT_COLORS[idx + 1]);
      else threadColors.delete(tk);
      saveSidebarMeta();
      closeActionMenus();
      repaint();
    },
  });
  $("title").textContent = row?.title || (id ? "Session" : "New session");
  $("nav-title").textContent = $("title").textContent;
  // A worker says what it runs as: its billing route, the model the runtime
  // confirmed (pending until then), the access ceiling and its checkout.
  const worker = origin && origin.relation !== "handoff";
  $("context").textContent =
    (worker ? "checkout " + leaf(scope.project) : leaf(scope.project)) +
    " · " +
    routesLabel(row?.detail?.route || scope.route) +
    (worker
      ? ` · worker ${origin.job_id} · ${ceilingLabels[origin.ceiling] ?? origin.ceiling} ceiling${workerModel(row?.detail?.worker_model) || " · model pending"}`
      : origin?.relation === "handoff"
        ? " · handoff"
        : "");
  $("context").title = worker ? scope.project : "";
  if (worker) {
    const parentOf = catalogParentResolver(entries());
    const parent = parentOf({ scope, session: row });
    if (parent) {
      const link = button(`↳ ${parent.title || "Parent session"}`, () =>
        open(parent.scope, parent.id),
      );
      link.className = "context-link";
      link.title = parent.project;
      $("context").append(document.createTextNode(" · "), link);
    }
  }
  handoffMarks(row);
  if (
    pendingGrant?.id === id &&
    scopeKey(pendingGrant.scope) === k &&
    detailLink?.ready
  ) {
    pendingGrant = null;
    detailLink.act({ yolo_grant: [id, true] });
  }
  const timeline = s?.transcripts[id];
  showReview(timeline?.detail?.reviews);
  const pinnedBottom = jumpLatest.pinned();
  const items = timeline?.items || [];
  const tail = items[items.length - 1];
  const asks = (s?.asks || []).filter((a) => a.thread === id);
  transcript(
    items,
    !!origin?.handoff && !timeline?.detail?.older,
  );
  $("older").hidden = !timeline?.detail?.older;
  $("older").onclick = () => detailLink.act({ load_older: id });
  $("start-worker").hidden = !id || !caps.includes("open");
  $("continue-in").hidden = !id || !caps.includes("open");
  showAsks(asks);
  // The activity line only trusts a live link: a reconnect must not look like a finished turn.
  if (detailLink?.ready && s) {
    activityLine.update({
      session: keyOf(scope, id),
      items,
      running: caps.includes("stop"),
      waiting: asks.length > 0,
      failed: items[items.length - 1]?.role === "error",
      stopped: timeline?.detail?.interrupted === true,
    });
  }
  const streamStamp = `${items.length}:${tail?.id ?? ""}:${tail?.revision ?? 0}:${askKeys}:${document.documentElement.scrollHeight}`;
  jumpLatest.update(items.length, streamStamp, pinnedBottom);
  if (selected.revealLatest && timeline?.items?.length) {
    selected.revealLatest = false;
    const answers = $("timeline").querySelectorAll(".assistant, .result");
    const latest =
      $("asks").firstElementChild ??
      answers[answers.length - 1] ??
      $("timeline").lastElementChild;
    if (
      latest &&
      window.scrollY < 2 &&
      !document.activeElement?.closest("#conversation")
    ) {
      const nav = document.querySelector(".conversation-nav");
      if (
        latest.getBoundingClientRect().top >
        $("composer").getBoundingClientRect().top - 80
      ) {
        window.scrollBy(
          0,
          latest.getBoundingClientRect().top - nav.offsetHeight - 8,
        );
      }
    }
  }
  updateComposerState();
  // A running turn that waits on an approval still gets the session terminal, so the
  // person can review the command there (the desktop offers the same path).
  const awaitingApproval =
    caps.includes("stop") && (s?.asks || []).some((a) => a.thread === id);
  $("operator-session").hidden =
    !id ||
    !!row?.parent ||
    !!origin ||
    !(caps.includes("send") || awaitingApproval);
  $("menu-session").hidden = !id;
  $("menu-workers").hidden = !id || !caps.includes("open");
  $("menu-settings").hidden = $("session-controls").hidden;
}
function render() {
  if (!$("pair").hidden) return;
  const live = globalLink?.ready && (!selected || detailLink?.ready);
  net(live ? "Connected" : "Reconnecting", live);
  if (!$("catalog").hidden) catalog();
  if (!$("conversation").hidden) conversation();
  updateNeedsBar();
}
// A sticky "needs you" bar in the composer stack whenever any session has a pending ask.
function updateNeedsBar() {
  const bar = $("needs-bar");
  const seen = new Set(),
    asks = [];
  for (const model of [global, detailLink?.model]) {
    for (const s of model?.scopes.values() ?? []) {
      for (const a of s.asks ?? []) {
        const id = scopeKey(s.scope) + " " + a.id;
        if (seen.has(id)) continue;
        seen.add(id);
        asks.push({ scope: s.scope, thread: a.thread });
      }
    }
  }
  bar.hidden = !asks.length;
  if (!asks.length) return;
  const n = asks.length;
  bar.textContent = `NEEDS YOU · ${n === 1 ? "1 request" : `${n} requests`}`;
  bar.onclick = () => {
    const here = $("asks").firstElementChild;
    if (here) here.scrollIntoView({ behavior: "smooth", block: "center" });
    else open(asks[0].scope, asks[0].thread);
  };
}
$("menu-stop").onclick = () => $("stop").click();
$("menu-settings").onclick = () => $("open-settings").click();
$("home").onclick = () => {
  if (key) home();
};
document.querySelectorAll(".back").forEach((b) => (b.onclick = home));
$("find-sessions").oninput = repaint;
$("filters").onclick = (e) => {
  if (!e.target.dataset.filter) return;
  filter = e.target.dataset.filter;
  for (const b of $("filters").children)
    b.setAttribute("aria-pressed", String(b.dataset.filter === filter));
  repaint();
};
function fillCreateRoutes() {
  const select = $("route"),
    keep = select.value;
  const subs = [],
    starred = [],
    rest = [];
  const rank = (r) => {
    const i = modelChoices.starred.indexOf(r.billing?.api);
    return i < 0 ? Infinity : i;
  };
  routes.forEach((r, i) => {
    const opt = new Option(routesLabel(r), String(i));
    if (r.billing === "subscription") subs.push(opt);
    else if (rank(r) !== Infinity) starred.push([rank(r), opt]);
    else rest.push(opt);
  });
  starred.sort((a, b) => a[0] - b[0]);
  const groups = [];
  if (subs.length) groups.push(optgroup("Subscriptions", subs));
  if (starred.length)
    groups.push(optgroup("Starred", starred.map(([, o]) => o)));
  if (rest.length) groups.push(optgroup("All models", rest));
  select.replaceChildren(...groups);
  if (keep !== "" && routes[+keep]) select.value = keep;
}
$("new").onclick = () => {
  const options = (node, values, labels) =>
    node.replaceChildren(
      ...values.map((_, i) => new Option(labels[i], String(i))),
    );
  projects = [
    ...new Set([
      ...projects,
      ...[...global.scopes.values(), ...cold.values()].map(
        (s) => s.scope.project,
      ),
    ]),
  ];
  $("folder-picker").hidden = true;
  options($("project"), projects, projects);
  fillCreateRoutes();
  page("create");
  loadModelChoices();
};
let browseSequence = 0,
  folderPage,
  afterName;
const pickedProjects = new Set();
async function browseFolder(path, after = null) {
  const seq = ++browseSequence;
  $("folder-picker").hidden = false;
  $("folder-use").disabled = true;
  $("folder-more").hidden = true;
  if (!after) $("folder-list").replaceChildren();
  try {
    const page = await settingsRequest(key, {
      action: "browse_folders",
      path,
      after,
    });
    if (seq !== browseSequence || $("create").hidden) return;
    folderPage = page;
    $("folder-path").value = page.path;
    $("folder-up").disabled = !page.parent;
    for (const name of page.folders) {
      const path = page.path.replace(/\/$/, "") + "/" + name;
      $("folder-list").append(button(name, () => browseFolder(path)));
    }
    afterName = page.folders.at(-1);
    $("folder-more").hidden = !page.more;
    $("folder-use").disabled = false;
    notice();
  } catch (error) {
    if (seq === browseSequence) notice(error.message);
  }
}
$("browse-projects").onclick = () => browseFolder(null);
$("folder-go").onclick = () => browseFolder($("folder-path").value);
$("folder-path").onkeydown = (e) => {
  if (e.key === "Enter") {
    e.preventDefault();
    browseFolder(e.target.value);
  }
};
$("folder-up").onclick = () => browseFolder(folderPage.parent);
$("folder-more").onclick = () => browseFolder(folderPage.path, afterName);
$("folder-use").onclick = () => {
  const path = folderPage.path;
  if (!projects.includes(path)) {
    projects.push(path);
    $("project").add(new Option(path, String(projects.length - 1)));
  }
  pickedProjects.add(path);
  $("project").value = String(projects.indexOf(path));
  $("folder-picker").hidden = true;
};
$("new-form").onsubmit = async (e) => {
  e.preventDefault();
  const project = projects[+$("project").value],
    route = routes[+$("route").value];
  if (!project || !route) return;
  try {
    if (pickedProjects.has(project))
      await settingsRequest(key, {
        action: "choose_project",
        path: project,
        route,
      });
    if (!$("create").hidden) open({ project, route }, null);
  } catch (error) {
    notice(error.message);
  }
};
// The model a worker runs, after its billing route: the model that answered, or the
// requested one marked pending until the runtime answers. Mirrors the desktop line.
function workerModel(report) {
  if (!report) return "";
  const { model_state: state, actual_model: actual, pinned_model: pinned } = report;
  if ((state === "confirmed" || state === "reported") && actual) return ` · ${actual}`;
  if (state === "differs" && actual)
    return ` · ${actual}, not ${pinned || "the pinned model"}`;
  const asked = pinned || (report.billing === "subscription" && report.requested_model);
  return asked ? ` · ${asked} (pending)` : "";
}
const ceilingLabels = {
  read: "read-only",
  write: "workspace write",
  classifier: "classifier",
  yolo: "YOLO",
};
let pendingGrant = null;
const bypassMode = (m) => m === "yolo" || m === "bypass_permissions";
// The provider that receives a route's conversation, for the one-line notice.
const provider = (route) =>
  route.billing?.api
    ? `the ${route.billing.api.split("/")[0]} API provider`
    : route.runtime === "codex"
      ? "OpenAI (ChatGPT subscription)"
      : "Anthropic (Claude subscription)";
function linkCard({ label, title, meta, tone = "", action }) {
  const card = el("article", "link-card " + tone);
  const body = el("div", "link-card-body");
  body.append(
    el("p", "link-card-label", label),
    el("strong", "link-card-title", title),
  );
  if (meta) body.append(el("span", "meta", meta));
  card.append(el("span", "link-card-icon"), body);
  if (action) {
    const b = button(action[0], action[1]);
    b.className = "quiet link-card-action";
    card.append(b);
  }
  return card;
}
function handoffMarks(row) {
  const box = $("handoff-marks"),
    d = row?.detail ?? {},
    origin = d.origin,
    nodes = [];
  const stamp = JSON.stringify([
    row?.id ?? null,
    origin ?? null,
    d.handed_off_to ?? null,
    d.handoff_attempts ?? null,
    d.yolo_grant ?? null,
    d.yolo_children ?? null,
  ]);
  if (stamp === handoffStamp) return;
  handoffStamp = stamp;
  if (origin?.relation === "handoff" && origin.handoff) {
    const h = origin.handoff,
      sel = h.selection ?? {};
    const why =
      {
        inherited: "inherited",
        lowered: "lowered",
        operator: "chosen by you",
        host_policy: "set by the host policy",
        capped: "capped below YOLO",
      }[origin.decided] ?? "unknown";
    nodes.push(
      linkCard({
        label: "Continued from",
        title: h.title,
        meta: `${routesLabel(h.route)} · mode ${modeLabel(origin.started)} (${why}) · ${sel.carried ?? 0} messages carried, ${sel.omitted ?? 0} omitted`,
        action: ["Open source", () => open(h.scope, h.session)],
      }),
    );
  }
  for (const to of d.handed_off_to ?? [])
    nodes.push(
      linkCard({
        label: "Handed off to",
        title: to.label,
        action: ["Open", () => open(to.scope, to.session)],
      }),
    );
  for (const a of d.handoff_attempts ?? [])
    nodes.push(
      a.status === "failed"
        ? linkCard({
            label: "Handoff failed",
            title: a.label,
            meta: a.message ?? "unknown error",
            tone: "failed",
            action: ["Retry", () => $("continue-in").click()],
          })
        : linkCard({ label: "Handing off", title: a.label, tone: "pending" }),
    );
  box.replaceChildren(...nodes);
  box.hidden = !nodes.length;
  const status = $("yolo-grant-status");
  if (d.yolo_grant === true && row) {
    const children = d.yolo_children ?? [];
    const p = el(
      "p",
      "",
      `YOLO also allowed for this session's workers and handoffs. ${children.length} started under it keep their mode if you revoke.`,
    );
    p.append(
      button("Revoke", () =>
        detailLink?.act({ yolo_grant: [row.id, false] }, null, (ok, m) => {
          if (!ok) notice(m || "The host refused.");
        }),
      ),
    );
    status.replaceChildren(p);
    status.hidden = false;
  } else {
    status.replaceChildren();
    status.hidden = true;
  }
}
const modeLabel = (m) =>
  ({
    read_only: "Read only",
    workspace: "Workspace",
    auto_review: "Auto-review",
    yolo: "YOLO",
    default: "Default",
    auto: "Auto",
    accept_edits: "Accept edits",
    plan: "Plan",
    bypass_permissions: "bypass permissions",
  })[m] ?? "unverified";
function handoffRoutes() {
  const runtime = $("handoff-runtime").value,
    select = $("handoff-route"),
    keep = select.value;
  const list = routes.filter((r) => r.runtime === runtime),
    rank = (r) => {
      const i = modelChoices.starred.indexOf(r.billing?.api);
      return i < 0 ? Infinity : i;
    };
  const starred = list
      .filter((r) => rank(r) !== Infinity)
      .sort((x, y) => rank(x) - rank(y)),
    rest = list.filter((r) => rank(r) === Infinity);
  const option = (r) => new Option(routesLabel(r), String(routes.indexOf(r)));
  const groups = [];
  if (starred.length)
    groups.push(optgroup("Starred", starred.map(option)));
  if (rest.length) groups.push(optgroup("All models", rest.map(option)));
  select.replaceChildren(...groups);
  if (keep !== "" && routes[+keep]?.runtime === runtime) select.value = keep;
  handoffNotice();
}
// The route the handoff form currently targets. An empty selection (no route of
// the chosen runtime is configured) is no route, never index 0 of another runtime.
const handoffTarget = () => {
  const index = $("handoff-route").value;
  const route = index === "" ? undefined : routes[+index];
  return route?.runtime === $("handoff-runtime").value ? route : undefined;
};
const runtimeName = (r) => (r === "codex" ? "ChatGPT/Codex" : "Claude");
function handoffNotice() {
  const target = handoffTarget(),
    source = selected?.scope?.route;
  const line = $("handoff-provider");
  if (!target) {
    line.textContent = `No ${runtimeName($("handoff-runtime").value)} model or subscription is configured on this host. Choose another runtime or sign in on the host.`;
    line.hidden = false;
  } else if (source && provider(target) !== provider(source)) {
    line.textContent = `This handoff sends the carried conversation to ${provider(target)} (${routesLabel(target)}).`;
    line.hidden = false;
  } else line.hidden = true;
  $("handoff-grant-row").hidden = $("handoff-mode").value !== "yolo";
  labelPermissions(
    $("handoff-mode"),
    $("handoff-runtime").value,
    $("handoff-permission-note"),
  );
}
$("continue-in").onclick = () => {
  // Starts on the session's own runtime and route, as the desktop handoff sheet does.
  const from = selected?.scope?.route;
  if (from) $("handoff-runtime").value = from.runtime;
  handoffRoutes();
  const same = routes.findIndex(
    (r) =>
      r.runtime === from?.runtime &&
      (r.billing === "subscription"
        ? from.billing === "subscription"
        : r.billing?.api === from?.billing?.api),
  );
  if (same >= 0) $("handoff-route").value = String(same);
  handoffNotice();
  loadModelChoices();
  $("handoff-form").hidden = false;
  $("handoff-request").focus();
};
$("handoff-runtime").onchange = handoffRoutes;
$("handoff-route").onchange = handoffNotice;
$("handoff-mode").onchange = handoffNotice;
$("handoff-cancel").onclick = () => ($("handoff-form").hidden = true);
$("handoff-form").onsubmit = (e) => {
  e.preventDefault();
  if (!selected?.id) return;
  const route = handoffTarget();
  if (!route) {
    notice(
      `No ${runtimeName($("handoff-runtime").value)} model or subscription is configured on this host.`,
    );
    return;
  }
  const mode = $("handoff-mode").value;
  const request = $("handoff-request").value.trim();
  if (!request) {
    notice("Write the request for the new session.");
    return;
  }
  const confirmed = mode === "yolo";
  const start = {
    source: selected.id,
    runtime: route.runtime,
    model: route.billing?.api ?? null,
    mode,
    request,
    yolo_confirmed: confirmed,
  };
  const grant = mode === "yolo" && $("handoff-grant").checked;
  const key = detailLink?.act({ start_handoff: start }, null, (ok, message) => {
    if (!ok) {
      notice(message || "The host refused this handoff.");
      if (pendingGrant?.key === key) pendingGrant = null;
    } else {
      // An immediate refusal arrives as a worker_reply error; keep that notice.
      $("handoff-form").hidden = true;
      $("handoff-request").value = "";
    }
  });
  // Register the grant before any reply is handled: on a ready route the host's
  // handoff reply can arrive before its acknowledgement.
  if (grant && key) pendingGrant = { key };
};
// Model lists for the worker and handoff sheets: the host's starred models first,
// then every known model and every API route the phone already has.
let modelChoices = { starred: [], all: [] };
const optgroup = (label, options) => {
  const g = document.createElement("optgroup");
  g.label = label;
  g.append(...options);
  return g;
};
async function loadModelChoices() {
  let starred = [],
    known = [];
  const clean = (list) => [
    ...new Set(list.filter((m) => typeof m === "string" && m)),
  ];
  const publish = () => {
    modelChoices = {
      starred,
      all: clean([...starred, ...known, ...routes.map((r) => r.billing?.api)]),
    };
    addCatalogRoutes();
    fillWorkerModels();
    handoffRoutes();
    if (!$("create").hidden) fillCreateRoutes();
    repaint();
  };
  try {
    const rows = await settingsRequest(key, { action: "read_rows" });
    starred = clean(rows.favorites ?? []);
    known = rows.models ?? [];
    apiProvider = !!rows.api_provider;
  } catch {
    // The route list alone still works.
  }
  // Settings are quick: show the configured aliases before discovery returns.
  publish();
  try {
    // The full discovered catalog: stars only order, any model can be used.
    const found = await settingsRequest(key, { action: "models" });
    if (Array.isArray(found)) known = [...known, ...found];
  } catch {
    // Discovery needs the providers; the starred and configured models still work.
  }
  publish();
}
// Routes are only appended, so the indexes the forms hold stay valid.
function addCatalogRoutes() {
  const have = new Set(routes.map((r) => `${r.runtime} ${r.billing?.api ?? ""}`));
  for (const runtime of ["codex", "claude"]) {
    if (!apiProvider && !routes.some((r) => r.runtime === runtime)) continue;
    for (const m of modelChoices.all) {
      if (have.has(`${runtime} ${m}`)) continue;
      routes.push({ runtime, billing: { api: m } });
    }
  }
}
// The model the worker sheet should start from: the parent session's route, set when
// the sheet opens (desktop worker_sheet.rs Choice::new). Cleared when the runtime changes.
let workerSeed = null;
let workerSubmission = null;
let workerHosts = [];
let workerProjectRule = null;
function requestWorkerHosts() {
  if (!detailLink?.ready || !detailLink.placement || detailLink.inventoryRequest || $("worker-form").hidden) return;
  detailLink.inventoryRequest = crypto.randomUUID();
  detailLink.send({type:"worker_hosts", id:detailLink.inventoryRequest, project:detailLink.scope.project});
}
function showWorkerHosts(result, error) {
  $("worker-inventory-note").textContent = error || result?.warning || result?.placement_error || "";
  if (!result?.hosts) {
    workerHosts = [];
    showWorkerLimits();
    return;
  }
  workerHosts = result.hosts;
  workerProjectRule = result.project_placement;
  const select = $("worker-host"), previous = select.value;
  const options = workerHosts.map(row => {
    const name = row.parent_host ? "Same as parent" : row.paired_name || row.host.name;
    const r = row.host.resources;
    const info = r ? `load ${r.load_one_minute.toFixed(1)} · ${(r.available_memory / 1e9).toFixed(1)} GB free` : row.status;
    const opt = new Option(name + " · " + info, row.parent_host ? "parent" : row.host.id);
    opt.disabled = row.status !== "online";
    return opt;
  });
  if (result.project_placement) {
    const rule = result.project_placement;
    options.push(new Option(["Project rule", rule.host || "Prefer parent", rule.os].filter(Boolean).join(" · "), "project"));
  }
  options.push(new Option("Auto", "auto"));
  // Keep an explicit choice visible if the link was removed while the sheet was open.
  if (previous && !options.some(o => o.value === previous)) {
    const gone = new Option(select.selectedOptions[0].text + " · unavailable", previous);
    gone.disabled = true;
    options.push(gone);
  }
  select.replaceChildren(...options);
  select.value = previous || "parent";
  fillWorkerModels();
}
function workerRoutes() {
  const host = $("worker-host").value;
  if (!detailLink?.placement || host === "parent") return null;
  return workerHosts.filter(r => r.status === "online" && (host === "auto" || host === "project" || r.host.id === host)).flatMap(r => r.routes || []);
}
$("worker-host").onchange = () => {
  const auto = $("worker-host").value === "auto";
  $("worker-needs").hidden = $("worker-needs").disabled = !auto;
  workerSeed = null;
  fillWorkerModels();
};
$("worker-model").onchange = () => { workerSeed = null; showWorkerLimits(); };
$("worker-model-free").oninput = showWorkerLimits;
function showWorkerLimits() {
  const panel = $("worker-subscription-limits");
  panel.replaceChildren();
  const target = $("worker-host").value, runtime = $("worker-runtime").value;
  if (!workerModelValue()) {
    const rule = target === "project" ? workerProjectRule : null;
    const selector = (target === "project" ? rule?.host || "auto" : target).toLowerCase();
    const now = Date.now() / 1000;
    const stamp = (n) => new Date(n * 1000).toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
    for (const row of workerHosts) {
      const selected = selector === "auto" || (selector === "parent" ? row.parent_host
        : selector.startsWith("label:") ? row.host.labels?.some(s => s.toLowerCase() === selector.slice(6))
        : [row.host.id, row.host.name, row.paired_name].some(s => s?.toLowerCase() === selector));
      if (!selected || row.status !== "online" || (rule?.os && rule.os.toLowerCase() !== row.host.resources?.os?.toLowerCase())) continue;
      if (!row.routes?.some(r => r.runtime === runtime && r.billing === "subscription")) continue;
      const windows = row.host.subscription_limits?.[runtime];
      if (!Array.isArray(windows)) continue;
      for (const w of windows) {
        if (!w || typeof w.label !== "string" || !Number.isFinite(w.observed_at)) continue;
        if (w.resets_at != null && w.resets_at <= now) continue;
        const parts = [row.paired_name || row.host.name, runtime === "claude" ? "Claude" : "ChatGPT", w.label];
        if (w.used_percent != null) parts.push(w.used_percent + "% used");
        if (w.status && (w.status !== "allowed" || w.used_percent == null)) parts.push(w.status);
        parts.push((now - w.observed_at > 3600 ? "last reported " : "reported ") + stamp(w.observed_at));
        if (w.resets_at != null) parts.push("resets " + stamp(w.resets_at));
        const line = document.createElement("div");
        line.textContent = parts.join(" · ");
        panel.append(line);
      }
    }
  }
  panel.hidden = !panel.childElementCount;
}
setInterval(() => { if (!document.hidden) requestWorkerHosts(); }, 30_000);
function fillWorkerModels() {
  const select = $("worker-model"),
    runtime = $("worker-runtime").value,
    current = workerSeed ? workerSeed.model : select.value,
    eligible = workerRoutes(),
    aliases = eligible ? [...new Set(eligible.filter(r => r.runtime === runtime && r.billing?.api).map(r => r.billing.api))] : modelChoices.all,
    starred = modelChoices.starred.filter(m => aliases.includes(m)),
    rest = aliases.filter((m) => !starred.includes(m));
  const option = (m) => new Option(m, m);
  // A Codex worker needs an OX API model (desktop worker_sheet.rs subscription_ok).
  const subscription = new Option(
    runtime === "codex" ? "ChatGPT subscription" : "Claude subscription",
    "",
  );
  subscription.disabled = runtime === "codex";
  select.replaceChildren(
    subscription,
    ...(starred.length ? [optgroup("Starred", starred.map(option))] : []),
    ...(rest.length ? [optgroup("All models", rest.map(option))] : []),
  );
  select.value = [...select.options].some((o) => o.value === current)
    ? current
    : "";
  if (runtime === "codex" && !select.value && select.options.length > 1)
    select.value = select.options[1].value;
  // Free text is the fallback only when the phone has no model list at all.
  const free = !aliases.length;
  select.hidden = free;
  $("worker-model-free").hidden = !free;
  showWorkerLimits();
}
function workerModelValue() {
  return ($("worker-model-free").hidden
    ? $("worker-model").value
    : $("worker-model-free").value.trim()) || null;
}
// Labels and one-line notes are the desktop's (worker_sheet.rs mode_label / mode_note).
const workerModes = {
  session: "Session",
  goal: "Goal",
  workflow: "Workflow",
};
const workerModeNotes = {
  session: "The task starts a new session under this one.",
  goal: "Codex keeps working toward the task as an objective.",
  workflow: "Claude Code runs the task with its Workflow tool.",
};
const modesFor = (runtime) =>
  runtime === "codex" ? ["session", "goal"] : ["session", "workflow"];
function renderWorkerMode() {
  const allowed = modesFor($("worker-runtime").value);
  if (!allowed.includes($("worker-mode").value)) $("worker-mode").value = "session";
  const current = $("worker-mode").value;
  $("worker-mode-note").textContent = workerModeNotes[current] ?? "";
  $("worker-mode-group").replaceChildren(
    ...allowed.map((m) => {
      const b = button(workerModes[m], () => {
        $("worker-mode").value = m;
        renderWorkerMode();
      });
      b.className = "segment";
      b.setAttribute("role", "radio");
      b.setAttribute("aria-checked", String(m === current));
      return b;
    }),
  );
}
// Each shared level names the native preset it starts in (ox-desk-core modes.rs preset),
// so the labels and one-line descriptions are the desktop's for that runtime.
const permissionPresets = {
  codex: {
    read: ["Read only", "Read files. Ask before writing or running outside the sandbox."],
    write: ["Workspace", "Edit the workspace. Ask you before leaving the sandbox."],
    classifier: ["Auto-review", "Edit the workspace. Let Codex review approval requests."],
    yolo: ["YOLO", "Full filesystem and network access. Run commands without approval."],
  },
  claude: {
    read: ["Plan mode", "Let Claude Code explore and plan before making changes."],
    write: ["Accept edits", "Let Claude Code apply edits without asking about each edit."],
    classifier: ["Auto", "Let Claude Code’s classifier review tool actions before they run."],
    yolo: ["YOLO · bypass permissions", "Run Claude Code tools without permission prompts."],
  },
};
function labelPermissions(select, runtime, note) {
  const table = permissionPresets[runtime] ?? permissionPresets.codex;
  for (const o of select.options) if (table[o.value]) o.textContent = table[o.value][0];
  note.textContent =
    select.value === "inherit"
      ? "Uses this session's current permission mode."
      : (table[select.value]?.[1] ?? "");
}
function renderWorkerPermission() {
  const yolo = $("worker-permission").value === "yolo";
  labelPermissions($("worker-permission"), $("worker-runtime").value, $("worker-permission-note"));
  $("worker-warning").textContent =
    "YOLO runs this worker's commands and edits without asking. Choosing it here confirms that.";
  $("worker-warning").hidden = !yolo;
}
$("worker-runtime").onchange = () => {
  // A new runtime starts from its own subscription or first starred model, not the old one.
  workerSeed = null;
  fillWorkerModels();
  renderWorkerMode();
  renderWorkerPermission();
};
$("worker-permission").onchange = renderWorkerPermission;
$("start-worker").onclick = () => {
  // Start from the parent session's route, as the desktop sheet does.
  workerSubmission = null;
  $("worker-submit").disabled = false;
  workerHosts = [];
  $("worker-placement").hidden = !detailLink?.placement;
  $("worker-host").replaceChildren(new Option("Same as parent","parent"));
  $("worker-needs").hidden = $("worker-needs").disabled = true;
  for (const id of ["worker-memory","worker-cpus","worker-os"]) $(id).value = "";
  $("worker-gpu").checked = false;
  $("worker-inventory-note").textContent = detailLink?.placement ? "Loading worker hosts…" : "";
  const route = selected?.scope?.route;
  if (route) {
    $("worker-runtime").value = route.runtime;
    workerSeed = { model: route.billing?.api ?? "" };
  }
  fillWorkerModels();
  renderWorkerMode();
  renderWorkerPermission();
  $("worker-form").hidden = false;
  requestWorkerHosts();
  $("worker-task").focus();
  loadModelChoices();
};
$("worker-cancel").onclick = () => ($("worker-form").hidden = true);
$("worker-form").onsubmit = (e) => {
  e.preventDefault();
  if (!selected?.id) return;
  const task = $("worker-task").value.trim();
  if (!task) {
    notice("Write the task for the worker.");
    return;
  }
  const runtime = $("worker-runtime").value,
    permission = $("worker-permission").value;
  // No model chosen means the runtime's subscription, which must be configured.
  if (
    !workerModelValue() &&
    !(workerRoutes() || routes).some((r) => r.runtime === runtime && r.billing === "subscription")
  ) {
    notice(
      `${runtimeName(runtime)} has no subscription configured on this host. Choose an OX API model or sign in on the host.`,
    );
    return;
  }
  const start = {
    parent: selected.id,
    runtime,
    model: workerModelValue(),
    mode: modesFor(runtime).includes($("worker-mode").value)
      ? $("worker-mode").value
      : "session",
    task,
    permission,
    // Choosing YOLO in the sheet is the confirmation, as for any YOLO change.
    yolo_confirmed: permission === "yolo",
  };
  if (detailLink?.placement) {
    if ($("worker-host").value !== "project") start.host = $("worker-host").value;
    if (start.host === "auto") {
      start.needs = {gpu:$("worker-gpu").checked};
      if ($("worker-memory").value) start.needs.memory_gb = $("worker-memory").valueAsNumber;
      if ($("worker-cpus").value) start.needs.cpus = $("worker-cpus").valueAsNumber;
      if ($("worker-os").value.trim()) start.needs.os = $("worker-os").value.trim();
    }
  }
  if (workerSubmission) return;
  workerSubmission = detailLink?.act({ start_worker: start }, null, (ok, message, id) => {
    if (!ok && id === workerSubmission) {
      workerSubmission = null;
      $("worker-submit").disabled = false;
      notice(message || "The host refused this worker.");
    }
  });
  $("worker-submit").disabled = Boolean(workerSubmission);
};
$("draft").oninput = () => {
  mentionInput();
  if (selected)
    drafts.set(keyOf(selected.scope, selected.id), $("draft").value);
  syncDraftHeight();
  updateComposerState();
};
const keepDraftFocus = (event) => {
  if (document.activeElement === $("draft")) event.preventDefault();
};
$("send").onpointerdown = keepDraftFocus;
$("send").onmousedown = keepDraftFocus;
$("composer").onsubmit = (e) => {
  e.preventDefault();
  if ($("send").disabled || !selected) return;
  const { scope, id } = selected,
    link = detailLink,
    s = link.model.scopes.get(scopeKey(scope)),
    row = s?.sessions.find((x) => x.id === id);
  const content = $("draft").value;
  const owner = keyOf(scope, id);
  const images = imagesFor(owner);
  const action = images.length
      ? { send_images: [id, content, images.map((v) => v.image)] }
      : { send_text: [id, content] };
  pendingNew = { owner };
  const sentNow = { content, images };
  unacked.set(owner, sentNow);
  if (
    !link.act(action, null, (ok) => {
      if (unacked.get(owner) === sentNow) unacked.delete(owner);
      pendingNew = null;
      if (ok && !sentNow.restored) {
        clearImages(owner, images);
        if (drafts.get(owner) === content) drafts.delete(owner);
        if (
          keyOf(selected?.scope || scope, selected?.id) === owner &&
          $("draft").value === content
        ) {
          $("draft").value = "";
          syncDraftHeight();
        }
        notice(id ? "" : "Message sent. Your new session is in Sessions.");
      }
      repaint();
    })
  ) {
    pendingNew = null;
    unacked.delete(owner);
  }
  repaint();
};
const closeCamera = setupCamera((secret) => {
  code = secret;
  $("pair-link").value = "";
  notice();
  pairingPage();
  $("device-label").focus();
});
const ios =
  /iPhone|iPad|iPod/.test(navigator.userAgent) ||
  (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const standalone =
  navigator.standalone === true ||
  matchMedia("(display-mode: standalone)").matches;
$("pair-form").onsubmit = async (e) => {
  e.preventDefault();
  try {
    closeCamera();
    const secret = code ?? pairingCode($("pair-link").value, location.origin);
    if (!/^[A-Za-z0-9_-]{43}$/.test(secret))
      throw new Error("Request a fresh pairing QR in OX Desktop.");
    await pair(secret, $("device-label").value);
    $("pair-link").value = "";
    code = null;
    key = await deviceKey();
    start(true);
  } catch (err) {
    if (err.status === 403) {
      code = null;
      pairingPage();
      notice(
        "Pairing was refused. Get a fresh code in OX Desktop for this Tailscale account.",
      );
    } else notice(err.message);
  }
};
function notificationGuide(text, buttonLabel) {
  $("notification-guide").textContent = text;
  $("notifications").hidden = !buttonLabel;
  if (buttonLabel) $("notifications").textContent = buttonLabel;
}
const alertsKey = "ox-phone-alerts-dismissed";
function alertsDismissed() {
  try {
    return localStorage.getItem(alertsKey) === "1";
  } catch {
    return false;
  }
}
function setAlertsDismissed(on) {
  try {
    if (on) localStorage.setItem(alertsKey, "1");
    else localStorage.removeItem(alertsKey);
  } catch {
    // Private windows may refuse storage; the banner then simply returns.
  }
}
function notificationSetup(justPaired) {
  if (justPaired) {
    setAlertsDismissed(false);
    $("notification-setup").open = true;
  }
  const supportsPush =
    "serviceWorker" in navigator &&
    "Notification" in window &&
    "PushManager" in window;
  if (ios && !standalone) {
    notificationGuide(
      "On iPhone, tap Share → Add to Home Screen in Safari. Turn on Open as Web App if shown. Open OX from its icon, scan a fresh pairing QR, then enable notifications there.",
    );
  } else if (!supportsPush) {
    notificationGuide(
      "This browser cannot receive session notifications. On iPhone, use the Home Screen app on iOS 16.4 or later.",
    );
  } else if (Notification.permission === "denied") {
    notificationGuide(
      ios
        ? "Notifications are off. In iPhone Settings → Notifications, choose the name of this web app and turn on Allow Notifications. Then return to OX."
        : "Allow notifications for this site in your browser settings, then return to OX.",
    );
  } else {
    notificationGuide(
      "Get alerts when a session needs approval or an answer, finishes a turn, or fails. Tap Enable notifications, then Allow.",
      "Enable notifications",
    );
    if (Notification.permission === "granted") registerNotifications(false);
  }
  // One-time banner until alerts are on or the user dismisses it; then it lives in the menu.
  const granted =
    "Notification" in window && Notification.permission === "granted";
  const dismissed = alertsDismissed();
  $("notification-banner").hidden = granted || dismissed;
  $("open-notifications").hidden = granted || !dismissed;
}
$("notification-dismiss").onclick = () => {
  setAlertsDismissed(true);
  notificationSetup(false);
};
$("open-notifications").onclick = () => {
  closeActionMenus();
  setAlertsDismissed(false);
  notificationSetup(false);
  $("notification-setup").open = true;
};
let registering = false;
async function registerNotifications(explicit) {
  if (registering) return;
  registering = true;
  $("notifications").disabled = true;
  try {
    if (await notifications(key, explicit)) {
      notificationGuide(
        "Notifications enabled on this device. Tap an alert to open the session.",
      );
    }
  } catch (err) {
    if (Notification.permission === "denied") notificationSetup(false);
    else
      notificationGuide("Notifications are off. " + err.message, "Try again");
  } finally {
    registering = false;
    $("notifications").disabled = false;
  }
}
$("notifications").onclick = () => registerNotifications(true);
function pairingPage() {
  page("pair");
  net("Pair device");
  const scanned = Boolean(code);
  $("step-scan").toggleAttribute("aria-current", !scanned);
  $("step-name").toggleAttribute("aria-current", scanned);
  $("scan-pair").hidden = scanned;
  $("pair-submit").classList.toggle("primary", scanned);
  $("paste-pair").hidden = Boolean(code);
  $("pair-link").required = !code;
  $("pair-hint").textContent = code
    ? "Give this device a name to access your OX sessions."
    : ios && standalone
      ? "This Home Screen app has its own pairing. On your laptop, open OX Desktop → Settings → Remote access and create a fresh QR. Scan it here."
      : "In OX Desktop → Settings → Remote access, create a pairing QR. Scan it here or paste its link below.";
}
function start(justPaired = false) {
  page("catalog");
  notice();
  globalLink?.close();
  globalLink = new Link(null, global);
  repaint();
  notificationSetup(justPaired);
}
if ("serviceWorker" in navigator)
  navigator.serviceWorker.register("/sw.js").catch(() => {});
let hiddenAt = 0;
window.addEventListener("offline", () => {
  globalLink?.reconnect();
  detailLink?.reconnect();
});
window.addEventListener("online", () => {
  globalLink?.reconnect();
  detailLink?.reconnect();
});
document.addEventListener("visibilitychange", () => {
  if (document.hidden) hiddenAt = Date.now();
  else if (hiddenAt && Date.now() - hiddenAt > 60000) {
    globalLink?.reconnect();
    detailLink?.reconnect();
  }
});
document.addEventListener("visibilitychange", () => {
  if (!document.hidden && key && !$("catalog").hidden) notificationSetup(false);
});
navigator.serviceWorker?.addEventListener("message", (e) => {
  if (typeof e.data?.openThread === "string") {
    if (key && $("pair").hidden) home();
    notificationTarget.request(e.data.openThread);
    // An existing window may have finished discovery before the alert arrived.
    if (globalLink?.discoveryResult && globalLink.socket?.readyState === WebSocket.OPEN)
      globalLink.discover();
    repaint();
  }
});
try {
  key = await deviceKey();
  if (code || !key) pairingPage();
  else start();
} catch {
  page("pair");
  notice(
    "This browser cannot store a device key. Open the HTTPS Serve address in a supported browser.",
  );
}

$("operator-home").onclick = () => openPhoneTerminal("shell");
$("operator-project").onclick = () => {
  if (selected) openPhoneTerminal("shell", selected.scope.project);
};

$("operator-session").onclick = () => {
  if (selected?.id)
    openPhoneTerminal("native-session", null, {
      scope: selected.scope,
      thread: selected.id,
    });
};
