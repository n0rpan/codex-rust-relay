#!/usr/bin/env bash
# Exercise both endpoint profiles, the stock CLI and the matching workbench test binary.
set -euo pipefail
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
floor=$(python3 -c 'import tomllib; print(tomllib.load(open("desktop/gate.toml", "rb"))["claude"]["minimum_version"])')
latest=$(npm view @anthropic-ai/claude-code@latest version)
ox=$(realpath "${1:?pass the path to the built OX helper}")
OX_CLAUDE_WORKBENCH_TEST_BIN=$(cargo test --no-run --locked --release --manifest-path desktop/Cargo.toml \
    -p ox-desktop --message-format=json | jq -r 'select(.profile.test == true and .target.name == "ox-desktop" and .executable != null) | .executable')
test -x "$OX_CLAUDE_WORKBENCH_TEST_BIN"
export OX_CLAUDE_WORKBENCH_TEST_BIN
versions=("$floor")
if [ "$latest" != "$floor" ]; then versions+=("$latest"); fi
mkdir -p "$stage/home" "$stage/profile"
for version in "${versions[@]}"; do
    npm install --prefix "$stage/stock" --no-audit --no-fund "@anthropic-ai/claude-code@$version"
    binary="$stage/stock/node_modules/.bin/claude"
    actual=$(env HOME="$stage/home" CLAUDE_CONFIG_DIR="$stage/profile" "$binary" --version)
    test "$actual" = "$version (Claude Code)"
    echo "Verifying $actual"
    # claude_subcommands, claude_mcp_meta and the host session fixtures run
    # in the fast lane (fast.yml); this job keeps what needs the packaged build.
    python3 desktop/tests/claude_stock_session.py "$ox" "$binary"
done

title_test=$(cargo test --no-run --locked --release --manifest-path desktop/Cargo.toml \
    -p ox-desk-core --lib --message-format=json | jq -r 'select(.profile.test == true and .target.name == "ox_desk_core" and .executable != null) | .executable')
test -x "$title_test"
codex_floor=$(python3 -c 'import tomllib; print(tomllib.load(open("desktop/gate.toml", "rb"))["codex"]["minimum_version"])')
codex_latest=$(npm view @openai/codex@latest version)
for track in floor latest; do
    if [[ "$track" == floor ]]; then
        cc=$floor
        codex=$codex_floor
    else
        cc=$latest
        codex=$codex_latest
        if [[ "$cc" == "$floor" && "$codex" == "$codex_floor" ]]; then break; fi
    fi
    npm install --prefix "$stage/titles" --no-audit --no-fund \
        "@openai/codex@$codex" "@anthropic-ai/claude-code@$cc"
    python3 desktop/tests/stock_titles.py "$title_test" "$ox" \
        "$stage/titles/node_modules/.bin/codex" "$stage/titles/node_modules/.bin/claude"
done
