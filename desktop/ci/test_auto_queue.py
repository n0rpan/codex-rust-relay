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
BOT_USER = {"login": BOT, "type": "Bot"}

# A stand-in for the gh CLI: answers the three reads from files and records writes.
FAKE_GH = r"""#!/bin/bash
dir="$(dirname "$0")"
fail="$(cat "$dir/fail" 2>/dev/null || true)"
case "$*" in
    *"pr merge"*|*"pr comment"*) echo "$*" >> "$dir/calls"; exit 0 ;;
    *graphql*) file=threads.json ;;
    */comments*) file=comments.json ;;
    */pulls/*) file=pull.json ;;
    *) echo "unexpected gh $*" >&2; exit 1 ;;
esac
if [ "$fail" = "$file" ]; then echo "HTTP 503" >&2; exit 1; fi
filter=.
while [ $# -gt 0 ]; do
    if [ "$1" = --jq ]; then filter="$2"; fi
    shift
done
jq -r "$filter" "$dir/$file"
"""


def summary(sha, status, security="completed", security_sha=None, unresolved=False):
    state = {"code_review": {"commit_sha": sha, "status": status}}
    if security:
        state["security_review"] = {"commit_sha": security_sha or sha, "status": security,
                                    "has_unresolved_findings": unresolved if security == "completed" else None}
    state = json.dumps(state)
    escaped = state.replace("{", "&#123;").replace("}", "&#125;")
    return f"<!-- codex-pull-request-review-summary -->\n<!-- codex-review-state:v1 {escaped} -->\n<details>"


def thread(body, resolved=False, author="chatgpt-codex-connector", kind="Bot"):
    return {"isResolved": resolved,
            "comments": {"nodes": [{"author": {"__typename": kind, "login": author}, "body": body}]}}


