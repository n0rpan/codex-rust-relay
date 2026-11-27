# Working on OX

Read this before changing anything. It is the same for every agent and every
person. `CLAUDE.md` only points here.

## What OX is, and what follows from it

OX is a power tool for developers. The model is the user's advisor and the user
decides.

- **Defaults match stock Claude Code and Codex in a terminal.** OX adds no
  prompt, confirmation or restriction of its own by default.
- **Do not design against a misbehaving model.** We do not make the product
  worse to protect the user from the tool they chose to run.
- **A guardrail is a setting.** If something like YOLO mode deserves a guard,
  it is an option in Settings that the user controls. It is not hard-coded.
- **Before adding a block, confirmation or limit**, name the irreversible harm
  it prevents. If there is none, do not add it. If there is one, add the
  override with it. Example: a signed install refuses unsigned updates, and
  `OX_ALLOW_UNSIGNED=1` turns that off.

## How a change reaches users

1. **Every push to a pull request** runs the scoped fast checks, about 5
   minutes. `desktop/ci/pr_scope.py` picks extra lanes from the files changed.
2. **The `full-ci` label** runs the legs `main` builds: four release binaries,
   three native test legs, four desktop packages. About 40 minutes.
3. **The merge queue** tests the pull request on top of current `main` and
   merges it. A branch does not have to be current with `main`, so do not
   update one just to catch up.
4. **`main`** builds, signs with Developer ID, notarizes and publishes the
   rolling `dev` release. A push builds and publishes nothing when everything
   since the released commit is documentation.
5. **A `v*` tag** publishes a stable release. Only the maintainer creates tags.

## Rules for agents

- **Push per finished step, not per edit.** Each push cancels the run before
  it.
- **Add `full-ci` last, and only when you would merge the change yourself.**
  On a pull request from the maintainer's account that is not a draft, the
  label also queues it for merge once it is green and Codex has reviewed the
  head commit with no unresolved P0 or P1 finding and no open security
  finding (`auto-queue.yml`). A push takes it out of line until the new
  commit is reviewed. Keep the pull
  request a draft if you want the full run without the merge. It costs about
  40 minutes of runners on every later push. Use it when code, packaging or
  workflows changed; documentation does not need it.
- **Do not merge, enqueue, tag or change repository settings** unless the
  maintainer asked for that action.
- **A red check is yours until you have shown otherwise.** Read the log. If the
  failure is in code you did not touch, say so on the pull request with the
  job, the test and the evidence.
- **One re-run per job, at most.** "Flaky" is not a cause. A test that fails
  and then passes on the same code has a bug: find it and fix the test or the
  product. Reproduce locally by running the test binary a few hundred times.
- **Never skip, ignore or quarantine a test to get green.** Every test is there
  for a reason, or it is removed with that reason written down.
- **Say what you did not verify.** A workflow change is unverified until the
  path it changes has actually run.

## While `main` moves under you

Branches are not kept current, and several agents work at once. The queue
builds and tests your change on top of whatever `main` is at that moment. That
catches a broken build or a failing test. It does not catch two changes that
each work and do not belong together, because git merges them cleanly:

- the same feature built twice, in two places;
- the same setting, flag, protocol field or file format added with two
  meanings, or one side changing what the other relies on;
- two changes to the same screen or the same flow that were each designed as
  if the other did not exist;
- a rule or a doc that now contradicts one that landed meanwhile.

So, before you call work finished:

- Fetch `main` and read what landed since you branched in the areas you
  touched: `git fetch origin main && git log --oneline HEAD..origin/main -- <paths>`.
- If something there overlaps, merge `main` into your branch and reconcile it
  on purpose. That is the one reason to bring `main` in by hand.
- Say in the description which commit of `main` you checked against.

## What a pull request cannot test

These run for the first time on `main` or a tag. A green pull request says
nothing about them, so a change to any of them needs a run of that path before
you call it done.

| Path | Where it lives | How to exercise it before merge |
| --- | --- | --- |
| Developer ID signing and notarization | `sign-cli` in `ci.yml`, `sign-macos` in `desktop.yml`, `desktop/ci/macos-*.sh` | Manual OX Desktop run with `sign` on a ref the `apple-release` environment admits. Ask the maintainer to admit the branch for that run. |
| The release asset list | `desktop/ci/release-assets.sh`, used by both publish jobs | `full-ci` runs it on the pull request's unsigned artifacts (job `release-assets`). Signing itself still runs only on `main`. |
| Publishing | `publish-dev` and `publish` in `ci.yml` | The upload and the swap cannot be rehearsed. Keep changes small and watch the first `main` run. |
| Tagged release | `publish` in `ci.yml` | Has never run. Rehearse before the first `v*` tag. |
| Intel desktop package | `package (macos-26-intel)` | `full-ci`, or a manual OX Desktop run with `intel`. |

## Traps we have already paid for

