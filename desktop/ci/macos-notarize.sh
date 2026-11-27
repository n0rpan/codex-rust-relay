#!/bin/sh
# Re-sign a packaged macOS archive with Developer ID, notarize it, staple the
# ticket and write the archive that ships, with its checksum.
#
# Usage: desktop/ci/macos-notarize.sh INPUT_ZIP OUTPUT_DIR
#   OX_CODESIGN_IDENTITY, OX_CODESIGN_KEYCHAIN, OX_SIGNING_TEAM: from macos-keychain.sh
#   APPLE_API_KEY_P8, APPLE_API_KEY_ID, APPLE_API_ISSUER_ID: App Store Connect API key
set -eu
input=${1:?pass the packaged archive}
output=${2:?pass the output directory}
ci=$(cd "$(dirname "$0")" && pwd)
name=$(basename "$input")
for variable in OX_CODESIGN_IDENTITY OX_SIGNING_TEAM APPLE_API_KEY_P8 APPLE_API_KEY_ID APPLE_API_ISSUER_ID; do
    eval "value=\${$variable:-}"
    if [ -z "$value" ]; then
        echo "::error::$variable is empty. Store it as a secret of the apple-release environment." >&2
        exit 1
    fi
done
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
key="$work/AuthKey.p8"
(umask 077 && printf '%s\n' "$APPLE_API_KEY_P8" > "$key")
ditto -x -k "$input" "$work/app"
bundle="$work/app/OX Desktop.app"
[ -d "$bundle" ] || { echo "::error::$name does not contain OX Desktop.app." >&2; exit 1; }
"$ci/macos-sign.sh" "$bundle"

# Developer ID issued by Apple to this team, for every executable in the bundle.
requirement="anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$OX_SIGNING_TEAM\""
for code in "$bundle" "$bundle/Contents/MacOS/ox" "$bundle/Contents/MacOS/ox-host"; do
    codesign --verify --strict -R="$requirement" "$code"
done
codesign --display --verbose=2 "$bundle" 2>&1 | grep -E '^(Identifier|TeamIdentifier|Timestamp|Runtime Version|CodeDirectory)'

# Apple notarizes an archive, but the ticket is stapled to the bundle, so the
# shipped archive is made after stapling.
ditto -c -k --keepParent "$bundle" "$work/submit.zip"
result="$work/notary.json"
status=0
xcrun notarytool submit "$work/submit.zip" --key "$key" --key-id "$APPLE_API_KEY_ID" \
    --issuer "$APPLE_API_ISSUER_ID" --wait --timeout 30m --output-format json > "$result" || status=$?
cat "$result"; echo
id=$(jq -r '.id // empty' "$result" 2>/dev/null || true)
verdict=$(jq -r '.status // empty' "$result" 2>/dev/null || true)
if [ "$status" -ne 0 ] || [ "$verdict" != Accepted ]; then
    if [ -n "$id" ]; then
        xcrun notarytool log "$id" --key "$key" --key-id "$APPLE_API_KEY_ID" \
            --issuer "$APPLE_API_ISSUER_ID" || true
    fi
    echo "::error::Apple did not notarize $name (status: ${verdict:-none})." >&2
    exit 1
fi
xcrun stapler staple "$bundle"
xcrun stapler validate "$bundle"
# What Gatekeeper decides for a downloaded copy.
spctl --assess --type execute --verbose=2 "$bundle"

mkdir -p "$output"
rm -f "$output/$name" "$output/$name.sha256"
ditto -c -k --keepParent "$bundle" "$output/$name"
(cd "$output" && shasum -a 256 "$name" > "$name.sha256")

# Check the archive that ships, not the tree it was made from.
ditto -x -k "$output/$name" "$work/shipped"
shipped="$work/shipped/OX Desktop.app"
codesign --verify --strict --deep -R="$requirement" "$shipped"
xcrun stapler validate "$shipped"
# Start each executable this runner can run under the hardened runtime.
case "$name" in
    *-"$(uname -m)".zip)
        version=$("$shipped/Contents/MacOS/ox" --version)
        version=${version#ox }
        [ "$("$shipped/Contents/MacOS/ox-host" --version)" = "ox-host $version" ]
        [ "$("$shipped/Contents/MacOS/ox-desktop" --version)" = "ox-desktop $version" ]
        echo "Signed, notarized and started OX $version ($name)"
        ;;
    *) echo "Signed and notarized $name (another architecture; not started here)" ;;
esac