class AutoQueue(unittest.TestCase):
    def decide(self, *, pull=None, comments=None, threads=None, more_threads=False, dry_run=False, fail=None):
        base = {"state": "open", "draft": False, "user": {"login": "maintainer"},
                "head": {"sha": HEAD, "repo": {"full_name": "o/r"}}, "labels": [{"name": "full-ci"}]}
        base.update(pull or {})
        if comments is None:
            comments = [{"user": BOT_USER, "body": summary(HEAD, "completed")}]
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            (tmp / "pull.json").write_text(json.dumps(base))
            (tmp / "comments.json").write_text(json.dumps(comments))
            (tmp / "threads.json").write_text(json.dumps(
                {"data": {"repository": {"pullRequest": {"reviewThreads": {
                    "pageInfo": {"hasNextPage": more_threads}, "nodes": threads or []}}}}}))
            if fail:
                (tmp / "fail").write_text(fail)
            gh = tmp / "gh"
            gh.write_text(FAKE_GH)
            gh.chmod(gh.stat().st_mode | stat.S_IXUSR)
            env = dict(os.environ, PATH=f"{tmp}:{os.environ['PATH']}", GITHUB_REPOSITORY="o/r",
                       MAINTAINER="maintainer", GH_TOKEN="x")
            env.pop("GITHUB_STEP_SUMMARY", None)
            if dry_run:
                env["AUTO_QUEUE_DRY_RUN"] = "1"
            run = subprocess.run(["bash", str(SCRIPT), "38"], env=env, capture_output=True, text=True)
            self.assertEqual(run.returncode, 1 if fail else 0, run.stderr)
            calls = (tmp / "calls").read_text() if (tmp / "calls").exists() else ""
            return run.stdout.strip(), calls

    def test_a_reviewed_labelled_pull_request_is_queued_at_the_commit_that_was_judged(self):
        out, calls = self.decide()
        self.assertIn("queue:", out)
        self.assertIn(f"--auto --match-head-commit {HEAD}", calls)

    def test_a_read_that_fails_holds_and_fails_the_run(self):
        for file in ("pull.json", "comments.json", "threads.json"):
            with self.subTest(file):
                out, calls = self.decide(fail=file)
                self.assertIn("hold: could not read", out)
                self.assertIn("--disable-auto", calls)
                self.assertNotIn("--auto --match", calls)

    def test_a_resolved_or_minor_finding_does_not_block(self):
        out, calls = self.decide(threads=[thread("![P1 Badge](x) fixed", resolved=True),
                                          thread("![P2 Badge](x) nit"),
                                          thread("![P1 Badge](x) a person quoting it", author="someone", kind="User")])
        self.assertIn("queue:", out)
        self.assertIn("--auto", calls)

    def test_an_unresolved_p0_or_p1_finding_turns_auto_merge_off(self):
        for badge in ("P0", "P1"):
            with self.subTest(badge):
                out, calls = self.decide(threads=[thread(f"**![{badge} Badge](x)** leak")])
                self.assertIn("hold: 1 unresolved", out)
                self.assertIn("--disable-auto", calls)
                self.assertNotIn("--auto --match", calls)

    def test_threads_beyond_the_page_read_hold(self):
        out, calls = self.decide(more_threads=True)
        self.assertIn("hold: more than 100 review threads", out)
        self.assertIn("--disable-auto", calls)

    def test_no_finished_review_of_the_head_commit_holds(self):
        cases = {
            "no review at all": [],
            "review of an older commit": [{"user": BOT_USER, "body": summary("0" * 40, "completed")}],
            "review still running": [{"user": BOT_USER, "body": summary(HEAD, "running")}],
            "a person imitating the summary": [
                {"user": {"login": "someone", "type": "User"}, "body": summary(HEAD, "completed")}],
            "a longer name that starts like the reviewer's": [
                {"user": {"login": "chatgpt-codex-connector-untrusted", "type": "User"},
                 "body": summary(HEAD, "completed")}],
            "another app with a name that starts like the reviewer's": [
                {"user": {"login": "chatgpt-codex-connector-x[bot]", "type": "Bot"},
                 "body": summary(HEAD, "completed")}],
            "a person's account with the reviewer's bare name": [
                {"user": {"login": "chatgpt-codex-connector", "type": "User"},
                 "body": summary(HEAD, "completed")}],
            "a format this script cannot read": [{"user": BOT_USER, "body": "codex-review-state:v2 ???"}],
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
                self.assertNotIn("--auto --match", calls)
                # A pull request this workflow does not own is left alone.
                if word == "skip:":
                    self.assertEqual(calls, "")

    def test_a_head_commit_without_a_security_review_gets_one_requested_once(self):
        for comments in ([{"user": BOT_USER, "body": summary(HEAD, "completed", security=None)}],
                         [{"user": BOT_USER, "body": summary(HEAD, "running", security="completed", security_sha="0" * 40)}]):
            out, calls = self.decide(comments=comments)
            self.assertIn("hold:", out)
            self.assertIn("pr comment", calls)
            self.assertIn(f"@codex security review\n\nAsked for by the `full-ci` label, for commit {HEAD}.", calls)
            self.assertIn("--disable-auto", calls)
            asked = {"user": {"login": "maintainer", "type": "User"},
                     "body": f"@codex security review\n\nAsked for by the `full-ci` label, for commit {HEAD}."}
            out, calls = self.decide(comments=comments + [asked])
            self.assertIn("hold:", out)
            self.assertNotIn("pr comment", calls)
        # Someone else's comment is not the maintainer's request.
        asked["user"] = {"login": "someone", "type": "User"}
        out, calls = self.decide(comments=comments + [asked])
        self.assertIn("pr comment", calls)

    def test_a_running_or_unresolved_security_review_holds_without_a_new_request(self):
        for state in ({"security": "running"}, {"security": "completed", "unresolved": True}):
            out, calls = self.decide(comments=[{"user": BOT_USER, "body": summary(HEAD, "completed", **state)}])
            self.assertIn("security review", out)
            self.assertNotIn("pr comment", calls)
            self.assertNotIn("--auto", calls)

    def test_a_finished_security_review_with_nothing_reported_is_clean(self):
        # What Codex wrote for a finished review with no findings on #39.
        out, calls = self.decide(comments=[{"user": BOT_USER, "body": summary(HEAD, "completed", unresolved=None)}])
        self.assertIn("queue:", out)
        self.assertNotIn("pr comment", calls)

    def test_an_unresolved_security_thread_holds_at_any_severity(self):
        finding = thread("**P2 Badge** leak\n<!-- codex-security-review-finding:v1 -->")
        out, calls = self.decide(threads=[finding])
        self.assertIn("hold:", out)
        self.assertNotIn("--auto ", calls)

    def test_no_security_review_is_requested_without_the_label_or_for_a_draft(self):
        bare = [{"user": BOT_USER, "body": summary(HEAD, "completed", security=None)}]
        for pull in ({"labels": []}, {"draft": True}):
            out, calls = self.decide(pull=pull, comments=bare)
            self.assertNotIn("pr comment", calls)

    def test_dry_run_changes_nothing(self):
        out, calls = self.decide(dry_run=True)
        self.assertIn("queue:", out)
        self.assertEqual(calls, "")


if __name__ == "__main__":
    unittest.main()
