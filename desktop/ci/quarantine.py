#!/usr/bin/env python3
"""Run a window test unless it is quarantined for this event.

    quarantine.py --check
        Validate desktop/ci/quarantine.toml: every entry has a known name, an
        issue number and an expiry date that has not passed.
    quarantine.py --self-test
        Check this script's own rules.
    quarantine.py NAME -- COMMAND...
        Run COMMAND. If NAME is quarantined for this runner, run it, blocking,
        for the nightly schedule, manual runs and tags (release builds), and
        skip it with a notice on branches, pull requests and main.

The runner comes from $OX_RUNNER (the matrix runner label), the event from
$GITHUB_EVENT_NAME. Names are desktop/tests/<name>.py stems or a gate step
name listed in GATES below.
"""
import datetime
import os
import pathlib
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[2]
LIST = ROOT / "desktop/ci/quarantine.toml"
GATES = {"packaged-window", "gate-x11", "gate-wayland-claude"}
BLOCKING_EVENTS = {"schedule", "workflow_dispatch"}
MAX_DAYS = 30


def blocking(event, ref):
    # Release builds reach desktop.yml through ci.yml's workflow_call, where
    # the event is the caller's push; the tag ref is what marks a release.
    return event in BLOCKING_EVENTS or ref.startswith("refs/tags/")


def known(name):
    return name in GATES or (ROOT / "desktop/tests" / f"{name}.py").is_file()


def entries():
    with open(LIST, "rb") as f:
        return tomllib.load(f).get("test", [])


def problems(items, today):
    errors = []
    for i, e in enumerate(items):
        name = e.get("name")
        if not isinstance(name, str) or not known(name):
            errors.append(f"entry {i}: unknown window test {name!r}")
        if not isinstance(e.get("issue"), int) or e["issue"] <= 0:
            errors.append(f"entry {i} ({name}): needs issue = <number>")
        runners = e.get("runners", [])
        if not isinstance(runners, list) or not all(isinstance(r, str) for r in runners):
            errors.append(f"entry {i} ({name}): runners must be a list of labels")
        until = e.get("until")
        if not isinstance(until, datetime.date):
            errors.append(f"entry {i} ({name}): needs until = YYYY-MM-DD (at most {MAX_DAYS} days ahead)")
        elif until < today:
            errors.append(f"entry {i} ({name}): quarantine expired on {until}; fix the test or renew with a reason")
        elif (until - today).days > MAX_DAYS:
            errors.append(f"entry {i} ({name}): until is more than {MAX_DAYS} days ahead")
    return errors


def check():
    errors = problems(entries(), datetime.date.today())
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
    ref = os.environ.get("GITHUB_REF", "")
    for e in entries():
        if e["name"] == name and (not e.get("runners") or runner in e["runners"]):
            issue = e["issue"]
            if not blocking(event, ref):
                print(f"::notice title=Quarantined::{name} skipped on {runner or 'this runner'} "
                      f"(issue #{issue}); it runs nightly")
                return 0
            print(f"{name} is quarantined (issue #{issue}); running it for the {event} run")
    sys.stdout.flush()
    os.execvp(command[0], command)


def self_test():
    today = datetime.date(2026, 1, 10)
    good = {"name": "keyboard_window", "issue": 7, "until": datetime.date(2026, 1, 20)}
    assert problems([good], today) == []
    assert problems([dict(good, issue=None)], today)
    assert problems([dict(good, name="nope")], today)
    assert any("expired" in p for p in problems([dict(good, until=datetime.date(2026, 1, 9))], today))
    assert problems([dict(good, until=datetime.date(2026, 3, 1))], today)
    assert problems([{k: v for k, v in good.items() if k != "until"}], today)
    assert blocking("schedule", "refs/heads/main") and blocking("workflow_dispatch", "refs/heads/x")
    assert blocking("push", "refs/tags/v0.1.0")
    assert not blocking("push", "refs/heads/main") and not blocking("pull_request", "refs/pull/8/merge")
    print("quarantine self-test OK")
    return 0


def main(argv):
    if argv == ["--check"]:
        return check()
    if argv == ["--self-test"]:
        return self_test()
    if len(argv) >= 3 and argv[1] == "--":
        return run(argv[0], argv[2:])
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
