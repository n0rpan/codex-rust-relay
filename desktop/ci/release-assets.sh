#!/usr/bin/env bash
# Usage: release-assets.sh [--unsigned] <artifacts> <version>
#
# Verifies that <artifacts>, this run's downloaded `ox-*` artifacts (one
# directory per artifact), holds every file a release of <version> uploads and
# that each matches its .sha256, then prints the files one per line in upload
# order. It reads only that directory. Both publish jobs in ci.yml upload
# exactly this list after install.sh, and the release-assets job runs the same
# check on a full-ci pull request, so a renamed artifact or file fails there
# and not on main.
#
# macOS ships only the Developer ID signed and notarized builds (jobs
# sign-macos in desktop.yml and sign-cli in ci.yml), never the ad hoc ones the
# build jobs tested. A pull request cannot sign, so --unsigned looks for the
# same files in the artifacts the build jobs upload.
set -euo pipefail

signed="signed-"
if [ "${1:-}" = "--unsigned" ]; then
    signed=""
    shift
fi
if [ "$#" -ne 2 ] || [ -z "$1" ] || [ -z "$2" ]; then
    echo "usage: release-assets.sh [--unsigned] <artifacts> <version>" >&2
    exit 2
fi
artifacts="$1"
version="$2"
files=()
failed=0

# append <artifact> <file>...: each file and its checksum, in that order.
append() {
    local directory="$artifacts/$1" file
    shift
    for file in "$@"; do
        if [ ! -f "$directory/$file" ] || [ ! -f "$directory/$file.sha256" ]; then
            [ -f "$directory/$file" ] || echo "::error::release asset missing: $directory/$file" >&2
            [ -f "$directory/$file.sha256" ] || echo "::error::release asset missing: $directory/$file.sha256" >&2
            failed=1
        elif ! (cd "$directory" && sha256sum --check --strict "$file.sha256") >&2; then
            echo "::error::release asset does not match its checksum: $directory/$file" >&2
            failed=1
        fi
        files+=("$directory/$file" "$directory/$file.sha256")
    done
}

for entry in \
    ubuntu-24.04:linux-x86_64.tar.gz \
    ubuntu-24.04-arm:linux-aarch64.tar.gz \
    "${signed}macos-26:macos-arm64.dmg" \
    "${signed}macos-26-intel:macos-x86_64.dmg" \
    "${signed}macos-26:macos-arm64.zip" \
    "${signed}macos-26-intel:macos-x86_64.zip"; do
    append "ox-desktop-${entry%%:*}" "ox-desktop-${entry#*:}"
done
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin; do
    artifact="ox-$target"
    if [[ "$target" == *-apple-darwin ]]; then artifact="ox-$signed$target"; fi
    append "$artifact" "ox-$version-$target" "ox-$version-$target.tar.gz" "ox-$target.tar.gz"
done

if [ "$failed" -ne 0 ]; then
    echo "::error::the release assets in $artifacts are incomplete; nothing may be published from them" >&2
    exit 1
fi
printf '%s\n' "${files[@]}"