- **A called workflow gets no secrets by default.** `ci.yml` calls
  `desktop.yml`; without `secrets: inherit` the signing job received an empty
  certificate and failed after 36 minutes of green builds.
- **Starting a workflow directly is not the same as having it called.** A
  manual run of `desktop.yml` resolved the secrets that the called run did not.
  Test the path that production uses.
- **`actions/download-artifact` with a pattern** unpacks one match straight
  into the path and several matches into a directory each. Find files, do not
  assume the layout.
- **Linux runners deny unprivileged user namespaces.** Stock Codex needs them
  for its sandbox. Every Linux leg that runs Codex lifts the restriction
  first; the probe must unshare the network, as Codex does.
- **A platform a pull request skips is a platform it can break.** Linux ARM
  and macOS Intel only run with `full-ci` or on `main`.
- **Any failed job on `main` loses the release for every platform.**
  `publish-dev` needs every job. `retry-main.yml` re-runs failed jobs once;
  it does not make an intermittent test acceptable.
- **`cargo test -p one-package` is not a slice of `--workspace`.** Cargo
  resolves features for the packages it is asked for, so the narrower build
  compiles the shared crates and the test again. The package job builds every
  test once; its scripts get executables from `desktop/ci/test-bin.sh`.
- **On macOS, a pseudo-terminal can drop what a program printed.** A test
  that closes its own copy of the terminal's writing end and then reads loses
  the whole output of a program that prints and exits at once: 1 run in 1500
  on macOS Intel, none on macOS ARM or Linux. Keep that end open until the
  program has exited and the output is drained
  (`tests/fixtures/setup_selector.py`).
- **Tests that share a binary share a process.** A test that depends on a
  file descriptor being closed can be broken by a sibling test that spawns a
  child at the same moment.
- **A test that writes a script and runs it can get "Text file busy".** The
  sibling's child holds the script's write descriptor until its exec. Write
  it with `executable::write` from `tests/common/executable.rs`.

## Secrets

- The certificate and the App Store Connect key live only in the
  `apple-release` environment. Only the two signing jobs enter it.
- Never reference a secret in a job that compiles third-party crates or
  installs third-party packages. That is why signing is a separate job.
- Which refs may enter the environment is a repository setting (`main` and
  `v*` tags), not something the workflow files enforce.

## Review guidelines

For anyone reviewing a pull request, human or automated. Security review has
its own rules in `SECURITY.md` and `docs/threat-model.md`.

Blocking:

- A test that is skipped, ignored, quarantined, retried or has a weakened
  assertion, without a root cause written down.
- A timeout made longer without a measurement showing the wait is slow.
- A secret referenced in a job that compiles or installs third-party code, or
  reachable from a pull request that does not come from this repository.
- A change to a path only `main` or a tag runs (signing, publishing, the
  release asset list) with no statement of how it was exercised.
- A new prompt, confirmation, block or limit on what the user or the agent may
  do, without a setting to turn it off and a named irreversible harm.
- A default that is stricter than stock Claude Code or Codex.
- A claim in the description that the diff or a run does not support.
- A change that duplicates or contradicts something on `main` today: the same
  feature built a second time, or a setting, flag, protocol field or format
  given a different meaning. Review against current `main`, not against the
  commit the branch started from; a clean merge does not show this.

Not worth a comment:

- Style the formatter and clippy already accept.
- A missing guard against the model or the user doing what they asked for.
- A request for more abstraction, more configuration or more comments.

Say plainly when a finding is a guess. Give the file, the line and the input
that goes wrong.

## Before asking for a merge

- Open a pull request ready for review, not as a draft: reviews run when it is
  opened, marked ready and on every push. A draft is only for holding a
  `full-ci` pull request back from merging by itself.
- Codex reviews the code of a ready pull request by itself, when it is opened
  and on every push. Wait for the review of the head commit and answer or fix
  every finding. A review of an earlier commit does not cover a later push.
- The security review runs by itself only when the pull request opens. The
  `full-ci` label asks for one of the head commit, and again after each
  later push; do not ask by hand as well. Ask by hand
  (`@codex security review`) only after resolving a security finding.
- Ask by hand (`@codex review`) in two cases only: no review of the head
  commit has appeared, or you resolved a P0 or P1 thread. Resolving alone
  does not put the pull request back in line.
- Resolve a P0 or P1 thread only when it is fixed or shown to be wrong.
- Then add `full-ci`.

## Local checks before you push

```sh
cargo fmt --all -- --check && cargo clippy --locked --all-targets -- -D warnings && cargo test --locked
cargo fmt --manifest-path desktop/Cargo.toml --all -- --check
sh -n install.sh && bash -n install.sh && shellcheck install.sh
python3 -m unittest discover -s desktop/ci -p 'test_*.py'
```

## Keeping this file true

When CI teaches you something that would have saved an hour, add one line here
with the reason. When a line stops being true, delete it. Keep the file short
enough to read in full.
