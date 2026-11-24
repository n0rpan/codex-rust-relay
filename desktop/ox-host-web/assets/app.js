import { openPhoneTerminal } from "./operator.js";
import {
  copyButton,
  messageText,
  showReview,
  imagesFor,
  clearImages,
  setComposerOwner,
  updateExtras,
  mentionInput,
  forgetComposer,
} from "./composer-tools.js";
import { pairingCode, setupCamera } from "./pair-camera.js";
import * as wire from "./wire.js";
import { SessionControls } from "./controls.js";
import { setupSettings } from "./settings.js";
import { scopeKey, Projection, emptyScope } from "./model.js";
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
const routesLabel = (r) =>
  r.runtime.replace(/^./, (x) => x.toUpperCase()) +
  " · " +
  (r.billing === "subscription" ? "Subscription" : r.billing.api);
const leaf = (path) => path.split("/").filter(Boolean).pop() || path;
const keyOf = (scope, id) => scopeKey(scope) + " " + id;
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
  if (id !== "settings") phoneSettings?.reset?.();
  for (const view of ["pair", "catalog", "create", "conversation", "settings"])
    $(view).hidden = view !== id;
  if (id !== "pair" && !$("camera-panel").hidden) closeCamera();
  $("location").textContent = location.hostname.split(".")[0];
  $("location").title = location.host;
}
const hash = new URLSearchParams(location.hash.slice(1));
let code = hash.get("pair"),
  requested = hash.get("thread");
if (location.hash) history.replaceState(null, "", location.pathname);
let key,
  globalLink,
  detailLink,
  selected = null,
  filter = "all",
  routes = [],
  projects = [],
  scheduled = false;
let cold = new Map(),
  drafts = new Map(),
  timelineRows = new Map(),
  askKeys = "",
  pendingNew = null,
  visibleRows = 200;
