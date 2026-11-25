#!/usr/bin/env python3
"""Run a window test unless it is quarantined for this event.

    quarantine.py --check
        Validate desktop/ci/quarantine.toml: every entry has a known name and
        an issue number.
    quarantine.py NAME -- COMMAND...
        Run COMMAND. If NAME is quarantined for this runner, run it only for
        the nightly schedule and manual runs (blocking there) and skip it with
        a notice everywhere else.

The runner comes from $OX_RUNNER (the matrix runner label), the event from
$GITHUB_EVENT_NAME. Names are desktop/tests/<name>.py stems or a gate step
name listed in GATES below.
"""
import os
import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[2]
LIST = ROOT / "desktop/ci/quarantine.toml"
GATES = {"packaged-window", "gate-x11", "gate-wayland-claude"}
BLOCKING_EVENTS = {"schedule", "workflow_dispatch"}


def known(name):
    return name in GATES or (ROOT / "desktop/tests" / f"{name}.py").is_file()


def entries():
    with open(LIST, "rb") as f:
        return tomllib.load(f).get("test", [])


def check():
    errors = []
    for i, e in enumerate(entries()):
        name = e.get("name")
        if not isinstance(name, str) or not known(name):
            errors.append(f"entry {i}: unknown window test {name!r}")
        if not isinstance(e.get("issue"), int) or e["issue"] <= 0:
            errors.append(f"entry {i} ({name}): needs issue = <number>")
        runners = e.get("runners", [])
        if not isinstance(runners, list) or not all(isinstance(r, str) for r in runners):
            errors.append(f"entry {i} ({name}): runners must be a list of labels")
    for line in errors:
        print(f"::error file=desktop/ci/quarantine.toml::{line}")
    if not errors:
        print(f"quarantine list OK ({len(entries())} entries)")
    return 1 if errors else 0


def run(name, command):
    if not known(name):
        print(f"::error::quarantine.py: unknown window test {name!r}")
        return 2
    runner = os.environ.get("OX_RUNNER", "")
    event = os.environ.get("GITHUB_EVENT_NAME", "")
    for e in entries():
        if e["name"] == name and (not e.get("runners") or runner in e["runners"]):
            issue = e["issue"]
            if event not in BLOCKING_EVENTS:
                print(f"::notice title=Quarantined::{name} skipped on {runner or 'this runner'} "
                      f"(issue #{issue}); it runs nightly")
                return 0
            print(f"{name} is quarantined (issue #{issue}); running it for the {event} run")
    sys.stdout.flush()
    os.execvp(command[0], command)


def main(argv):
    if argv == ["--check"]:
        return check()
    if len(argv) >= 3 and argv[1] == "--":
        return run(argv[0], argv[2:])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
