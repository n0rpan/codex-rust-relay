// The activity line: what the agent is doing, as a tiny pixel scene and plain words.
// Mirrors desktop/ox-desktop/src/activity/scene.rs: same triggers, labels and sprites.
// One timer, at most 8 fps, and none while the page is hidden or motion is reduced.

export const LABELS = {
  thinking: "Thinking",
  typing: "Writing",
  reading: "Reading files",
  building: "Building",
  terminal: "Running a command",
  testing: "Running tests",
  web: "Searching the web",
  tool: "Using a tool",
  worker: "Delegating to a worker",
  waiting: "Waiting for you · INSERT COIN",
  done: "Done · HIGH SCORE",
  gameover: "GAME OVER · turn failed",
};
// Frames a finale plays before it leaves the line.
const ONE_SHOT = { done: 16, gameover: 24 };
const KEY_FRAME = { building: 7, testing: 7, web: 7, worker: 7, thinking: 4 };
const tickMs = (scene) => (scene === "waiting" ? 500 : 125);

/** Claude and Codex tool names. Anything unknown is the generic tool scene. */
export function forTool(tool) {
  switch (String(tool ?? "").toLowerCase()) {
    case "read":
    case "glob":
    case "grep":
    case "ls":
    case "notebookread":
    case "read_file":
    case "list_dir":
    case "view":
      return "reading";
    case "edit":
    case "write":
    case "multiedit":
    case "notebookedit":
    case "apply_patch":
    case "file_change":
      return "building";
    case "bash":
    case "bashoutput":
    case "powershell":
    case "shell":
    case "exec_command":
    case "command_execution":
      return "terminal";
    case "websearch":
    case "webfetch":
    case "web_search":
    case "web_fetch":
      return "web";
    case "task":
    case "agent":
    case "workflow":
    case "spawn_agent":
      return "worker";
    default:
      return "tool";
  }
}

