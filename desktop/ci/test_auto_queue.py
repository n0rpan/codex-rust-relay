"""auto-queue.sh queues only a labelled, reviewed pull request with nothing blocking."""
import json
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("auto-queue.sh")
HEAD = "9b8d038d0312e695dbee60d69dbc0c652097a9bd"
BOT = "chatgpt-codex-connector[bot]"

# A stand-in for the gh CLI: answers the three reads from files and records writes.
FAKE_GH = r"""#!/bin/bash
dir="$(dirname "$0")"
case "$*" in
    *"pr merge"*) echo "$*" >> "$dir/calls"; exit 0 ;;
    *graphql*) file=threads.json ;;
    */comments*) file=comments.json ;;
    */pulls/*) file=pull.json ;;
    *) echo "unexpected gh $*" >&2; exit 1 ;;
esac
filter=.
while [ $# -gt 0 ]; do
    if [ "$1" = --jq ]; then filter="$2"; fi
    shift
done
jq -r "$filter" "$dir/$file"
"""


def summary(sha, status):
    state = json.dumps({"code_review": {"commit_sha": sha, "status": status}})
    escaped = state.replace("{", "&#123;").replace("}", "&#125;")
    return f"<!-- codex-pull-request-review-summary -->\n<!-- codex-review-state:v1 {escaped} -->\n<details>"


def thread(body, resolved=False, author="chatgpt-codex-connector"):
    return {"isResolved": resolved, "comments": {"nodes": [{"author": {"login": author}, "body": body}]}}


class AutoQueue(unittest.TestCase):
    def decide(self, *, pull=None, comments=None, threads=None, dry_run=False):
        base = {"state": "open", "draft": False, "user": {"login": "maintainer"},
                "head": {"sha": HEAD, "repo": {"full_name": "o/r"}}, "labels": [{"name": "full-ci"}]}
        base.update(pull or {})
        if comments is None:
            comments = [{"user": {"login": BOT}, "body": summary(HEAD, "completed")}]
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "pull.json").write_text(json.dumps(base))
            (tmp / "comments.json").write_text(json.dumps(comments))
            (tmp / "threads.json").write_text(json.dumps(
                {"data": {"repository": {"pullRequest": {"reviewThreads": {"nodes": threads or []}}}}}))
            gh = tmp / "gh"
            gh.write_text(FAKE_GH)
            gh.chmod(gh.stat().st_mode | stat.S_IXUSR)
            env = dict(os.environ, PATH=f"{tmp}:{os.environ['PATH']}", GITHUB_REPOSITORY="o/r",
                       MAINTAINER="maintainer", GH_TOKEN="x")
            env.pop("GITHUB_STEP_SUMMARY", None)
            if dry_run:
                env["AUTO_QUEUE_DRY_RUN"] = "1"
            out = subprocess.run(["bash", str(SCRIPT), "38"], env=env, check=True,
                                 capture_output=True, text=True).stdout.strip()
            calls = (tmp / "calls").read_text() if (tmp / "calls").exists() else ""
            return out, calls

    def test_a_reviewed_labelled_pull_request_is_queued(self):
        out, calls = self.decide()
        self.assertIn("queue:", out)
        self.assertIn("--auto", calls)

    def test_a_resolved_or_minor_finding_does_not_block(self):
        out, calls = self.decide(threads=[thread("![P1 Badge](x) fixed", resolved=True),
                                          thread("![P2 Badge](x) nit"),
                                          thread("![P1 Badge](x) a person quoting it", author="someone")])
        self.assertIn("queue:", out)
        self.assertIn("--auto", calls)

    def test_an_unresolved_p0_or_p1_finding_turns_auto_merge_off(self):
        for badge in ("P0", "P1"):
            with self.subTest(badge):
                out, calls = self.decide(threads=[thread(f"**![{badge} Badge](x)** leak")])
                self.assertIn("hold: 1 unresolved", out)
                self.assertIn("--disable-auto", calls)
                self.assertNotIn("--auto\n", calls)

    def test_no_finished_review_of_the_head_commit_holds(self):
        cases = {
            "no review at all": [],
            "review of an older commit": [{"user": {"login": BOT}, "body": summary("0" * 40, "completed")}],
            "review still running": [{"user": {"login": BOT}, "body": summary(HEAD, "running")}],
            "a person imitating the summary": [{"user": {"login": "someone"}, "body": summary(HEAD, "completed")}],
            "a format this script cannot read": [{"user": {"login": BOT}, "body": "codex-review-state:v2 ???"}],
        }
        for name, comments in cases.items():
            with self.subTest(name):
                out, calls = self.decide(comments=comments)
                self.assertIn("hold: no finished review", out)
                self.assertIn("--disable-auto", calls)

    def test_draft_unlabelled_foreign_and_closed_pull_requests_are_never_queued(self):
        for name, pull, word in (
            ("draft", {"draft": True}, "hold: draft"),
            ("no label", {"labels": []}, "hold: no full-ci label"),
            ("another author", {"user": {"login": "someone"}}, "skip:"),
            ("a fork", {"head": {"sha": HEAD, "repo": {"full_name": "fork/r"}}}, "skip:"),
            ("closed", {"state": "closed"}, "skip:"),
        ):
            with self.subTest(name):
                out, calls = self.decide(pull=pull)
                self.assertIn(word, out)
                self.assertNotIn("--auto\n", calls)
                # A pull request this workflow does not own is left alone.
                if word == "skip:":
                    self.assertEqual(calls, "")

    def test_dry_run_changes_nothing(self):
        out, calls = self.decide(dry_run=True)
        self.assertIn("queue:", out)
        self.assertEqual(calls, "")


if __name__ == "__main__":
    unittest.main()
