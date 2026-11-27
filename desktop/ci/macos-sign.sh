#!/bin/sh
# Sign "OX Desktop.app" from the inside out with the hardened runtime.
#
# Usage: desktop/ci/macos-sign.sh BUNDLE
#   OX_CODESIGN_IDENTITY  identity to sign with; unset or "-" signs ad hoc
#   OX_CODESIGN_KEYCHAIN  keychain that holds the identity (optional)
#
# Local, pull request and nightly packages are ad hoc. The release run signs
# the same bundle again with Developer ID (desktop.yml, job sign-macos), so
# both go through these flags and the tested runtime matches the shipped one.
set -eu
bundle=${1:?pass the application bundle}
packaging=$(cd "$(dirname "$0")/../packaging" && pwd)
identity=${OX_CODESIGN_IDENTITY:--}
set -- --force --options runtime --sign "$identity"
# A secure timestamp keeps the signature valid after the certificate expires.
if [ "$identity" != - ]; then set -- "$@" --timestamp; fi
if [ -n "${OX_CODESIGN_KEYCHAIN:-}" ]; then set -- "$@" --keychain "$OX_CODESIGN_KEYCHAIN"; fi
# Explicit identifiers: with Developer ID the designated requirement is the
# identifier plus the team, which is what Keychain and privacy grants follow
# across updates.
codesign "$@" --identifier dev.ox.cli "$bundle/Contents/MacOS/ox"
codesign "$@" --identifier dev.ox.host "$bundle/Contents/MacOS/ox-host"
codesign "$@" --entitlements "$packaging/entitlements.plist" "$bundle"
codesign --verify --strict --deep "$bundle"
