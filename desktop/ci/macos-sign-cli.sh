#!/bin/sh
# Sign the standalone macOS `ox` release binary with Developer ID, notarize it
# and rebuild its archives and checksums.
#
# Usage: desktop/ci/macos-sign-cli.sh INPUT_DIR VERSION TARGET OUTPUT_DIR
#   INPUT_DIR holds the release job's ox-VERSION-TARGET executable.
#   OX_CODESIGN_IDENTITY, OX_CODESIGN_KEYCHAIN, OX_SIGNING_TEAM: from macos-keychain.sh
#
# A bare executable cannot carry a stapled ticket; Gatekeeper looks the
# notarization up online when a quarantined copy first runs.
set -eu
input=${1:?pass the artifact directory}
version=${2:?pass the version}
target=${3:?pass the target triple}
output=${4:?pass the output directory}
. "$(dirname "$0")/macos-release.sh"
ox_require OX_CODESIGN_IDENTITY OX_SIGNING_TEAM
executable="ox-$version-$target"
archive="$executable.tar.gz"
alias="ox-$target.tar.gz"
(cd "$input" && shasum -a 256 --check "$executable.sha256")
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# Artifact downloads drop the executable bit.
install -m 755 "$input/$executable" "$work/ox"
set -- --force --options runtime --timestamp --identifier dev.ox.cli --sign "$OX_CODESIGN_IDENTITY"
if [ -n "${OX_CODESIGN_KEYCHAIN:-}" ]; then set -- "$@" --keychain "$OX_CODESIGN_KEYCHAIN"; fi
codesign "$@" "$work/ox"
codesign --verify --strict -R="$(ox_requirement)" "$work/ox"
ditto -c -k "$work/ox" "$work/submit.zip"
ox_notarize "$work/submit.zip" "$work"

mkdir -p "$output"
rm -f "$output/$executable" "$output/$archive" "$output/$alias"
cp "$work/ox" "$output/$executable"
tar --create --gzip --file "$output/$archive" --directory "$work" ox
cp "$output/$archive" "$output/$alias"
(
    cd "$output"
    # Each .sha256 file names the file it checks.
    for file in "$archive" "$executable" "$alias"; do
        shasum -a 256 "$file" > "$file.sha256"
    done
)
# Check the archive that ships.
mkdir "$work/shipped"
tar --extract --gzip --file "$output/$alias" --directory "$work/shipped"
cmp "$work/shipped/ox" "$output/$executable"
codesign --verify --strict -R="$(ox_requirement)" "$work/shipped/ox"
case "$target" in
    "$(uname -m | sed 's/arm64/aarch64/')"-*)
        [ "$("$work/shipped/ox" --version)" = "ox $version" ]
        echo "Signed, notarized and started ox $version ($target)"
        ;;
    *) echo "Signed and notarized ox $version ($target; another architecture, not started here)" ;;
esac
