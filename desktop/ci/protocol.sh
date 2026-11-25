#!/bin/sh
# Exercise the installed wire schema and the real desktop session for each release track.
set -eu
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
for version in 0.171.0 latest; do
    npm install --prefix "$stage/$version" --no-audit --no-fund "@openai/codex@$version"
    binary="$stage/$version/node_modules/.bin/codex"
    "$binary" --version
    # codex_thread_meta and codex_inject_items run in the fast lane.
    CODEOX_TEST_CODEX_BIN="$binary" cargo test --manifest-path desktop/Cargo.toml \
        --locked --release -p ox-desk-core --test contract_protocol -- --ignored --exact installed_protocol
    CODEOX_TEST_CODEX_BIN="$binary" cargo test --manifest-path desktop/Cargo.toml \
        --locked --release -p ox-desk-core --test handoff_codex -- --ignored --nocapture
    CODEOX_TEST_CODEX_BIN="$binary" cargo test --manifest-path desktop/Cargo.toml \
        --locked --release -p ox-desktop views::workbench::tests::stock_session -- --ignored --exact --nocapture
done
