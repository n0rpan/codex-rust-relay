import { lexer } from "./vendor/marked-18.3.0.js";
const $ = (id) => document.getElementById(id);
function element(tag, text) {
  const node = document.createElement(tag);
  node.textContent = text;
  return node;
}
export function copyButton(label, text) {
  const b = element("button", label);
  b.type = "button";
  b.className = "quiet copy";
  b.onclick = async () => {
    try {
      await navigator.clipboard.writeText(text);
      b.textContent = "Copied";
    } catch {
      b.textContent = "Select text to copy";
    }
  };
  return b;
}
const inlineTags = {
  strong: "strong",
  em: "em",
  del: "del",
  paragraph: "p",
  blockquote: "blockquote",
};

function renderTokens(parent, tokens) {
  for (const token of tokens) {
    const tag = inlineTags[token.type];
    if (tag || token.type === "heading") {
      const node = document.createElement(
        tag || "h" + Math.min(token.depth + 2, 6),
      );
      renderTokens(node, token.tokens);
      parent.append(node);
      continue;
    }
    switch (token.type) {
      case "space":
      case "def":
        break;
      case "br":
      case "hr":
        parent.append(document.createElement(token.type));
        break;
      case "codespan":
        parent.append(element("code", token.text));
        break;
      case "code": {
        const box = document.createElement("div");
        box.className = "code-card";
        const head = document.createElement("div");
        head.className = "code-head";
        head.append(element("span", token.lang || "Code"), copyButton("Copy", token.text));
        box.append(head, element("pre", token.text));
        parent.append(box);
        break;
      }
      case "list": {
        const list = document.createElement(token.ordered ? "ol" : "ul");
        if (token.ordered) list.start = token.start;
        for (const item of token.items) {
          const li = document.createElement("li");
          if (item.task) li.append(item.checked ? "☑ " : "☐ ");
          renderTokens(li, item.tokens);
          list.append(li);
        }
        parent.append(list);
        break;
      }
      case "table": {
        const scroller = document.createElement("div");
        scroller.className = "table-scroll";
        const table = document.createElement("table");
        const fill = (row, cells, kind) => {
          for (const cell of cells) {
            const td = document.createElement(kind);
            if (kind === "th") td.scope = "col";
            renderTokens(td, cell.tokens);
            row.append(td);
          }
        };
        fill(table.createTHead().insertRow(), token.header, "th");
        const body = table.createTBody();
        for (const cells of token.rows) fill(body.insertRow(), cells, "td");
        scroller.append(table);
        parent.append(scroller);
        break;
      }
      case "link": {
        if (!/^https?:\/\//i.test(token.href)) {
          renderTokens(parent, token.tokens);
          break;
        }
        const link = document.createElement("a");
        link.href = token.href;
        link.rel = "noopener noreferrer";
        link.target = "_blank";
        renderTokens(link, token.tokens);
        parent.append(link);
        break;
      }
      default:
        // HTML remains literal and images never cause a request from a paired device.
        if (token.tokens) renderTokens(parent, token.tokens);
        else parent.append(token.text ?? token.raw);
    }
  }
}

export function messageText(node, text) {
  const content = document.createDocumentFragment();
  try {
    renderTokens(content, lexer(text));
    node.replaceChildren(content);
  } catch {
    // Preserve the full reply if a deeply nested or partial message exceeds parser limits.
    node.textContent = text;
  }
}

export function formatPatchNode(node, text) {
  if (!text) {
    node.replaceChildren();
    return;
  }
  const lines = String(text).split("\n");
  const isDiff = lines.some(
    (l) => l.startsWith("@@ ") || l.startsWith("--- ") || l.startsWith("+++ "),
  );
  if (!isDiff) {
    node.textContent = text;
    return;
  }
  const frag = document.createDocumentFragment();
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const span = element("span", i + 1 < lines.length ? line + "\n" : line);
    span.className =
      line.startsWith("+") && !line.startsWith("+++")
        ? "diff-line diff-add"
        : line.startsWith("-") && !line.startsWith("---")
          ? "diff-line diff-del"
          : line.startsWith("@@")
            ? "diff-line diff-hunk"
            : "diff-line";
    frag.append(span);
  }
  node.replaceChildren(frag);
}

let lastReview;
export function showReview(reviews) {
  const pane = $("review-pane");
  const turns = (reviews?.order ?? [])
    .map((id) => reviews.turns[id])
    .filter(
      (turn) =>
        turn &&
        (turn.plan_text ||
          turn.steps?.length ||
          turn.explanation ||
          turn.aggregate?.length ||
          turn.items?.length),
    );
  pane.hidden = !turns.length;
  if (lastReview === reviews && pane.open) return;
  const draw = () => {
    if (!pane.open) return;
    const frag = document.createDocumentFragment();
    for (const t of turns) {
      if (t.explanation) frag.append(element("p", t.explanation));
      if (t.plan_text) frag.append(element("p", t.plan_text));
      if (t.steps?.length) {
        const list = element("ol", "");
        for (const step of t.steps)
          list.append(element("li", step.text + " · " + step.status));
        frag.append(list);
      }
      for (const file of t.aggregate ??
        (t.items ?? []).flatMap(([, files]) => files)) {
        const details = element("details", "");
        const head = document.createElement("div");
        head.className = "code-head";
        head.append(
          element("span", "Patch"),
          copyButton("Copy patch", file.diff),
        );
        const pre = element("pre", "");
        formatPatchNode(pre, file.diff);
        details.append(element("summary", file.path), head, pre);
        frag.append(details);
      }
    }
    $("review-content").replaceChildren(frag);
    lastReview = reviews;
  };
  pane.ontoggle = draw;
  draw();
}

