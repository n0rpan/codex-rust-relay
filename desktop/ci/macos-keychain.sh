#!/bin/sh
# Import the Developer ID identity into a throwaway keychain on a CI runner
# and export OX_CODESIGN_IDENTITY, OX_CODESIGN_KEYCHAIN and OX_SIGNING_TEAM to
# the following steps. Reads the apple-release environment secrets:
#   APPLE_DEVELOPER_ID_P12_BASE64, APPLE_DEVELOPER_ID_P12_PASSWORD
#   APPLE_TEAM_ID (optional): fail unless the certificate belongs to this team
set -eu
for name in APPLE_DEVELOPER_ID_P12_BASE64 APPLE_DEVELOPER_ID_P12_PASSWORD; do
    eval "value=\${$name:-}"
    if [ -z "$value" ]; then
        echo "::error::$name is empty. Store it as a secret of the apple-release environment." >&2
        exit 1
    fi
done
keychain="$RUNNER_TEMP/ox-signing.keychain-db"
password=$(openssl rand -hex 24)
p12="$RUNNER_TEMP/developer-id.p12"
trap 'rm -f "$p12"' EXIT
printf '%s' "$APPLE_DEVELOPER_ID_P12_BASE64" | base64 --decode > "$p12"
security create-keychain -p "$password" "$keychain"
security set-keychain-settings -lut 3600 "$keychain"
security unlock-keychain -p "$password" "$keychain"
security import "$p12" -k "$keychain" -P "$APPLE_DEVELOPER_ID_P12_PASSWORD" -f pkcs12 -T /usr/bin/codesign
# An exported .p12 usually holds only the leaf. codesign needs Apple's
# intermediate to build the chain, and a runner image may not ship it.
for authority in DeveloperIDG2CA DeveloperIDCA; do
    if curl -fsSL --retry 2 --max-time 30 -o "$RUNNER_TEMP/$authority.cer" \
        "https://www.apple.com/certificateauthority/$authority.cer"; then
        security import "$RUNNER_TEMP/$authority.cer" -k "$keychain" >/dev/null 2>&1 || true
    else
        echo "::warning::Could not download Apple's $authority intermediate certificate."
    fi
done
# Let codesign use the key without a prompt.
security set-key-partition-list -S apple-tool:,apple: -s -k "$password" "$keychain" >/dev/null
existing=$(security list-keychains -d user | sed 's/^ *"//; s/"$//')
# shellcheck disable=SC2086 # one keychain path per word
security list-keychains -d user -s "$keychain" $existing
found=$(security find-identity -v -p codesigning "$keychain" | grep 'Developer ID Application' | head -n 1 || true)
if [ -z "$found" ]; then
    echo "::error::The .p12 holds no Developer ID Application identity with its private key." >&2
    security find-identity -v "$keychain" >&2 || true
    exit 1
fi
identity=$(printf '%s\n' "$found" | awk '{print $2}')
team=$(printf '%s\n' "$found" | sed -n 's/.*(\([A-Z0-9]\{10\}\))".*/\1/p')
if [ -z "$team" ]; then echo "::error::Could not read the team from the signing identity." >&2; exit 1; fi
if [ -n "${APPLE_TEAM_ID:-}" ] && [ "$team" != "$APPLE_TEAM_ID" ]; then
    echo "::error::The certificate belongs to team $team, not APPLE_TEAM_ID." >&2
    exit 1
fi
echo "Signing as team $team"
{
    echo "OX_CODESIGN_IDENTITY=$identity"
    echo "OX_CODESIGN_KEYCHAIN=$keychain"
    echo "OX_SIGNING_TEAM=$team"
} >> "$GITHUB_ENV"
