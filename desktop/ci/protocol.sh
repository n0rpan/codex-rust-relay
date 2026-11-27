#!/bin/sh
# Exercise the installed wire schema and the real desktop session for each release track.
set -eu
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
contract=$(desktop/ci/test-bin.sh contract_protocol --release -p ox-desk-core --test contract_protocol)
handoff=$(desktop/ci/test-bin.sh handoff_codex --release -p ox-desk-core --test handoff_codex)
permissions=$(desktop/ci/test-bin.sh codex_permissions --release -p ox-desk-core --test codex_permissions)
workbench=$(desktop/ci/test-bin.sh ox-desktop --release -p ox-desktop)
for version in 0.171.0 latest; do
    npm install --prefix "$stage/$version" --no-audit --no-fund "@openai/codex@$version"
    binary="$stage/$version/node_modules/.bin/codex"
    "$binary" --version
    export CODEOX_TEST_CODEX_BIN="$binary"
    # codex_thread_meta and codex_inject_items run in the fast lane.
    # Each test runs in its package directory, where cargo test runs it.
    (cd desktop/ox-desk-core && "$contract" --ignored --exact installed_protocol)
    (cd desktop/ox-desk-core && "$handoff" --ignored --nocapture)
    (cd desktop/ox-desk-core && "$permissions" --ignored --nocapture)
    (cd desktop/ox-desktop && "$workbench" views::workbench::tests::stock_session --ignored --exact --nocapture)
done
