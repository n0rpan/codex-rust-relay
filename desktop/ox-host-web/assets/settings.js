// The local gateway owns validation, revisions and secrets. Keep unfinished edits in this page.
const connection = [
  ["name", "Display name"],
  ["base_url", "API endpoint", "url"],
  [
    "wire_api",
    "Protocol",
    ["responses", "chat_completions", "anthropic_messages"],
  ],
  ["env_key", "Key environment variable"],
  ["discovery", "Discover models", "bool"],
  ["models_url", "Model catalog URL", "url"],
  ["max_output_tokens", "Maximum output tokens", "integer"],
];
const adapters = {
  chat_completions: [
    [
      "max_tokens_field",
      "Output token field",
      ["max_tokens", "max_completion_tokens"],
    ],
    ["chat_reasoning_effort", "Reasoning effort", ["drop", "forward"]],
    ["developer_role", "Developer role", ["system", "developer"]],
    ["include_stream_usage", "Include stream usage", "bool"],
  ],
  anthropic_messages: [
    ["anthropic_version", "Anthropic API version"],
    ["anthropic_thinking", "Thinking mode", ["off", "adaptive", "manual"]],
    ["anthropic_thinking_budget_tokens", "Thinking token budget", "integer"],
    ["output_config_effort", "Send output effort", "bool"],
  ],
};
const modelFields = [
  ["id", "Upstream model ID"],
  ["name", "Display name"],
  ["alias", "Model alias"],
  ["context_window", "Context window", "integer"],
  ["input_modalities", "Input types (comma-separated)", "list"],
  ["reasoning_efforts", "Reasoning levels (comma-separated)", "list"],
];
const searchFields = [
  ["enabled", "Enable Brave Search", "bool"],
  ["env_key", "Key environment variable"],
  ["max_results", "Maximum results", "integer"],
  ["timeout_seconds", "Timeout in seconds", "integer"],
];
const legacyFields = [
  ["name", "Display name"],
  ["model", "Model"],
  ["base_url", "API endpoint", "url"],
  ["model_catalog_url", "Model catalog URL", "url"],
  ["env_key", "Key environment variable"],
  ["wire_api", "Protocol", ["responses"]],
];

function node(tag, text, css) {
  const el = document.createElement(tag);
  if (text !== undefined) el.textContent = text;
  if (css) el.className = css;
  return el;
}
function button(label, handler, css) {
  const el = node("button", label, css);
  el.type = "button";
  el.addEventListener("click", handler);
  return el;
}
function details(label, ...children) {
  const el = node("details");
  el.append(node("summary", label), ...children);
  return el;
}
function textField(parent, label, name, type = "text") {
  const wrap = node("label", label),
    input = node("input");
  input.name = name;
  input.type = type;
  wrap.append(input);
  parent.append(wrap);
  return input;
}
function fields(parent, specs, base = {}, isNew = false) {
  const inputs = {};
  for (const [key, label, type] of specs) {
    const cell = base[key];
    const wrap = node("label", label);
    const options = type === "bool" ? ["true", "false"] : type;
    const control = Array.isArray(options) ? node("select") : node("input");
    const initial =
      cell?.type === "private"
        ? ""
        : Array.isArray(cell?.value)
          ? cell.value.join(", ")
          : String(cell?.value ?? "");
    if (Array.isArray(options)) {
      control.append(new Option("Use default", ""));
      for (const entry of new Set([...options, ...(initial ? [initial] : [])]))
        control.append(
          new Option(
            type === "bool" ? (entry === "true" ? "On" : "Off") : entry,
            entry,
          ),
        );
    } else {
      control.type =
        type === "integer" ? "number" : type === "url" ? "url" : "text";
      if (type === "integer") {
        control.min = "1";
        control.step = "1";
      }
    }
    control.name = key;
    control.value = initial;
    if (cell?.type === "private") {
      control.disabled = true;
      wrap.append(node("small", "Private value retained on the host."));
    }
    wrap.append(control);
    parent.append(wrap);
    inputs[key] = { control, initial, type };
  }
  function changes() {
    const patch = {};
    for (const [key, { control, initial, type }] of Object.entries(inputs)) {
      const value = control.value.trim();
      if (
        control.disabled ||
        (!isNew && initial === value) ||
        (isNew && !value)
      )
        continue;
      if (!value) patch[key] = null;
      else if (type === "integer") {
        if (!Number.isSafeInteger(+value) || +value < 1)
          throw Error("Enter a positive whole number.");
        patch[key] = { type: "integer", value: Number(value) };
      } else if (type === "bool")
        patch[key] = { type: "bool", value: value === "true" };
      else if (type === "list")
        patch[key] = {
          type: "list",
          value: value
            .split(",")
            .map((v) => v.trim())
            .filter(Boolean),
        };
      else patch[key] = { type: "text", value };
    }
    return patch;
  }
  return { inputs, changes };
}