const drafts = new Map();
let owner, repaint, report, queryTimer, query, current, itemsShown;
export function forgetComposer() {
  clearTimeout(queryTimer);
  drafts.clear();
  current = null;
  itemsShown = null;
  lastReview = undefined;
  $("attached-images").replaceChildren();
  $("mention-choices").replaceChildren();
}
export function imagesFor(key) {
  return drafts.get(key) ?? [];
}
export function clearImages(key, sent) {
  if (drafts.get(key) === sent) drafts.delete(key);
}
// A provisional composer becomes the real session's: what was attached since the send
// (anything but the images already sent) moves along.
export function moveImages(from, to, sent) {
  const images = drafts.get(from);
  drafts.delete(from);
  if (images && images !== sent && images.length) drafts.set(to, images);
}
export function setComposerOwner(key, render, notice) {
  clearTimeout(queryTimer);
  query = null;
  current = null;
  owner = key;
  repaint = render;
  report = notice;
  itemsShown = null;
  $("mention-choices").replaceChildren();
  $("review-pane").open = false;
  lastReview = undefined;
}
const supportsFieldSizing =
  typeof CSS !== "undefined" &&
  typeof CSS.supports === "function" &&
  CSS.supports("field-sizing", "content");
export function syncDraftHeight() {
  const draft = $("draft");
  if (!draft) return;
  if (supportsFieldSizing) {
    draft.style.removeProperty("height");
    return;
  }
  draft.style.height = "auto";
  if (!draft.value) {
    draft.style.removeProperty("height");
    return;
  }
  const cap = Math.max(120, Math.round((window.visualViewport?.height ?? window.innerHeight) * 0.25));
  draft.style.height = Math.min(cap, Math.max(44, draft.scrollHeight)) + "px";
}
function readFile(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () =>
      resolve({
        file: file.name,
        image: { clipboard: [file.type, reader.result] },
        bytes: file.size,
      });
    reader.onerror = () => reject(new Error("Unable to read that image."));
    reader.readAsDataURL(file);
  });
}
$("attach-image").onchange = async (event) => {
  const key = owner,
    files = Array.from(event.target.files);
  event.target.value = "";
  try {
    if (
      files.some(
        (f) =>
          !["image/png", "image/jpeg", "image/webp", "image/gif"].includes(
            f.type,
          ),
      )
    )
      throw new Error("Choose a PNG, JPEG, GIF or WebP image.");
    const existing = imagesFor(key);
    if (
      existing.length + files.length > 4 ||
      [...existing.map((v) => v.bytes), ...files.map((f) => f.size)].reduce(
        (sum, n) => sum + n,
        0,
      ) >
        6 * 1024 * 1024
    )
      throw new Error("Choose up to four images, totalling 6 MiB or less.");
    const added = await Promise.all(files.map(readFile));
    if (imagesFor(key).length !== existing.length)
      throw new Error("Attachments changed. Select the images again.");
    drafts.set(key, [...existing, ...added]);
    repaint();
  } catch (error) {
    report(error.message);
  }
};
export function updateExtras(state, caps, link, id) {
  current = { link, id, mention: caps.includes("mention") };
  $("attach-label").hidden = !caps.includes("attach_image");
  const images = drafts.get(owner);
  if (itemsShown !== images) {
    itemsShown = images;
    $("attached-images").replaceChildren(
      ...(images ?? []).map((file, i) => {
        const b = element("button", "Remove " + file.file);
        b.className = "attached-chip";
        const src = file.image?.clipboard?.[1];
        if (typeof src === "string" && src.startsWith("data:image/")) {
          const img = document.createElement("img");
          img.src = src;
          img.alt = "";
          b.prepend(img);
        }
        b.type = "button";
        b.onclick = () => {
          drafts.set(
            owner,
            images.filter((_, n) => n !== i),
          );
          repaint();
        };
        return b;
      }),
    );
  }
  const mention = state?.detail?.mention;
  const match =
    query && mention && mention.owner === id && mention.query === query.value;
  const signature = JSON.stringify(
    match ? [query.start, query.end, mention.version, mention.paths] : null,
  );
  const box = $("mention-choices");
  if (box.dataset.query !== signature) {
    box.dataset.query = signature;
    box.replaceChildren(
      ...(match ? mention.paths : []).slice(0, 12).map((path) => {
        const b = element("button", path);
        b.type = "button";
        const keepFocus = (event) => {
          if (document.activeElement === $("draft")) event.preventDefault();
        };
        b.onpointerdown = keepFocus;
        b.onmousedown = keepFocus;
        b.onclick = () => {
          if (!query) return;
          const draft = $("draft");
          draft.setRangeText("@" + path + " ", query.start, query.end, "end");
          draft.focus();
          draft.dispatchEvent(new Event("input"));
        };
        return b;
      }),
    );
  }
}
export function mentionInput() {
  clearTimeout(queryTimer);
  const draft = $("draft");
  const pos = draft.selectionStart;
  const match = /(?:^|\s)@([^\s]*)$/.exec(draft.value.slice(0, pos));
  query = null;
  if (!current?.mention || !match) return;
  const rest = /^\S*/.exec(draft.value.slice(pos))[0];
  query = {
    start: pos - match[1].length - 1,
    end: pos + rest.length,
    value: match[1] + rest,
  };
  const { link, id } = current,
    value = query.value;
  if (value.length < 120)
    queryTimer = setTimeout(() => link?.act({ mention: [id, value] }), 150);
}
