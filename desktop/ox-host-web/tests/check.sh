#!/usr/bin/env bash
# Linux mobile acceptance uses Chromium and its matching chromedriver; the Rust tests run on macOS too.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
cd "$root"
cargo test --profile "${OX_TEST_CARGO_PROFILE:-dev}" --manifest-path desktop/Cargo.toml --locked -p ox-host-web --features server,client --lib
npx --yes --package typescript@5.9.3 tsc --noEmit --allowJs --checkJs --target es2023 \
    --module esnext --moduleResolution bundler desktop/ox-host-web/assets/{wire,model}.js
node --test desktop/ox-host-web/tests/*.test.mjs
harness=$(cargo test --profile "${OX_TEST_CARGO_PROFILE:-dev}" --manifest-path desktop/Cargo.toml --locked -p ox-host-web --features server,client --lib \
    --no-run --message-format=json | jq -r 'select(.profile.test and .target.name=="ox_host_web" and .executable!=null) | .executable')
profile=${OX_TEST_CARGO_PROFILE:-dev}
if [[ "$profile" == dev ]]; then profile=debug; fi
export OX_TEST_SETTINGS_OX="${OX_TEST_OX_BIN:-$root/desktop/target/$profile/ox}"
if [[ ! -x "$OX_TEST_SETTINGS_OX" ]]; then
    cargo build --locked --manifest-path Cargo.toml --bin ox --profile "${OX_TEST_CARGO_PROFILE:-dev}"
    export OX_TEST_SETTINGS_OX="$root/target/$profile/ox"
fi
host="${OX_TEST_HOST_BIN:-$root/desktop/target/$profile/ox-host}"
if [[ -z "${OX_TEST_HOST_BIN:-}" ]]; then
    cargo build --locked --manifest-path desktop/Cargo.toml --profile "${OX_TEST_CARGO_PROFILE:-dev}" -p ox-host
fi
if [[ -n "${CODEOX_TEST_CODEX_BIN:-}" ]]; then
    OX_TEST_PARENT_HARNESS=$(cargo test --manifest-path desktop/Cargo.toml --locked --profile "${OX_TEST_CARGO_PROFILE:-dev}" -p ox-host --bin ox-host --no-run --message-format=json |
        jq -r 'select(.profile.test and .target.name=="ox-host" and .executable!=null) | .executable')
    export OX_TEST_PARENT_HARNESS
fi
OX_TEST_HOST_BIN="$host" python3 desktop/ox-host-web/tests/browser.py "$harness"
set -- "$host" "$OX_TEST_SETTINGS_OX" "$harness"
if [[ -n "${CODEOX_TEST_CODEX_BIN:-}" && -n "${OX_TEST_CLAUDE_BIN:-}" ]]; then
    set -- "$@" --stock "$CODEOX_TEST_CODEX_BIN" "$OX_TEST_CLAUDE_BIN"
fi
python3 desktop/ox-host-web/tests/native_client.py "$@"