/** A shell command: a test run anywhere in it wins, then the first program decides. */
export function forCommand(command) {
  const words = String(command ?? "")
    .split(/[\s"';&|()]+/)
    .filter(Boolean)
    .map((w) => w.slice(w.lastIndexOf("/") + 1));
  const tests = words.some((word, i) => {
    const next = words[i + 1],
      after = words[i + 2];
    if (word === "pytest" || word === "nextest") return true;
    if (["cargo", "go", "make", "just"].includes(word))
      return next === "test" || next === "nextest";
    if (["npm", "pnpm", "yarn", "bun"].includes(word))
      return next === "test" || (next === "run" && after === "test");
    return false;
  });
  if (tests) return "testing";
  // Codex wraps commands as `bash -lc "…"`; a leading `cd dir`, flags and
  // `KEY=value` words are not the program either.
  let program = "";
  for (let i = 0; i < words.length; i++) {
    const word = words[i];
    if (word === "cd") i++;
    else if (
      !["bash", "zsh", "sh", "env", "sudo"].includes(word) &&
      !word.startsWith("-") &&
      !word.includes("=")
    ) {
      program = word;
      break;
    }
  }
  if (
    [
      "cat",
      "rg",
      "grep",
      "ls",
      "find",
      "fd",
      "head",
      "tail",
      "tree",
      "wc",
      "less",
      "bat",
    ].includes(program)
  )
    return "reading";
  if (program === "apply_patch") return "building";
  if (program === "curl" || program === "wget") return "web";
  return "terminal";
}

/**
 * The scene for a session: a pending request wins, then the newest item of the running turn.
 * Returns null when nothing is happening.
 */
export function sceneFor(items, running, waiting) {
  if (waiting) return "waiting";
  if (!running) return null;
  const last = items?.[items.length - 1];
  if (!last) return "thinking";
  if (["assistant", "plan", "progress"].includes(last.role)) return "typing";
  if (last.role !== "tool") return "thinking";
  const [kind, data] = Object.entries(last.detail?.item ?? {})[0] ?? [];
  if (kind === "WebSearch") return "web";
  const status = data?.status ?? last.detail?.status ?? "";
  if (!/^(inProgress|in_progress|running|started)$/.test(status))
    return "thinking";
  if (kind === "CommandExecution") return forCommand(data.command);
  if (kind === "FileChange") return "building";
  if (kind === "AgentOperation") return "worker";
  if (kind === "ToolCall" || kind === "McpToolCall") return forTool(data.tool);
  return "tool";
}

/** A stable seed for one turn of one thread, so a repaint never reshuffles. */
export function seed(thread, turn) {
  let hash = 2166136261;
  for (const ch of `${thread}\0${turn}`)
    hash = Math.imul(hash ^ ch.codePointAt(0), 16777619) >>> 0;
  return hash;
}

/** Which thinking sprite to show: the turn picks the first, the clock moves on every 12 s. */
export const thinkingVariant = (seed, seconds) =>
  ((seed % 4) + Math.floor(seconds / 12)) % 4;

function face(r, x, y) {
  r(x, y, 7, 4, "accent");
  for (const dx of [1, 5]) {
    r(x + dx, y - 2, 1, 2, "ink");
    r(x + dx, y + 1, 1, 1, "on-accent");
    r(x + dx, y + 4, 1, 1, "accent");
  }
}

/** Emit one frame as r(x, y, w, h, ink) on the 20 by 11 grid. */
export function draw(scene, variant, frame, r) {
  const b = frame % 8,
    half = b >> 1;
  switch (scene) {
    case "thinking":
      if (variant === 0) {
        // A thought bubble counting its dots.
        r(3, 2, 13, 5, "accent");
        r(2, 3, 1, 3, "accent");
        r(16, 3, 1, 3, "accent");
        r(5, 7, 3, 1, "accent");
        r(4, 8, 1, 1, "accent");
        for (let i = 0; i < 3; i++)
          r(5 + i * 4, 4, 2, 1, half % 3 === i ? "ink" : "on-accent");
      } else if (variant === 1) {
        // A rally of Pong while the answer loads.
        const [x, y] = [
          [3, 4],
          [7, 6],
          [11, 8],
          [15, 6],
          [16, 4],
          [12, 2],
          [8, 4],
          [4, 6],
        ][b];
        const follow = Math.min(7, Math.max(0, y - 1));
        r(1, x < 10 ? follow : 3, 1, 4, "ink");
        r(18, x >= 10 ? follow : 3, 1, 4, "ink");
        for (let i = 0; i < 5; i++) r(10, 1 + i * 2, 1, 1, "line");
        r(x, y, 1, 1, "warning");
      } else if (variant === 2) {
        // The light bulb: dim, then the idea lands.
        const lit = b >= 3,
          glass = lit ? "warning" : "line";
        r(8, 1, 5, 1, glass);
        r(7, 2, 7, 4, glass);
        r(8, 6, 5, 1, glass);
        r(9, 4, 3, 1, lit ? "ink" : "panel");
        r(9, 7, 3, 2, "accent");
        r(10, 9, 1, 1, "accent");
        if (lit) {
          r(4 - (b % 2), 3, 2, 1, "warning");
          r(15 + (b % 2), 3, 2, 1, "warning");
          r(5, 0, 1, 1, "warning");
          r(15, 0, 1, 1, "warning");
        }
      } else {
        // The ox ponders: dots rise to a question mark.
        face(r, 2, 4);
        if (b >= 1) r(10, 6, 1, 1, "ink");
        if (b >= 2) r(12, 4, 1, 1, "ink");
        if (b >= 3) {
          const ink = b % 2 === 0 ? "warning" : "ink";
          r(14, 1, 3, 1, ink);
          r(16, 2, 1, 2, ink);
          r(15, 4, 1, 2, ink);
          r(15, 7, 1, 1, ink);
        }
      }
      break;
    case "typing":
    case "tool":
      r(2, 1, 15, 8, "accent");
      r(3, 2, 13, 6, "panel");
      r(4, 3, 1, 1, "ink");
      r(5, 4, 1, 1, "ink");
      r(4, 5, 1, 1, "ink");
      if (scene === "typing") r(7, 5, 1 + b, 1, "accent");
      else r(7 + half, 6, 3, 1, "warning");
      r(6, 9, 7, 1, "line");
      break;
    case "reading":
      // The ox reads: the highlighted line moves down both pages, then a page turns.
      face(r, 1, 4);
      r(10, 3, 4, 6, "ink");
      r(15, 3, 4, 6, "ink");
      r(14, 4, 1, 6, "line");
      r(10, 9, 9, 1, "line");
      for (let i = 0; i < 4; i++)
        r(
          11 + (i >> 1) * 5,
          4 + (i % 2) * 2,
          2,
          1,
          half === i ? "accent" : "line",
        );
      if (b === 7) {
        r(14, 1, 1, 3, "ink");
        r(13, 2, 1, 1, "ink");
      }
      break;
    case "building":
      // Bricks drop into a wall, one per frame.
      r(1, 10, 18, 1, "line");
      [
        [2, 8],
        [7, 8],
        [12, 8],
        [4, 5],
        [9, 5],
        [14, 5],
        [2, 2],
        [7, 2],
      ].forEach(([x, y], i) => {
        if (i < b) r(x, y, 4, 2, "accent");
        else if (i === b) r(x, y - 1, 4, 2, "warning");
      });
      break;
    case "terminal": {
      // A cabinet with a prompt, a blinking cursor and one key going down.
      r(2, 0, 15, 8, "accent");
      r(3, 1, 13, 6, "panel");
      r(4, 2, 1, 1, "ink");
      r(5, 3, 1, 1, "ink");
      r(4, 4, 1, 1, "ink");
      const typed = Math.min(b, 6);
      if (typed > 0) r(7, 3, typed, 1, "accent");
      if (b % 2 === 0) r(7 + typed, 3, 1, 1, "warning");
      r(3, 9, 14, 2, "line");
      r(4 + ((b * 5) % 12), 9, 2, 1, "ink");
      break;
    }
    case "testing":
      // A checkered flag waves while three result lamps turn green.
      r(2, 0, 1, 11, "ink");
      for (let col = 0; col < 5; col++)
        for (let row = 0; row < 3; row++)
          r(
            3 + col * 2,
            1 + row * 2 + ((col + half) % 2),
            2,
            2,
            (col + row) % 2 === 0 ? "ink" : "line",
          );
      for (let i = 0; i < 3; i++)
        r(15, 1 + i * 3, 3, 2, half > i ? "success" : "line");
      break;
    case "web":
      // A turning globe sends out waves.
      [2, 3, 4, 4, 4, 4, 4, 3, 2].forEach((h, i) =>
        r(8 - h, 1 + i, h * 2 + 1, 1, "accent"),
      );
      r(4, 5, 9, 1, "on-accent");
      r(5 + Math.floor((b * 7) / 8), 2, 1, 7, "on-accent");
      for (let i = 0; i < 3; i++)
        if (half > i || b === 7) r(14 + i * 2, 4 - i, 1, 3 + i * 2, "warning");
      break;
    case "worker":
      // The ox sends out mini oxen, one more every other frame.
      face(r, 1, 3);
      if (b % 2 === 1) r(8, 4, 1, 1, "warning");
      for (let k = 0; k < 3; k++) {
        if (half < k) continue;
        const x = 9 + k * 4,
          y = 7 + ((b + k) % 2);
        r(x, y, 3, 2, "accent");
        r(x, y - 1, 1, 1, "ink");
        r(x + 2, y - 1, 1, 1, "ink");
      }
      break;
    case "waiting": {
      // PLAYER 1 blinks while a coin hangs over the slot.
      if (b % 2 === 0) {
        r(2, 2, 1, 5, "ink");
        r(1, 3, 1, 1, "ink");
        r(1, 7, 3, 1, "ink");
        r(5, 2, 1, 6, "ink");
        r(6, 2, 2, 1, "ink");
        r(8, 3, 1, 2, "ink");
        r(6, 5, 2, 1, "ink");
      }
      const drop = b % 4;
      r(13, drop, 3, 1, "warning");
      r(12, drop + 1, 5, 2, "warning");
      r(13, drop + 3, 3, 1, "warning");
      r(14, drop + 1, 1, 2, "on-accent");
      r(11, 7, 8, 4, "accent");
      r(13, 8, 4, 1, "panel");
      break;
    }
    case "done":
      // A trophy with a travelling shine and four twinkling stars.
      r(5, 2, 10, 1, "warning");
      r(5, 3, 1, 2, "warning");
      r(14, 3, 1, 2, "warning");
      r(7, 3, 6, 3, "warning");
      r(8, 6, 4, 1, "warning");
      r(9, 7, 2, 2, "warning");
      r(7, 9, 6, 1, "ink");
      r(8 + (b % 4), 3, 1, 2, "ink");
      [
        [2, 2],
        [17, 1],
        [16, 7],
        [3, 7],
      ].forEach(([x, y], i) => {
        r(x, y, 1, 1, "ink");
        if ((b + i * 2) % 4 < 2) {
          r(x - 1, y, 3, 1, "ink");
          r(x, y - 1, 1, 3, "ink");
        }
      });
      break;
    case "gameover":
      // The ox is out: crossed eyes, flashing red.
      r(5, 3, 10, 6, b % 4 < 2 ? "error" : "line");
      r(6, 1, 1, 2, "ink");
      r(13, 1, 1, 2, "ink");
      for (const x of [6, 11])
        for (const [dx, dy] of [
          [0, 0],
          [2, 0],
          [1, 1],
          [0, 2],
          [2, 2],
        ])
          r(x + dx, 4 + dy, 1, 1, "panel");
      r(8, 7, 4, 1, "panel");
      r(4, 10, 12, 1, "line");
      break;
  }
}

/**
 * Drives the activity line. `update` is called on every repaint with the session's state;
 * the line owns the single timer and stops it whenever there is nothing moving to show.
 */
export class ActivityLine {
  constructor(line, canvas, label) {
    this.line = line;
    this.canvas = canvas;
    this.label = label;
    this.scene = null;
    this.seed = 0;
    this.frame = 0;
    this.left = 0;
    this.timer = null;
    this.session = null;
    this.turn = null;
    this.finale = null;
    this.reduced = matchMedia("(prefers-reduced-motion: reduce)");
    document.addEventListener("visibilitychange", () => this.schedule());
    this.reduced.addEventListener("change", () => this.schedule());
  }

  /** `session` identifies the open session; `turn` is its running turn, if any. */
  update({ session, items, running, waiting, failed }) {
    let scene = sceneFor(items, running, waiting);
    const last = items?.[items.length - 1];
    const turn = running ? (last?.turn ?? "") : null;
    // A turn that ends while its session is open plays one finale.
    if (session !== this.session) this.finale = null;
    else if (this.turn !== null && turn === null)
      this.finale = failed ? "gameover" : "done";
    else if (turn !== null) this.finale = null;
    this.session = session;
    this.turn = turn;
    scene ??= this.finale;
    const nextSeed = turn === null ? 0 : seed(session, turn);
    if (scene === this.scene && nextSeed === this.seed) return;
    this.scene = scene;
    this.seed = nextSeed;
    this.frame = 0;
    this.left = ONE_SHOT[scene] ?? 0;
    this.line.hidden = !scene;
    this.label.textContent = scene ? LABELS[scene] : "";
    clearTimeout(this.timer);
    this.timer = null;
    this.paint();
    this.schedule();
  }

  still() {
    return (
      this.reduced.matches ||
      document.documentElement.dataset.phoneReducedMotion === "true"
    );
  }

  schedule() {
    if (this.timer !== null || !this.scene || document.hidden) return;
    const still = this.still();
    if (still) this.paint();
    // With motion off only a finale still wakes, once, to leave the line.
    if (still && !this.left) return;
    const frames = still ? this.left : 1;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.frame = (this.frame + 1) % 8;
      if (this.left && (this.left -= frames) <= 0) {
        this.finale = null;
        this.scene = null;
        this.line.hidden = true;
        return;
      }
      this.paint();
      this.schedule();
    }, tickMs(this.scene) * frames);
  }

  paint() {
    if (!this.scene) return;
    const g = this.canvas.getContext("2d");
    const css = getComputedStyle(this.canvas);
    const frame = this.still() ? (KEY_FRAME[this.scene] ?? 0) : this.frame;
    const variant = thinkingVariant(this.seed, performance.now() / 1000);
    g.clearRect(0, 0, 20, 11);
    draw(this.scene, variant, frame, (x, y, w, h, ink) => {
      g.fillStyle = css.getPropertyValue("--" + ink).trim();
      g.fillRect(x, y, w, h);
    });
  }
}
