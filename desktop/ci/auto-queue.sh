#!/bin/bash
# Decide whether a pull request may merge by itself, and tell GitHub.
#
# Usage: desktop/ci/auto-queue.sh PULL_NUMBER
#   GH_TOKEN, GITHUB_REPOSITORY  as in a workflow
#   MAINTAINER                   login whose pull requests may be queued
#   AUTO_QUEUE_DRY_RUN=1         print the decision, change nothing
#
# A pull request is queued when all of these hold:
#   - it is open, not a draft, from this repository and from the maintainer;
#   - it carries the full-ci label;
#   - Codex has finished a code review of its current head commit;
#   - no unresolved Codex review thread is marked P0 or P1.
# Otherwise auto-merge is turned off. Anything this script cannot read counts
# as "not reviewed", so a change in the reviewer's comment format stops
# automatic merges; it never lets one through.
set -euo pipefail
number=${1:?pass the pull request number}
repo=${GITHUB_REPOSITORY:?}
maintainer=${MAINTAINER:?}
reviewer='chatgpt-codex-connector'

decide() {
    local pr head state summary reviewed blocking
    pr="$(gh api "repos/${repo}/pulls/${number}")"
    if [ "$(jq -r '.state' <<< "${pr}")" != open ]; then echo "skip: not open"; return; fi
    if [ "$(jq -r '.draft' <<< "${pr}")" != false ]; then echo "hold: draft"; return; fi
    if [ "$(jq -r '.user.login' <<< "${pr}")" != "${maintainer}" ]; then echo "skip: not the maintainer's"; return; fi
    if [ "$(jq -r '.head.repo.full_name' <<< "${pr}")" != "${repo}" ]; then echo "skip: from another repository"; return; fi
    if ! jq -e '.labels | map(.name) | index("full-ci")' <<< "${pr}" > /dev/null; then echo "hold: no full-ci label"; return; fi
    head="$(jq -r '.head.sha' <<< "${pr}")"

    # The reviewer keeps one summary comment with its state as escaped JSON.
    summary="$(gh api --paginate "repos/${repo}/issues/${number}/comments" \
        --jq ".[] | select(.user.login | startswith(\"${reviewer}\")) | .body" |
        grep -o 'codex-review-state:v1 [^>]*' | tail -n 1 |
        sed -e 's/^codex-review-state:v1 //' -e 's/ *--$//' -e 's/&#123;/{/g' -e 's/&#125;/}/g' || true)"
    reviewed="$(jq -r --arg head "${head}" \
        'select(.code_review.commit_sha == $head and .code_review.status == "completed") | "yes"' \
        <<< "${summary:-null}" 2>/dev/null || true)"
    if [ "${reviewed}" != yes ]; then echo "hold: no finished review of ${head:0:7}"; return; fi

    blocking="$(gh api graphql -f owner="${repo%/*}" -f name="${repo#*/}" -F number="${number}" -f query='
        query($owner: String!, $name: String!, $number: Int!) {
          repository(owner: $owner, name: $name) { pullRequest(number: $number) {
            reviewThreads(first: 100) { nodes { isResolved
              comments(first: 1) { nodes { author { login } body } } } } } } }' \
        --jq "[.data.repository.pullRequest.reviewThreads.nodes[]
               | select(.isResolved | not) | .comments.nodes[0]
               | select(.author.login | startswith(\"${reviewer}\"))
               | select(.body | test(\"P[01] Badge\"))] | length")"
    if [ "${blocking}" != 0 ]; then echo "hold: ${blocking} unresolved P0/P1 finding(s)"; return; fi
    echo "queue: reviewed ${head:0:7}, nothing blocking"
}

decision="$(decide)"
echo "#${number}: ${decision}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then echo "- #${number}: ${decision}" >> "${GITHUB_STEP_SUMMARY}"; fi
if [ "${AUTO_QUEUE_DRY_RUN:-}" = 1 ]; then exit 0; fi
url="https://github.com/${repo}/pull/${number}"
case "${decision}" in
    queue:*) gh pr merge "${url}" --auto ;;
    hold:*) gh pr merge "${url}" --disable-auto || true ;;
esac
