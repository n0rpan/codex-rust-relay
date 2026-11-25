#!/bin/sh
# Install stock Codex and Claude Code for one release track into a prefix.
#   baseline: the floors in desktop/gate.toml
#   latest:   the current npm releases
# Usage: desktop/ci/stock-clients.sh baseline|latest PREFIX
set -eu
track=${1:?baseline or latest}
prefix=${2:?install prefix}
floor() {
    python3 -c 'import sys, tomllib; print(tomllib.load(open("desktop/gate.toml", "rb"))[sys.argv[1]]["minimum_version"])' "$1"
}
case "$track" in
    baseline) codex=$(floor codex); claude=$(floor claude) ;;
    latest) codex=latest; claude=latest ;;
    *) echo "unknown track $track" >&2; exit 2 ;;
esac
npm install --prefix "$prefix" --no-audit --no-fund --loglevel=error \
    "@openai/codex@$codex" "@anthropic-ai/claude-code@$claude"
home=$(mktemp -d)
"$prefix/node_modules/.bin/codex" --version
env HOME="$home" CLAUDE_CONFIG_DIR="$home/profile" "$prefix/node_modules/.bin/claude" --version
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
        echo "- $track: $("$prefix/node_modules/.bin/codex" --version)"
        echo "- $track: $(env HOME="$home" CLAUDE_CONFIG_DIR="$home/profile" "$prefix/node_modules/.bin/claude" --version)"
    } >> "$GITHUB_STEP_SUMMARY"
fi
rm -rf "$home"
