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
#   - Codex has finished a security review of that commit with nothing open;
#   - no unresolved Codex review thread is marked P0 or P1.
# Otherwise auto-merge is turned off. A labelled pull request whose head
# commit has no security review gets one requested, once per commit, by a
# comment from the maintainer's token: the security review runs by itself
# only when a pull request opens. Anything this script cannot read counts
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

# Sets `decision`, when the pull request could be read `head`, and `request`
# when a security review of that commit has to be asked for. It runs in
# this shell, not in a command substitution, so the commit it judged is the
# commit the merge request is bound to.
decision=''
head=''
request=''
decide() {
    local pr comments summary reviewed security threads blocking
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
    if ! comments="$(gh api --paginate "repos/${repo}/issues/${number}/comments" \
        --jq '.[] | {login: .user.login, type: .user.type, body: .body}' | jq -s .)"; then
        decision="hold: could not read the comments"; return
    fi
    summary="$(jq -r --arg bot "${reviewer}[bot]" \
        '.[] | select(.login == $bot and .type == "Bot") | .body' <<< "${comments}" |
        grep -o 'codex-review-state:v1 [^>]*' | tail -n 1 |
        sed -e 's/^codex-review-state:v1 //' -e 's/ *--$//' -e 's/&#123;/{/g' -e 's/&#125;/}/g' || true)"
    # Before any hold below, so the two reviews run side by side.
    security="$(jq -r --arg head "${head}" '.security_review | select(.commit_sha == $head)
        | if .status != "completed" then "running"
          elif .has_unresolved_findings == false then "clean" else "open" end' \
        <<< "${summary:-null}" 2>/dev/null || true)"
    if [ -z "${security}" ] && ! jq -e --arg who "${maintainer}" --arg head "${head}" \
        'any(.[]; .login == $who and (.body | startswith("@codex security review")) and (.body | contains($head)))' \
        <<< "${comments}" > /dev/null; then
        request=yes
    fi
    reviewed="$(jq -r --arg head "${head}" \
        'select(.code_review.commit_sha == $head and .code_review.status == "completed") | "yes"' \
        <<< "${summary:-null}" 2>/dev/null || true)"
    if [ "${reviewed}" != yes ]; then decision="hold: no finished review of ${head:0:7}"; return; fi
    case "${security}" in
        clean) ;;
        open) decision="hold: the security review of ${head:0:7} has unresolved findings"; return ;;
        *) decision="hold: no finished security review of ${head:0:7}"; return ;;
    esac

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
# After auto-merge is off, so a comment that cannot be posted leaves nothing
# queued. The commit is in the text so that a later run finds this request
# and does not repeat it.
if [ "${request}" = yes ]; then
    gh pr comment "${url}" --body "@codex security review

Asked for by the \`full-ci\` label, for commit ${head}."
fi
# A lookup that failed is a fault someone should see, not a quiet hold.
case "${decision}" in
    "hold: could not read"*) exit 1 ;;
esac
