#!/usr/bin/env bash
# Print the path of one test executable of the desktop workspace.
#   Usage: desktop/ci/test-bin.sh TARGET CARGO-ARGS...
# TARGET is cargo's name for the target: an integration test
# (contract_protocol), a library (ox_host_web) or a binary (ox-desktop).
#
# With OX_TEST_ARTIFACTS naming the output of
#   cargo test --workspace --no-run --message-format=json-render-diagnostics
# the executable is read from that file and nothing is compiled. The package
# job builds every test once that way. Cargo resolves features for the
# packages it is asked for, so each "-p one-package" build that followed the
# workspace build compiled the shared crates and the test a second time.
# Without the variable CARGO-ARGS select the build, as on a developer machine.
set -euo pipefail
target=${1:?pass the target name}
shift
root=$(cd "$(dirname "$0")/../.." && pwd)
pick() {
    jq -r --arg target "$target" \
        'select(.reason == "compiler-artifact" and .profile.test and .target.name == $target and .executable != null) | .executable'
}
if [[ -n "${OX_TEST_ARTIFACTS:-}" ]]; then
    executable=$(pick < "$OX_TEST_ARTIFACTS")
else
    executable=$(cargo test --no-run --locked --manifest-path "$root/desktop/Cargo.toml" \
        --message-format=json-render-diagnostics "$@" | pick)
fi
# Exactly one path: no answer, or two targets of one name, both fail here.
[[ -x "$executable" ]] || { echo "No single test executable named $target." >&2; exit 1; }
echo "$executable"
