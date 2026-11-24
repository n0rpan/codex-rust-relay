// Render only confirmed host projections. An accepted request may still be awaiting its native runtime.
const $ = (id) => document.getElementById(id);
const modes = {
  read_only: "Read only",
  workspace: "Workspace",
  auto_review: "Auto-review",
  yolo: "YOLO",
  configured: "Configured",
  default: "Default",
  auto: "Auto",
  accept_edits: "Accept edits",
  plan: "Plan mode",
  bypass_permissions: "YOLO · bypass permissions",
};
const bypass = (value) => value === "yolo" || value === "bypass_permissions";
const button = (label, click) => {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  b.onclick = click;
  return b;
};
function choices(select, rows, value) {
  const signature = JSON.stringify([rows, value]);
  if (select.dataset.selection === signature) return;
  select.dataset.selection = signature;
  select.replaceChildren(...rows.map(([v, l]) => new Option(l, v)));
  if (rows.some(([v]) => v === value)) select.value = value;
}
function field(parent, id, label, rows, value, enabled, change) {
  let box = $(id + "-field");
  if (!box) {
    box = document.createElement("label");
    box.id = id + "-field";
    box.append(label);
    const select = document.createElement("select");
    select.id = id;
    box.htmlFor = id;
    box.append(select);
    parent.append(box);
  }
  box.hidden = !rows.length;
  const select = $(id);
  choices(select, rows, value);
  select.disabled = !enabled;
  select.onchange = () => {
    const target = select.value;
    select.value = value;
    change(target);
  };
}
function nativeChoices(schema) {
  const rows = new Map(Object.entries(schema.session_commands ?? {}));
  rows.set("/status", "Session status");
  if (schema.methods?.includes("thread/compact/start"))
    rows.set("/compact", "Compact context");
  if (schema.goals && schema.methods?.includes("thread/goal/get"))
    rows.set("/goal", "Session goal");
  for (const cmd of ["/terminal", "/new", "/resume", "/speed", "/rename"])
    rows.delete(cmd);
  return [...rows]
    .map(([cmd, hint]) => [cmd, hint ? cmd + " · " + hint : cmd])
    .sort(([a], [b]) => a.localeCompare(b));
}
function usage(state, timeline, act, enabled) {
  const usage = timeline?.detail?.usage;
  const host = state?.detail?.usage;
  const entries = [];
  if (usage?.total) {
    const t = usage.total;
    entries.push(
      (t.totalTokens ?? 0).toLocaleString() +
        " tokens · " +
        (t.inputTokens ?? 0).toLocaleString() +
        " input · " +
        (t.outputTokens ?? 0).toLocaleString() +
        " output",
    );
    if (usage.modelContextWindow)
      entries.push(
        Math.round(
          (100 * (usage.last?.totalTokens ?? 0)) / usage.modelContextWindow,
        ) + "% context used",
      );
  }
  for (const [key, bucket] of Object.entries(host?.limits ?? {})) {
    for (const window of [bucket.primary, bucket.secondary].filter(Boolean)) {
      entries.push(
        (bucket.limitName || key) +
          " · " +
          window.usedPercent +
          "% used" +
          (window.resetsAt
            ? " · resets " +
              new Date(window.resetsAt * 1000).toLocaleTimeString()
            : ""),
      );
    }
  }
  if (host?.error) entries.push(host.error);
  const box = $("session-usage");
  const stamp = JSON.stringify([entries, enabled, host?.loading]);
  if (box.dataset.state === stamp) return;
  box.dataset.state = stamp;
  box.replaceChildren();
  for (const text of entries) {
    const p = document.createElement("p");
    p.textContent = text;
    box.append(p);
  }
  if (enabled) {
    const refresh = button("Refresh account limits", () =>
      act("read_account_limits"),
    );
    refresh.disabled = !!host?.loading;
    box.append(refresh);
  }
}
export class SessionControls {
  constructor() {
    this.reset();
    $("access-mode").onclick = () => {
      this.show(true);
      $("permission").focus();
    };
    $("open-settings").onclick = () => this.show($("session-settings").hidden);
    $("permissions-form").onsubmit = (e) => {
      e.preventDefault();
      const { id } = this.ctx;
      const value = $("permission").value;
      if ($("apply-permission").disabled || !this.allowed.includes(value))
        return;
      this.apply(
        id === null ? { set_next_access: value } : { set_access: [id, value] },
        "permission",
      );
    };
    $("permission").onchange = () => this.render();
    $("native-command-form").onsubmit = (e) => {
      e.preventDefault();
      const cmd = $("native-command").value;
      if (
        !this.commandRows.some(([v]) => v === cmd) ||
        !this.enabled("native_commands")
      )
        return;
      const args = $("command-args").value.trim();
      this.apply(
        { native_command: [this.ctx.id, args ? cmd + " " + args : cmd] },
        "command",
      );
    };
  }
  reset() {
    this.ctx = null;
    this.pending = new Set();
    this.error = "";
    this.actionStamp = null;
    $("session-controls").hidden = true;
    this.show(false);
    $("command-args").value = "";
    $("permission").removeAttribute("data-selection");
  }
  show(open) {
    $("session-settings").hidden = !open;
    $("open-settings").setAttribute("aria-expanded", String(open));
    $("access-mode").setAttribute("aria-expanded", String(open));
    if (open) this.render();
  }
  enabled(cap) {
    return !!this.ctx?.ready && this.caps.includes(cap);
  }
  update(ctx) {
    this.ctx = ctx;
    this.render();
  }
  apply(action, kind, after) {
    const ctx = this.ctx,
      pending = this.pending;
    if (!ctx?.ready || pending.has(kind)) return;
    this.error = "";
    ctx.notice();
    pending.add(kind);
    this.render();
    const finish = (ok, msg) => {
      if (this.pending !== pending) return;
      pending.delete(kind);
      if (!ok)
        this.error =
          ctx.routesLabel(ctx.scope.route) +
          " · " +
          (msg || "Disconnected before confirmation.");
      if (ok && after) after();
      this.render();
    };
    if (!ctx.act(action, finish)) finish(false);
  }
  render() {
    if (!this.ctx) return;
    const { state, id, row, scope, ready } = this.ctx,
      draft = id === null;
    const detail = state?.detail ?? {},
      own = row?.detail ?? {};
    const present = !!state && (draft || !!row);
    $("session-controls").hidden = !present;
    if (!present) return;
    this.caps = (draft ? state.capabilities : row.capabilities) ?? [];
    const confirmed = draft ? detail.applied_draft_access : own.access;
    const planned = draft ? detail.next_access : null;
    const current = confirmed ?? planned;
    const badge = $("access-mode");
    badge.textContent = confirmed
      ? (modes[confirmed] ?? confirmed)
      : planned
        ? (modes[planned] ?? planned) + " on start"
        : "Permission mode";
    badge.dataset.bypass = String(bypass(confirmed));
    if (bypass(confirmed)) {
      badge.textContent = "YOLO";
      badge.setAttribute(
        "aria-label",
        modes[confirmed] + ", change permission mode",
      );
    } else {
      badge.removeAttribute("aria-label");
    }
    // A worker's mode stays within its recorded ceiling. The host enforces this too.
    const ceiling = own.origin?.ceiling;
    const withinCeiling = (x) =>
      !ceiling ||
      ["read_only", "default", "plan"].includes(x) ||
      (ceiling === "write" && ["workspace", "accept_edits"].includes(x));
    this.allowed = (detail.access_choices ?? []).filter(
      (x) => Object.hasOwn(modes, x) && withinCeiling(x),
    );
    if ($("session-settings").hidden) return;
    const options = this.allowed.map((v) => [v, modes[v]]);
    if (!this.allowed.includes(current))
      options.unshift([
        "",
        current ? (modes[current] ?? current) : "Choose a mode",
      ]);
    choices($("permission"), options, current ?? "");
    $("permission").disabled =
      !this.enabled("access") || this.pending.has("permission");
    $("apply-permission").disabled =
      $("permission").disabled ||
      !this.allowed.includes($("permission").value) ||
      $("permission").value === current;
    const settings = (draft ? detail.default_options : own.options) ?? {};
    const schema = (draft ? null : own.command_caps) ?? detail.commands ?? {};
    const model = settings.model || schema.model || "";
    const models = schema.models ?? {};
    const route = own.route ?? scope.route,
      billing = route.billing;
    const compatible = (id) =>
      typeof billing === "string" ||
      billing.api === id ||
      (scope.route.runtime === "codex" &&
        (detail.provider !== "codeox" ||
          (id.includes("/") &&
            id.split("/")[0] === billing.api.split("/")[0])));
    const modelRows = Object.entries(models)
      .filter(([k]) => compatible(k))
      .map(([k, v]) => [k, v.label && v.label !== k ? v.label + " · " + k : k]);
    const edit =
      this.enabled("model_options") &&
      !own.changing &&
      !this.pending.has("options");
    const change = (choice) =>
      this.apply({ model_options: [id, choice] }, "options");
    const parent = $("model-fields"),
      activeModel = models[model];
    field(parent, "session-model", "Model", modelRows, model, edit, (v) =>
      change({ model: v }),
    );
    const efforts = (activeModel?.efforts ?? []).map((v) => [
      v.reasoningEffort,
      v.reasoningEffort,
    ]);
    field(
      parent,
      "session-effort",
      "Reasoning effort",
      efforts,
      settings.effort ?? activeModel?.default_effort ?? "",
      edit,
      (v) => change({ effort: v }),
    );
    const speeds = schema.speed
      ? [
          ["", "Default"],
          ...(schema.catalog?.[model] ?? []).map((v) => [v.id, v.name]),
        ]
      : [];
    field(
      parent,
      "session-speed",
      "Speed",
      speeds,
      settings.tier ?? "",
      edit,
      (v) => change({ speed: v || null }),
    );
    this.commandRows = nativeChoices(schema);
    $("native-command-form").hidden =
      !this.caps.includes("native_commands") || !this.commandRows.length;
    choices($("native-command"), this.commandRows, undefined);
    $("native-command-form").querySelector("button").disabled =
      !ready || this.pending.has("command");
    this.threadActions();
    const timeline = state.transcripts[id];
    const readLimits =
      scope.route.runtime === "codex" &&
      billing === "subscription" &&
      schema.methods?.includes("account/rateLimits/read");
    usage(
      state,
      timeline,
      (action) => this.apply(action, "usage"),
      !!readLimits && ready,
    );
    const status =
      this.error ||
      timeline?.detail?.command_notice ||
      detail.notice ||
      detail.error;
    $("control-status").hidden = !status;
    $("control-status").textContent = typeof status === "string" ? status : "";
  }
  threadActions() {
    const { id, ready, archived } = this.ctx;
    const stamp = JSON.stringify([
      id,
      ready,
      this.caps,
      this.pending.has("thread"),
    ]);
    if (this.actionStamp === stamp) return;
    this.actionStamp = stamp;
    const container = $("thread-actions");
    container.replaceChildren();
    if (!id) return;
    for (const [cap, label] of [
      ["fork", "Fork"],
      ["rename", "Rename"],
      ["generate_title", "Generate title"],
      ["archive", "Archive"],
    ]) {
      if (!this.caps.includes(cap)) continue;
      const b = button(label, () => {
        let action;
        if (cap === "rename") {
          const value = window
            .prompt("New session name", this.ctx.row.title)
            ?.trim();
          if (!value) return;
          action = { rename_thread: [id, value] };
        } else if (cap === "archive") {
          if (!window.confirm("Archive this session?")) return;
          action = { archive_thread: id };
        } else
          action =
            cap === "fork" ? { fork_thread: id } : { generate_title: id };
        this.apply(action, "thread", cap === "archive" ? archived : undefined);
      });
      b.disabled = !ready || this.pending.has("thread");
      container.append(b);
    }
  }
}