export function setupSettings({ request, open, close, notifications, login }) {
  const panel = node("section");
  panel.id = "settings";
  panel.hidden = true;
  document.querySelector("main").append(panel);
  let snapshot,
    pageId = 0,
    accountsPage,
    installPrompt,
    inviteTimer;
  const localDefaults = {
    theme: "dark",
    font_size: 16,
    enter_sends: false,
    reduce_motion: false,
  };
  let local = { ...localDefaults };
  try {
    const saved = JSON.parse(localStorage.getItem("ox-phone-appearance"));
    if (
      saved &&
      ["system", "dark", "light"].includes(saved.theme) &&
      Number.isInteger(saved.font_size) &&
      saved.font_size >= 12 &&
      saved.font_size <= 24
    )
      local = {
        theme: saved.theme,
        font_size: saved.font_size,
        enter_sends: saved.enter_sends === true,
        reduce_motion: saved.reduce_motion === true,
      };
  } catch {
    /* Local display preferences are optional in a private browser. */
  }
  const systemLight = matchMedia("(prefers-color-scheme: light)");
  function applyLocal() {
    const css = document.documentElement.dataset;
    css.phoneTheme =
      local.theme === "system"
        ? systemLight.matches
          ? "light"
          : "dark"
        : local.theme;
    // Status bar and native form colours follow the active theme (house nav token).
    const light = css.phoneTheme === "light";
    document.documentElement.style.colorScheme = light ? "light" : "dark";
    document
      .querySelector('meta[name="theme-color"]')
      ?.setAttribute("content", light ? "#ffffff" : "#12131f");
    css.phoneSize = String(local.font_size);
    css.phoneReducedMotion = String(local.reduce_motion);
  }
  applyLocal();
  systemLight.addEventListener("change", applyLocal);
  window.addEventListener("ox-native-login-finished", async () => {
    const id = accountsPage;
    if (panel.hidden || id !== pageId) return;
    try {
      await load(accounts);
    } catch (err) {
      if (id === pageId && !panel.hidden) status.textContent = err.message;
    }
  });
  document.getElementById("draft").addEventListener("keydown", (event) => {
    if (
      event.key !== "Enter" ||
      event.shiftKey ||
      event.altKey ||
      event.isComposing ||
      event.keyCode === 229
    )
      return;
    if (!(local.enter_sends || event.ctrlKey || event.metaKey)) return;
    event.preventDefault();
    if (!event.repeat && !document.getElementById("send").disabled)
      document.getElementById("composer").requestSubmit();
  });
  const status = node("p", "", "phone-settings-status");
  status.setAttribute("role", "status");
  const body = node("div", undefined, "phone-settings-body");
  window.addEventListener("beforeinstallprompt", (e) => {
    e.preventDefault();
    installPrompt = e;
  });
  function screen(title, back = menu) {
    pageId++;
    clearTimeout(inviteTimer);
    status.textContent = "";
    body.replaceChildren();
    const heading = node("h1", title);
    heading.tabIndex = -1;
    panel.replaceChildren(
      button(
        back === close
          ? "← Sessions"
          : back === providers
            ? "← Providers"
            : "← Settings",
        back,
        "back",
      ),
      heading,
      status,
      body,
    );
    heading.focus({ preventScroll: true });
    window.scrollTo(0, 0);
    return pageId;
  }
  async function readPage(id, query) {
    try {
      const value = await request(query);
      return id === pageId && !panel.hidden ? value : null;
    } catch (err) {
      if (id === pageId && !panel.hidden) status.textContent = err.message;
      return null;
    }
  }
  function action(label, work, css) {
    return button(
      label,
      async (e) => {
        const control = e.currentTarget,
          before = pageId;
        control.disabled = true;
        status.textContent = "";
        try {
          await work();
        } catch (err) {
          if (before === pageId)
            status.textContent =
              err.message || "The host could not complete this request.";
        } finally {
          control.disabled = false;
        }
      },
      css,
    );
  }
  function submit(form, label, work) {
    const control = node("button", label, "primary");
    control.type = "submit";
    form.append(control);
    form.onsubmit = async (event) => {
      event.preventDefault();
      if (control.disabled) return;
      const before = pageId;
      control.disabled = true;
      try {
        await work();
      } catch (err) {
        if (before === pageId)
          status.textContent = err.message || "Could not save settings.";
      } finally {
        control.disabled = false;
      }
    };
  }
  async function load(show = menu) {
    const before = pageId;
    const updated = await request({ action: "read_rows" });
    if (before !== pageId || panel.hidden) return;
    snapshot = updated;
    show();
  }
  async function save(requestBody, next, message = "Saved on the host.") {
    const before = pageId;
    const result = await request(requestBody);
    if (before !== pageId || panel.hidden) return;
    if (result.status === "changed_on_disk") {
      status.textContent =
        "These settings changed on the host. Your edits are still here. ";
      status.append(action("Reload host values", () => load(next)));
      return;
    }
    if (result.status !== "applied")
      throw Error("The host did not confirm this change.");
    await load(() => {
      next();
      status.textContent = message;
    });
  }
  const edit = (revision, change, next) =>
    save({ action: "edit", revision, edit: change }, next);
  function confirmAction(label, question, work) {
    const wrap = node("div", undefined, "phone-settings-confirm");
    const first = button(
      label,
      () => {
        const cancel = button("Cancel", () => {
          wrap.replaceChildren(first);
          first.focus();
        });
        wrap.replaceChildren(
          node("p", question),
          cancel,
          action(label, work, "danger"),
        );
        cancel.focus();
      },
      "quiet",
    );
    wrap.append(first);
    return wrap;
  }
  function menu() {
    screen("Settings", close);
    const choices = snapshot
      ? [
          ["Accounts", accounts],
          ["Providers & models", providers],
          ["Credentials", credentials],
          ["Favorites", favorites],
          ["Web search", search],
          ["Advanced configuration", advanced],
        ]
      : [];
    if (snapshot?.legacy)
      choices.splice(5, 0, ["Legacy Codex provider", legacy]);
    choices.push(
      ["Remote access", remote],
      ["Install & notifications", phone],
      ["Phone appearance & input", () => behaviorForm(true)],
      ["Desktop appearance & input", () => behaviorForm(false)],
    );
    const nav = node("nav", undefined, "phone-settings-menu");
    nav.setAttribute("aria-label", "Settings sections");
    for (const [label, destination] of choices)
      nav.append(action(label, destination));
    body.append(
      nav,
      action("Refresh host settings", () => load(), "quiet"),
    );
    for (const message of snapshot?.issues || [])
      body.append(node("p", message));
  }
  document.getElementById("open-phone-settings").onclick = async () => {
    open();
    const id = screen("Settings", close);
    status.textContent = "Loading host settings…";
    try {
      await load();
    } catch (err) {
      if (id === pageId) {
        snapshot = null;
        menu();
        status.textContent = err.message;
      }
    }
  };
  async function accounts() {
    const id = screen("Accounts");
    accountsPage = id;
    const deviceStatus = await readPage(id, { action: "devices" });
    if (!deviceStatus) return;
    const granted = deviceStatus.devices.some(
      (d) => d.id === deviceStatus.self && d.terminal === true,
    );
    if (!granted)
      body.append(
        node(
          "p",
          "To sign in from this phone, enable terminal access for this device in OX Desktop → Settings → Remote access.",
        ),
      );
    for (const [runtime, name, account, cmd] of [
      [
        "codex",
        "Codex · ChatGPT",
        snapshot.chatgpt_account,
        "ox login chatgpt --device-auth",
      ],
      ["claude", "Claude Code", snapshot.claude_account, "ox login claude"],
    ]) {
      const detail = node("div", undefined, "phone-settings-card");
      detail.append(
        node("h2", name),
        node(
          "p",
          typeof account === "string"
            ? account
            : account
              ? "Signed in"
              : "Signed out",
        ),
      );
      const launch = action(
        "Sign in with " + (runtime === "codex" ? "ChatGPT" : "Claude"),
        () => login(runtime),
      );
      launch.disabled = !granted;
      detail.append(launch);
      detail.append(
        details(
          "Host terminal command",
          node(
            "p",
            "Run this command in the host terminal to use the native sign-in flow.",
          ),
          node("code", cmd),
          action("Copy sign-in command", () =>
            navigator.clipboard.writeText(cmd),
          ),
        ),
      );
      body.append(detail);
    }
    body.append(
      action("Refresh accounts", () => load(accounts)),
      button("Configure API provider", providers),
    );
  }
  function providers() {
    screen("Providers");
    if (!snapshot.registry) {
      body.append(node("p", "The host could not read its provider registry."));
      return;
    }
    const list = node("div", undefined, "phone-settings-menu");
    for (const row of snapshot.registry.providers)
      list.append(
        button(row.fields.name?.value || row.id, () =>
          providerEditor(row, false),
        ),
      );
    const select = node("select");
    select.setAttribute("aria-label", "Provider template");
    select.append(new Option("Custom provider", ""));
    snapshot.presets.forEach(([name], i) =>
      select.append(new Option(name, String(i))),
    );
    const add = button("Add provider", () => {
      const row =
        select.value === ""
          ? { id: "", fields: {}, models: [] }
          : snapshot.presets[Number(select.value)][1];
      providerEditor(row, true);
    });
    const models = node("ul");
    body.append(
      list,
      select,
      add,
      action("Discover available models", async () => {
        const result = await request({ action: "models" });
        models.replaceChildren(...result.map((m) => node("li", m)));
        if (!result.length) models.append(node("li", "No models reported."));
      }),
      models,
    );
  }
  function providerEditor(row, isNew) {
    screen(
      isNew ? "Add provider" : row.fields.name?.value || row.id,
      providers,
    );
    const form = node("form");
    const id = textField(form, "Provider ID", "provider_id");
    id.required = true;
    id.value = row.id;
    const base = fields(form, connection, row.fields, isNew);
    const wire = base.inputs.wire_api.control;
    const options = {};
    for (const [protocol, specs] of Object.entries(adapters)) {
      const box = details("Protocol options");
      options[protocol] = { box, group: fields(box, specs, row.fields, isNew) };
      form.append(box);
    }
    const showProtocol = () => {
      for (const [type, { box }] of Object.entries(options))
        box.hidden = type !== wire.value;
    };
    wire.onchange = showProtocol;
    showProtocol();
    const list = details("Static models");
    const rows = [];
    function addModel(values, original, expand) {
      const box = details(values.id?.value || "New model");
      box.open = expand;
      const group = fields(box, modelFields, values, !original);
      group.inputs.id.control.required = true;
      const state = { original, group, removed: false };
      rows.push(state);
      box.append(
        button(
          "Remove model",
          () => {
            state.removed = true;
            box.remove();
          },
          "quiet",
        ),
      );
      list.insertBefore(box, add);
    }
    const add = button("Add static model", () => addModel({}, null, true));
    list.append(add);
    for (const model of row.models)
      addModel(model, isNew ? null : model.id?.value, false);
    form.append(list);
    function mutation(removed = false) {
      const patch = removed ? {} : base.changes();
      for (const [protocol, { group }] of Object.entries(options)) {
        if (protocol === wire.value) Object.assign(patch, group.changes());
        else if (!isNew && wire.value !== (row.fields.wire_api?.value || ""))
          for (const [key] of adapters[protocol])
            if (row.fields[key]) patch[key] = null;
      }
      const models = removed
        ? []
        : rows.flatMap(({ original, removed, group }) => {
            if (removed)
              return original ? [{ original, id: null, fields: {} }] : [];
            const modelId = group.inputs.id.control.value.trim(),
              changes = group.changes();
            if (!modelId) throw Error("Enter an upstream model ID.");
            return !original ||
              original !== modelId ||
              Object.keys(changes).length
              ? [{ original, id: modelId, fields: changes }]
              : [];
          });
      return {
        section: "providers",
        changes: [
          {
            row: {
              original: isNew ? null : row.id,
              id: removed ? null : id.value.trim(),
              fields: patch,
            },
            models,
          },
        ],
      };
    }
    const revision = snapshot.revisions.registry;
    submit(form, "Save provider", () => edit(revision, mutation(), providers));
    body.append(form);
    if (!isNew)
      body.append(
        action("Test saved connection", async () => {
          const message = await request({
            action: "test_provider",
            id: row.id,
            revision,
          });
          status.textContent = `Connection verified. ${message} models discovered.`;
        }),
        confirmAction(
          "Remove provider",
          "Remove this provider and its static models from OX?",
          () => edit(revision, mutation(true), providers),
        ),
      );
  }
  function credentials() {
    screen("Credentials");
    if (!snapshot.credentials.length)
      body.append(
        node("p", "Configure a provider or Brave Search to add a credential."),
      );
    const revision = snapshot.revisions.keys;
    for (const key of snapshot.credentials) {
      const form = node("form", undefined, "phone-settings-card");
      form.autocomplete = "off";
      form.append(
        node("h2", key.name),
        node("p", key.scope),
        node(
          "p",
          key.environment
            ? "Environment key active"
            : key.saved
              ? "Saved key configured"
              : key.configured
                ? "Configured"
                : "No key configured",
        ),
      );
      const field = textField(
        form,
        key.saved ? "Replacement key" : "New key",
        "secret",
        "password",
      );
      field.autocomplete = "new-password";
      field.maxLength = 8192;
      field.required = true;
      field.spellcheck = false;
      field.setAttribute("autocapitalize", "none");
      const req = (value) => ({
        action: "edit_key",
        revision,
        name: key.name,
        scope: key.scope,
        value,
      });
      submit(form, key.saved ? "Replace key" : "Save key", async () => {
        const value = field.value;
        field.value = "";
        await save(req(value), credentials);
      });
      if (key.saved)
        form.append(
          confirmAction(
            "Remove saved key",
            "Remove the saved key for this destination?",
            () => save(req(null), credentials),
          ),
        );
      body.append(form);
    }
  }
  function favorites() {
    screen("Starred models");
    const ordered = [...snapshot.favorites];
    const list = node("ol", undefined, "phone-favorites");
    function draw() {
      list.replaceChildren(
        ...ordered.map((model, i) => {
          const li = node("li"),
            controls = node("div", undefined, "phone-settings-actions");
          const shift = (offset) => {
            [ordered[i], ordered[i + offset]] = [
              ordered[i + offset],
              ordered[i],
            ];
            draw();
          };
          const up = button("Move up", () => shift(-1)),
            down = button("Move down", () => shift(1));
          up.disabled = i === 0;
          down.disabled = i + 1 === ordered.length;
          controls.append(
            up,
            down,
            button("Remove", () => {
              ordered.splice(i, 1);
              draw();
            }),
          );
          li.append(node("span", model), controls);
          return li;
        }),
      );
    }
    draw();
    const form = node("form"),
      datalist = node("datalist");
    datalist.id = "phone-known-models";
    datalist.append(...snapshot.models.map((id) => new Option(id, id)));
    const input = textField(form, "Model alias", "alias");
    input.setAttribute("list", datalist.id);
    input.required = true;
    submit(form, "Add to favorites", () => {
      const alias = input.value.trim();
      if (!alias) throw Error("Enter a model alias.");
      if (!ordered.includes(alias)) ordered.push(alias);
      input.value = "";
      draw();
    });
    body.append(
      list,
      datalist,
      form,
      action(
        "Save favorites",
        () =>
          edit(
            snapshot.revisions.favorites,
            { section: "favorites", models: ordered },
            favorites,
          ),
        "primary",
      ),
    );
  }
  function search() {
    screen("Search");
    if (!snapshot.registry) {
      body.append(node("p", "The provider registry is unavailable."));
      return;
    }
    const form = node("form");
    const data = fields(form, searchFields, snapshot.registry.search);
    submit(form, "Save search settings", () =>
      edit(
        snapshot.revisions.registry,
        { section: "search", fields: data.changes() },
        search,
      ),
    );
    body.append(form, button("Search credential", credentials));
  }
  function legacy() {
    const current = snapshot.legacy;
    screen("Legacy");
    const form = node("form");
    const data = fields(form, legacyFields, {
      ...current.fields,
      model: current.model,
    });
    submit(form, "Save legacy provider", () =>
      edit(
        snapshot.revisions.legacy,
        { section: "legacy", id: current.id, fields: data.changes() },
        legacy,
      ),
    );
    body.append(node("p", current.id), form);
  }
  async function advanced() {
    const id = screen("Advanced");
    const docs = await readPage(id, { action: "read_advanced" });
    if (!docs) return;
    for (const doc of docs) {
      const form = node("form"),
        area = node("textarea");
      area.value = doc.text;
      area.spellcheck = false;
      area.autocomplete = "off";
      area.setAttribute("aria-label", doc.title);
      form.append(area);
      submit(form, "Save configuration", async () => {
        const result = await request({
          action: "save_advanced",
          id: doc.id,
          revision: doc.revision,
          original: doc.text,
          text: area.value,
        });
        if (pageId !== id || panel.hidden) return;
        if (result.status !== "applied")
          throw Error(
            "The host configuration changed. Reopen this page to load it.",
          );
        await advanced();
        status.textContent = "Saved on the host.";
      });
      body.append(details(doc.title, form));
    }
  }
  async function remote() {
    const id = screen("Remote access");
    const result = await readPage(id, { action: "devices" });
    if (!result) return;
    body.append(node("p", result.origin || location.origin));
    const invite = node("div", undefined, "phone-settings-invite");
    body.append(
      action(
        "Pair another device",
        async () => {
          const pair = await readPage(id, { action: "pair_device" });
          if (!pair) return;
          clearTimeout(inviteTimer);
          const qr = new DOMParser().parseFromString(
            pair.qr_svg,
            "image/svg+xml",
          );
          const size = Number(qr.documentElement.getAttribute("width"));
          const path = qr.querySelector("path")?.getAttribute("d");
          if (!Number.isInteger(size) || size < 1 || size > 1024 || !path)
            throw Error("Could not read the pairing QR.");
          const code = node("canvas");
          code.width = code.height = size;
          code.setAttribute("role", "img");
          code.setAttribute(
            "aria-label",
            "One-time QR for pairing another device",
          );
          const ctx = code.getContext("2d");
          ctx.fillStyle = "white";
          ctx.fillRect(0, 0, size, size);
          ctx.fillStyle = "black";
          ctx.fill(new Path2D(path));
          invite.replaceChildren(
            code,
            node(
              "p",
              "On the new phone, connect to the same tailnet, open this OX address and choose Scan pairing QR.",
            ),
            action("Copy pairing link", () =>
              navigator.clipboard.writeText(pair.url),
            ),
          );
          inviteTimer = setTimeout(
            () =>
              invite.replaceChildren(
                node("p", "Pairing code expired. Create a fresh code."),
              ),
            Math.max(0, pair.expires_at * 1000 - Date.now()),
          );
        },
        "primary",
      ),
      invite,
      action("Copy OX address", () =>
        navigator.clipboard.writeText(result.origin || location.origin),
      ),
      details(
        "Pair from OX Desktop",
        node(
          "p",
          "In OX Desktop on the host, choose Settings → Remote access → Pair a device. Open OX on the new phone and scan the code.",
        ),
      ),
    );
    for (const device of result.devices) {
      const card = node("div", undefined, "phone-settings-card");
      card.append(
        node("h2", device.label),
        node(
          "p",
          device.login + (device.id === result.self ? " · This device" : ""),
        ),
        node(
          "p",
          "Terminal access " +
            (device.terminal ? "enabled" : "off") +
            " · Managed in OX Desktop",
        ),
        node(
          "p",
          "Last used " + new Date(device.last_used * 1000).toLocaleString(),
        ),
        confirmAction(
          "Revoke device",
          "Remove this device’s access to OX?",
          async () => {
            await request({ action: "revoke_device", id: device.id });
            await remote();
          },
        ),
      );
      body.append(card);
    }
  }
  async function behaviorForm(onPhone) {
    const id = screen("Appearance & input");
    const record = onPhone
      ? { behavior: local }
      : await readPage(id, { action: "read_behavior" });
    if (!record) return;
    const prefs = record.behavior;
    const form = node("form");
    const themeLabel = node("label", "Theme"),
      theme = node("select");
    theme.name = "theme";
    for (const mode of ["system", "dark", "light"])
      theme.append(new Option(mode[0].toUpperCase() + mode.slice(1), mode));
    theme.value = prefs.theme;
    themeLabel.append(theme);
    form.append(themeLabel);
    const font = textField(form, "Text size (12–24 px)", "font_size", "number");
    font.min = "12";
    font.max = "24";
    font.step = "1";
    font.required = true;
    font.value = prefs.font_size;
    const entry = node("label", "Return key"),
      enter = node("select");
    enter.name = "enter_sends";
    enter.append(
      new Option("Insert a new line", "newline"),
      new Option("Send the message", "send"),
    );
    enter.value = prefs.enter_sends ? "send" : "newline";
    entry.append(enter);
    form.append(entry);
    const motion = textField(
      form,
      "Reduce motion",
      "reduce_motion",
      "checkbox",
    );
    motion.checked = prefs.reduce_motion;
    // Only a city-level place derived from the laptop's timezone reaches api.met.no.
    const weatherBox = onPhone
      ? null
      : textField(form, "Live weather (MET Norway)", "live_weather", "checkbox");
    if (weatherBox) weatherBox.checked = prefs.live_weather !== false;
    submit(
      form,
      onPhone ? "Save phone preferences" : "Save desktop preferences",
      async () => {
        const behavior = {
          theme: theme.value,
          font_size: Number(font.value),
          enter_sends: enter.value === "send",
          reduce_motion: motion.checked,
        };
        if (weatherBox) behavior.live_weather = weatherBox.checked;
        if (onPhone) {
          local = behavior;
          applyLocal();
          try {
            localStorage.setItem(
              "ox-phone-appearance",
              JSON.stringify(behavior),
            );
            status.textContent = "Saved on this phone.";
          } catch {
            status.textContent =
              "Applied for this visit. Browser storage is unavailable.";
          }
        } else {
          const result = await request({
            action: "save_behavior",
            revision: record.revision,
            behavior,
          });
          if (id !== pageId || panel.hidden) return;
          if (result.status !== "applied") {
            status.textContent = "Desktop preferences changed on the host. ";
            status.append(
              action("Reload desktop preferences", () => behaviorForm(false)),
            );
          } else {
            await behaviorForm(false);
            status.textContent = "Saved on the host.";
          }
        }
      },
    );
    body.append(form);
  }
  function phone() {
    screen("Install & notifications");
    const installed =
      navigator.standalone === true ||
      matchMedia("(display-mode: standalone)").matches;
    const ios =
      /iPhone|iPad|iPod/.test(navigator.userAgent) ||
      (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
    if (installed) body.append(node("p", "OX is open as an installed app."));
    else if (installPrompt)
      body.append(
        action(
          "Install OX",
          async () => {
            const prompt = installPrompt;
            installPrompt = null;
            await prompt.prompt();
            await prompt.userChoice;
            phone();
          },
          "primary",
        ),
      );
    else
      body.append(
        details(
          ios ? "Install OX on iPhone" : "Install OX",
          node(
            "p",
            ios
              ? "In Safari, open Share → Add to Home Screen. Turn on Open as Web App if shown. Open OX from its icon, then scan a fresh pairing QR."
              : "Open your browser menu and choose Install app or Add to Home screen. Open OX from its icon.",
          ),
        ),
      );
    body.append(
      action("Pair another device", remote),
      button("Set up notifications", notifications),
    );
  }
  return {
    reset() {
      pageId++;
      clearTimeout(inviteTimer);
      snapshot = null;
      status.textContent = "";
      body.replaceChildren();
      panel.replaceChildren();
    },
  };
}
