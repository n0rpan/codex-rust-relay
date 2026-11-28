import { registeredHostOrigin } from "./settings.js";

const $ = (id) => document.getElementById(id);

function destinationLink(move) {
  const host = move.destination;
  const receipt = move.receipt;
  const target = registeredHostOrigin(host?.origin);
  if (!target || receipt?.operation !== move.id || receipt?.host !== host.id ||
      typeof receipt?.thread !== "string" || !receipt.thread) return null;
  target.hash = new URLSearchParams({ thread: receipt.thread }).toString();
  return target;
}

export class MoveControls {
  constructor() {
    this.dialog = $("move-dialog");
    this.progress = $("move-progress");
    this.launcher = $("menu-move");
    this.choice = $("move-destination");
    this.launcher.onclick = () => this.open();
    $("move-cancel").onclick = () => this.dialog.close();
    this.choice.onchange = () => this.updateChoice();
    this.dialog.onclose = () => {
      if (!this.progress.hidden) $("move-progress-title").focus({ preventScroll: true });
      else this.launcher.closest("details").querySelector("summary").focus({ preventScroll: true });
    };
    $("move-form").onsubmit = (event) => {
      event.preventDefault();
      const target = this.targets.find((host) => host.id === this.choice.value);
      const view = this.view;
      if (!view?.ready || !target || target.unavailable || this.submitting) return;
      const operation = crypto.randomUUID();
      this.submitting = operation;
      this.updateChoice();
      const sent = view.act({ move_session: {
        thread: view.thread, destination: target.id, operation,
        after_turn: view.running,
      } }, (ok, message) => {
        if (this.view?.owner !== view.owner) return;
        this.submitting = null;
        if (ok) this.dialog.close();
        else {
          $("move-note").textContent = message || "The move could not be queued.";
          this.updateChoice();
        }
      });
      if (!sent) {
        this.submitting = null;
        this.updateChoice();
      }
    };
    this.submit = $("move-submit");
    this.targets = [];
  }

  update(view) {
    if (this.view?.owner !== view?.owner) {
      this.dialog.close();
      this.move = null;
      this.renderKey = null;
      this.submitting = null;
    }
    this.view = view;
    if (view?.move || view?.ready) this.move = view?.move;
    this.targets = Array.isArray(view?.targets) ? view.targets : [];
    this.launcher.hidden = !view?.supported;
    this.launcher.disabled = !view?.ready || Boolean(this.submitting) ||
      (this.move && !["failed", "cancelled"].includes(this.move.phase));
    if (this.dialog.open) this.updateChoice();
    const key = JSON.stringify([this.move, view?.ready, view?.source]);
    if (this.renderKey === key) return;
    this.renderKey = key;
    this.renderProgress();
  }

  open() {
    if (!this.view?.ready || this.launcher.disabled) return;
    const { source, running } = this.view;
    this.choicesKey = null;
    $("move-source").textContent = running
      ? `This turn will finish on ${source}. Keep it on until OX confirms the move.`
      : `Keep ${source} on until OX confirms the move.`;
    $("move-note").textContent = this.targets.length
      ? ""
      : "Connect a host that can run this session in Settings.";
    this.updateChoice();
    this.launcher.closest("details").open = false;
    this.dialog.showModal();
  }

  updateChoice() {
    const key = JSON.stringify(this.targets);
    if (this.choicesKey !== key) {
      this.choicesKey = key;
      const previous = this.choice.value;
      const options = this.targets.map((host) => {
        const option = document.createElement("option");
        option.value = host.id;
        option.textContent = host.unavailable ? `${host.name} · ${host.unavailable}` : host.name;
        option.disabled = Boolean(host.unavailable);
        return option;
      });
      this.choice.replaceChildren(...options);
      if (this.targets.some((host) => host.id === previous && !host.unavailable))
        this.choice.value = previous;
    }
    const target = this.targets.find((host) => host.id === this.choice.value && !host.unavailable);
    $("move-destination-detail").textContent = target?.detail || "";
    this.submit.textContent = this.view?.running
      ? "Move after this turn"
      : (target ? `Move to ${target.name}` : "Move session");
    this.submit.disabled = !target || !this.view?.ready || Boolean(this.submitting);
  }

  renderProgress() {
    const move = this.move;
    const progress = this.progress;
    const actions = $("move-progress-actions");
    progress.hidden = !move;
    actions.replaceChildren();
    if (!move) return;
    const source = this.view.source;
    const name = move.destination?.name;
    const link = move.phase === "ready" && destinationLink(move);
    const locked = ["preparing", "closing", "checking", "ready"].includes(move.phase);
    const status = {
      waiting: [`Moves to ${name} after this turn`, ""],
      waiting_for_workers: ["Waiting for workers", `Your move to ${name} is queued.`],
      preparing: [`Preparing ${name}`, `Copying the conversation and working files. Keep ${source} on.`],
      closing: [`Closing on ${source}`, `Keep ${source} on until ${name} confirms the move.`],
      checking: ["Confirming the move", `Keep ${source} on while OX checks whether ${name} can continue.`],
      ready: link
        ? [`Ready on ${name}`, `You can shut down ${source}.`]
        : ["Confirming the move", `Keep ${source} on while OX checks whether ${name} can continue.`],
      failed: [`Couldn’t prepare ${name}`, move.message || `Your session is still on ${source}.`],
      cancelled: ["Move cancelled", `Your session is still on ${source}.`],
    }[move.phase];
    progress.hidden = !status || !name;
    progress.dataset.phase = move.phase;
    progress.dataset.locked = String(locked);
    if (!status) return;
    $("move-progress-title").textContent = status[0];
    const copy = $("move-progress-detail");
    copy.textContent = status[1];
    copy.hidden = !status[1];
    if (link) {
      const open = document.createElement("a");
      open.href = link.href;
      open.textContent = `Open on ${name}`;
      actions.append(open);
      return;
    }
    if (["waiting", "waiting_for_workers", "preparing"].includes(move.phase))
      actions.append(this.action("Cancel move", { cancel_move: move.id }));
    if (move.phase === "checking" || move.phase === "ready")
      actions.append(this.action("Check status", { check_move: move.id }));
    if (move.phase === "failed")
      actions.append(this.action("Try again", { retry_move: move.id }));
  }

  action(label, command) {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    b.disabled = !this.view?.ready;
    b.onclick = () => this.view?.act(command, (ok, message) => {
      if (!ok) this.view?.notice(message || "OX could not update the move.");
    });
    return b;
  }
}
