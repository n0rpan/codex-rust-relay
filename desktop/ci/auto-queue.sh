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
# as "not reviewed": a failed lookup or a change in the reviewer's comment
# format stops automatic merges; it never lets one through.
set -euo pipefail
number=${1:?pass the pull request number}
repo=${GITHUB_REPOSITORY:?}
maintainer=${MAINTAINER:?}
# The reviewer's exact identity. A GitHub App posts as "<slug>[bot]" with type
# Bot, a name no person's account can have; GraphQL reports the same account
# as "<slug>" with type Bot. A prefix match would trust anyone who registers a
# longer name.
reviewer='chatgpt-codex-connector'

# Sets `decision` and, when the pull request could be read, `head`. It runs in
# this shell, not in a command substitution, so the commit it judged is the
# commit the merge request is bound to.
decision=''
head=''
decide() {
    local pr bodies summary reviewed threads blocking
    if ! pr="$(gh api "repos/${repo}/pulls/${number}")" || [ -z "${pr}" ]; then
        decision="hold: could not read the pull request"; return
    fi
    if [ "$(jq -r '.state' <<< "${pr}")" != open ]; then decision="skip: not open"; return; fi
    if [ "$(jq -r '.user.login' <<< "${pr}")" != "${maintainer}" ]; then decision="skip: not the maintainer's"; return; fi
    if [ "$(jq -r '.head.repo.full_name' <<< "${pr}")" != "${repo}" ]; then decision="skip: from another repository"; return; fi
    head="$(jq -r '.head.sha' <<< "${pr}")"
    if [ "$(jq -r '.draft' <<< "${pr}")" != false ]; then decision="hold: draft"; return; fi
    if ! jq -e '.labels | map(.name) | index("full-ci")' <<< "${pr}" > /dev/null; then decision="hold: no full-ci label"; return; fi

    # The reviewer keeps one summary comment with its state as escaped JSON.
    if ! bodies="$(gh api --paginate "repos/${repo}/issues/${number}/comments" \
        --jq ".[] | select(.user.login == \"${reviewer}[bot]\" and .user.type == \"Bot\") | .body")"; then
        decision="hold: could not read the comments"; return
    fi
    summary="$(grep -o 'codex-review-state:v1 [^>]*' <<< "${bodies}" | tail -n 1 |
        sed -e 's/^codex-review-state:v1 //' -e 's/ *--$//' -e 's/&#123;/{/g' -e 's/&#125;/}/g' || true)"
    reviewed="$(jq -r --arg head "${head}" \
        'select(.code_review.commit_sha == $head and .code_review.status == "completed") | "yes"' \
        <<< "${summary:-null}" 2>/dev/null || true)"
    if [ "${reviewed}" != yes ]; then decision="hold: no finished review of ${head:0:7}"; return; fi

    # One page of threads. With more than a page this holds: an unread thread
    # could be the blocking one.
    if ! threads="$(gh api graphql -f owner="${repo%/*}" -f name="${repo#*/}" -F number="${number}" -f query='
        query($owner: String!, $name: String!, $number: Int!) {
          repository(owner: $owner, name: $name) { pullRequest(number: $number) {
            reviewThreads(first: 100) { pageInfo { hasNextPage } nodes { isResolved
              comments(first: 1) { nodes { author { __typename login } body } } } } } } }' \
        --jq '.data.repository.pullRequest.reviewThreads')" || [ -z "${threads}" ]; then
        decision="hold: could not read the review threads"; return
    fi
    if [ "$(jq -r '.pageInfo.hasNextPage' <<< "${threads}")" != false ]; then
        decision="hold: more than 100 review threads, not all read"; return
    fi
    blocking="$(jq -r --arg reviewer "${reviewer}" '[.nodes[]
        | select(.isResolved | not) | .comments.nodes[0]
        | select(.author.__typename == "Bot" and .author.login == $reviewer)
        | select(.body | test("P[01] Badge"))] | length' <<< "${threads}")"
    if [ "${blocking}" != 0 ]; then decision="hold: ${blocking} unresolved P0/P1 finding(s)"; return; fi
    decision="queue: reviewed ${head:0:7}, nothing blocking"
}

decide
echo "#${number}: ${decision}"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then echo "- #${number}: ${decision}" >> "${GITHUB_STEP_SUMMARY}"; fi
if [ "${AUTO_QUEUE_DRY_RUN:-}" = 1 ]; then exit 0; fi
url="https://github.com/${repo}/pull/${number}"
case "${decision}" in
    # Bound to the commit that was judged: if a push landed since, GitHub
    # refuses, and that push's own run decides about the new commit.
    queue:*) gh pr merge "${url}" --auto --match-head-commit "${head}" ;;
    hold:*) gh pr merge "${url}" --disable-auto || true ;;
esac
# A lookup that failed is a fault someone should see, not a quiet hold.
case "${decision}" in
    "hold: could not read"*) exit 1 ;;
esac
