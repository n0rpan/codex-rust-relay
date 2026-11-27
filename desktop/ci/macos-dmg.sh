#!/bin/sh
# Verify the Finder image, or place the release's signed app in the same layout.
# Usage: macos-dmg.sh INPUT_DMG [SIGNED_APP OUTPUT_DMG]
set -eu
input=${1:?pass the packaged disk image}
work=$(mktemp -d)
mount="$work/mount"
mounted=0
cleanup() {
    if [ "$mounted" = 1 ] && ! hdiutil detach "$mount"; then
        hdiutil detach -force "$mount" || return 1
    fi
    rm -rf "$work"
}
trap cleanup EXIT

attach() {
    mkdir -p "$mount"
    hdiutil attach -nobrowse -noautoopen -mountpoint "$mount" "$@"
    mounted=1
}

check_layout() {
    [ "$(readlink "$mount/Applications")" = /Applications ]
    [ -s "$mount/.DS_Store" ]
    [ -s "$mount/.background.png" ]
}

if [ "$#" -eq 1 ]; then
    attach -readonly "$input"
    check_layout
    copied="$work/OX Desktop.app"
    ditto "$mount/OX Desktop.app" "$copied"
    codesign --verify --strict --deep "$copied"
    bin="$copied/Contents/MacOS"
    version=$("$bin/ox" --version)
    [ "$("$bin/ox-host" --version)" = "ox-host ${version#ox }" ]
    [ "$("$bin/ox-desktop" --version)" = "ox-desktop ${version#ox }" ]
    exit
fi

bundle=${2:?pass the signed app bundle}
output=${3:?pass the release disk image path}
ci=$(cd "$(dirname "$0")" && pwd)
. "$ci/macos-release.sh"
ox_require OX_CODESIGN_IDENTITY OX_CODESIGN_KEYCHAIN OX_SIGNING_TEAM
requirement=$(ox_requirement)

# Keep the filesystem and its Finder background bookmark intact.
hdiutil convert "$input" -format UDRW -o "$work/writable.dmg"
attach "$work/writable.dmg"
check_layout
rm -rf "$mount/OX Desktop.app"
ditto "$bundle" "$mount/OX Desktop.app"
hdiutil detach "$mount"
mounted=0
hdiutil convert "$work/writable.dmg" -format UDZO -ov -o "$output"
codesign --sign "$OX_CODESIGN_IDENTITY" --keychain "$OX_CODESIGN_KEYCHAIN" --timestamp "$output"
ox_notarize "$output" "$work"
xcrun stapler staple "$output"
xcrun stapler validate "$output"
codesign --verify -R="$requirement" "$output"

# Verify a copy as dragged into Applications, including the app's own ticket.
attach -readonly "$output"
check_layout
copied="$work/OX Desktop.app"
ditto "$mount/OX Desktop.app" "$copied"
codesign --verify --strict --deep -R="$requirement" "$copied"
xcrun stapler validate "$copied"
spctl --assess --type execute --verbose=2 "$copied"
name=$(basename "$output")
(cd "$(dirname "$output")" && shasum -a 256 "$name" > "$name.sha256")