const global = new Projection(true);
const controls = new SessionControls();
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
          timelineRows.clear();
          pickedProjects.clear();
          forgetComposer();
          routes = [];
          projects = [];
          for (const id of ["list", "timeline", "asks", "review-content"])
            $(id).replaceChildren();
          $("draft").value = "";
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
      const liveCatalog =
        !this.scope && msg.capabilities.includes("live_catalog");
      this.send(
        liveCatalog
          ? { type: "watch_catalog" }
          : wire.subscribe(this.scope, this.model.cursor),
      );
      if (!this.scope) {
        this.discovery = crypto.randomUUID();
        this.discovering = new Map();
        this.send(wire.discover(this.discovery));
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
        drafts.delete(old);
        open(msg.scope, msg.thread);
      }
      return;
    }
    if (msg.type === "ack" || msg.type === "error") {
      if (msg.type === "error") this.opens.delete(msg.id);
      const cb = this.pending.get(msg.id);
      this.pending.delete(msg.id);
      if (msg.type === "error")
        notice(
          this.scope
            ? routesLabel(this.scope.route) + " · " + msg.message
            : msg.message,
        );
      cb?.(msg.type === "ack", msg.message);
      return;
    }
    if (msg.type === "snapshot" || msg.type === "delta") {
      const changed = this.model.apply(msg);
      if (!this.ready) this.connected();
      if (changed) repaint();
    }
  }
  connected() {
    this.ready = true;
    this.attempt = 0;
    if (this.scope && selected?.id) {
      this.act({ select_thread: selected.id });
    }
    repaint();
  }
  lost() {
    if (this.closed) return;
    this.ready = false;
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
    return true;
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
    clearTimeout(this.timer);
    this.socket?.close();
    this.pending.clear();
  }
}
function home() {
  detailLink?.close();
  detailLink = null;
  selected = null;
  pendingNew = null;
  page("catalog");
  notice();
  repaint();
}
function open(scope, id) {
  controls.reset();
  detailLink?.close();
  selected = { scope, id, revealLatest: Boolean(id) };
  setComposerOwner(keyOf(scope, id), repaint, notice);
  pendingNew = null;
  visibleRows = 200;
  timelineRows.clear();
  askKeys = "";
  $("asks").replaceChildren();
  $("timeline").replaceChildren();
  $("draft").value = drafts.get(keyOf(scope, id)) || "";
  page("conversation");
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
  if (requested) {
    const ref = requested;
    const cut = ref.lastIndexOf("~");
    const id = cut < 0 ? ref : ref.slice(0, cut);
    const matches = rows.filter((v) => v.session.id === id);
    if (matches.length) {
      requested = null;
      (async () => {
        for (const hit of matches) {
          const b = hit.scope.route.billing;
          const bytes = new TextEncoder().encode(
            JSON.stringify([
              hit.scope.project,
              hit.scope.route.runtime,
              typeof b === "string" ? null : b.api,
              hit.session.id,
            ]),
          );
          const hash = btoa(
            String.fromCharCode(
              ...new Uint8Array(await crypto.subtle.digest("SHA-256", bytes)),
            ),
          )
            .replaceAll("+", "-")
            .replaceAll("/", "_")
            .replaceAll("=", "");
          if (
            (cut >= 0 && hash === ref.slice(cut + 1)) ||
            (cut < 0 && matches.length === 1)
          ) {
            open(hit.scope, id);
            return;
          }
        }
        notice(
          "The notification’s session is unavailable for this project and billing route.",
        );
      })();
    }
  }
  const needle = $("find-sessions").value.trim().toLocaleLowerCase();
  const byProject = new Map();
  for (const row of rows) {
    const { scope, session } = row,
      s = global.scopes.get(scopeKey(scope)),
      needsInput =
        s?.detail?.request_threads?.includes(session.id) ||
        s?.asks?.some((a) => a.thread === session.id);
    if (s?.detail?.archived?.includes(session.id)) continue;
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
      (filter === "active" &&
        !session.capabilities.some((c) => c === "stop" || c === "steer"))
    )
      continue;
    const list = byProject.get(scope.project) ?? [];
    list.push({ ...row, needsInput });
    byProject.set(scope.project, list);
  }
  const frag = document.createDocumentFragment();
  for (const [project, list] of [...byProject].sort((a, b) =>
    a[0].localeCompare(b[0]),
  )) {
    frag.append(el("h2", "project", project));
    const done = new Set(),
      children = new Map();
    for (const entry of list)
      if (entry.session.parent) {
        const k = keyOf(entry.scope, entry.session.parent),
          group = children.get(k) ?? [];
        group.push(entry);
        children.set(k, group);
      }
    const add = (entry, depth) => {
      const { scope, session, needsInput } = entry,
        key = keyOf(scope, session.id);
      if (done.has(key)) return;
      done.add(key);
      const b = button("", () => open(scope, session.id));
      b.dataset.source =
        session.parent || session.detail?.worker || session.detail?.job
          ? "worker"
          : scope.route.billing !== "subscription"
            ? "api"
            : scope.route.runtime;
      b.dataset.needsInput = String(Boolean(needsInput));
      b.dataset.passive = String(
        !needsInput && ["saved", "idle"].includes(session.status),
      );
      b.dataset.threadId = session.id;
      b.className = "session" + (depth ? " nested" : "");
      const head = el("div", "session-top");
      head.append(
        el("span", "session-name", session.title || "Untitled session"),
        el("span", "status", needsInput ? "Needs input" : session.status),
      );
      b.append(
        head,
        el("div", "meta", (depth ? "↳ " : "") + routesLabel(scope.route)),
      );
      frag.append(b);
      for (const child of children.get(key) || []) add(child, depth + 1);
    };
    list.sort((a, b) => b.session.updated_at - a.session.updated_at);
    for (const entry of list) if (!entry.session.parent) add(entry, 0);
    for (const entry of list) add(entry, entry.session.parent ? 1 : 0);
  }
  if (!frag.childNodes.length)
    frag.append(
      el(
        "p",
        "empty",
        filter === "requests"
          ? "No pending requests."
          : filter === "active"
            ? "No active sessions."
            : "No sessions found.",
      ),
    );
  $("list").replaceChildren(frag);
  $("new").disabled = !globalLink?.ready || !routes.length;
}
function showAsks(asks) {
  const stamp = JSON.stringify(asks);
  if (stamp === askKeys) return;
  askKeys = stamp;
  const root = $("asks");
  root.replaceChildren();
  for (const a of asks) {
    const card = el("article", "ask");
    card.append(el("h2", "", a.title));
    if (a.detail) card.append(el("p", "", a.detail));
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
        choice.className = "decision";
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

function toolInfo(item) {
  const [kind, data] = Object.entries(item.detail?.item ?? {})[0] ?? [];
  const labels = {
    CommandExecution:
      data?.command
        ?.replace(/^(?:\S*\/)?(?:bash|zsh|sh)\s+-[a-z]*c\s+['"]?/i, "")
        .slice(0, 120) || "Shell command",
    FileChange: "File changes",
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
        output.textContent = node.open ? toolText(row.item, row.info) : "";
      };
    }
    if (row.item !== item || row.revision !== item.revision) {
      row.item = item;
      row.revision = item.revision;
      row.info = info;
      row.summary.textContent = info.title;
      if (row.node.open) row.output.textContent = toolText(item, info);
    }
    next.set(item.id, row);
  }
  syncChildren(
    group.list,
    [...next.values()].map((r) => r.node),
  );
  group.children = next;
}

function transcript(items) {
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
      if (row.item !== i || row.revision !== i.revision) {
        row.item = i;
        row.revision = i.revision;
        if (["assistant", "result", "plan"].includes(i.role))
          messageText(row.text, i.text);
        else row.text.textContent = i.text;
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
  const worker = Boolean(row?.detail?.job);
  controls.update({
    scope,
    id,
    state: s,
    row: s?.sessions.find((x) => x.id === id),
    ready: Boolean(detailLink?.ready),
    act: (action, cb) => detailLink?.act(action, null, cb),
    archived: home,
    notice,
    routesLabel,
  });
  $("title").textContent = row?.title || (id ? "Session" : "New session");
  $("context").textContent =
    leaf(scope.project) +
    " · " +
    routesLabel(row?.detail?.route || scope.route);
  const timeline = s?.transcripts[id];
  const inputCaps = id ? caps : (s?.capabilities ?? []);
  updateExtras(s, inputCaps, detailLink, id);
  showReview(timeline?.detail?.reviews);
  const job = row?.detail?.job;
  if (job) {
    const snapshot = job.snapshot ?? {};
    const items = (snapshot.messages ?? []).map((m, i) => ({
      id: String(i),
      turn: id,
      role: m.role,
      text: m.text,
      revision: 0,
    }));
    for (const [tag, text] of Object.entries({
      progress: snapshot.progress,
      result: snapshot.result,
      error: job.error ?? snapshot.error,
    })) {
      if (text) items.push({ id: tag, turn: id, role: tag, text, revision: 0 });
    }
    transcript(items);
  } else transcript(timeline?.items || []);
  $("older").hidden = !timeline?.detail?.older;
  $("older").onclick = () => detailLink.act({ load_older: id });
  $("refresh-worker").hidden = !job || !caps.includes("refresh");
  $("refresh-worker").onclick = () =>
    detailLink.act({ worker_control: [id, "refresh"] });
  showAsks((s?.asks || []).filter((a) => a.thread === id));
  if (selected.revealLatest && (timeline?.items?.length || job)) {
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
  $("operator-session").hidden =
    !id ||
    !!row?.parent ||
    !!row?.detail?.job ||
    !caps.includes("send") ||
    caps.includes("stop");
  $("stop").hidden = !caps.includes("stop");
  $("stop").disabled = !detailLink?.ready;
  $("stop").onclick = () =>
    detailLink.act(worker ? { worker_control: [id, "stop"] } : { stop: id });
}
function render() {
  if (!$("pair").hidden) return;
  const live = globalLink?.ready && (!selected || detailLink?.ready);
  net(live ? "Connected" : "Reconnecting", live);
  if (!$("catalog").hidden) catalog();
  if (!$("conversation").hidden) conversation();
}
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
  options($("route"), routes, routes.map(routesLabel));
  page("create");
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
$("draft").oninput = () => {
  mentionInput();
  if (selected)
    drafts.set(keyOf(selected.scope, selected.id), $("draft").value);
  conversation();
};
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
  const action = row?.detail?.job
    ? { worker_control: [id, { steer: content }] }
    : images.length
      ? { send_images: [id, content, images.map((v) => v.image)] }
      : { send_text: [id, content] };
  pendingNew = { owner };
  if (
    !link.act(action, null, (ok) => {
      pendingNew = null;
      if (ok) {
        clearImages(owner, images);
        if (drafts.get(owner) === content) drafts.delete(owner);
        if (
          keyOf(selected?.scope || scope, selected?.id) === owner &&
          $("draft").value === content
        )
          $("draft").value = "";
        notice(id ? "" : "Message sent. Your new session is in Sessions.");
      }
      repaint();
    })
  )
    pendingNew = null;
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
function notificationSetup(justPaired) {
  if (justPaired) $("notification-setup").open = true;
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
}
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
    requested = e.data.openThread;
    home();
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
