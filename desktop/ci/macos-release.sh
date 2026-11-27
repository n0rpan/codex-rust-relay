# Sourced by the macOS release signing scripts.
#   OX_SIGNING_TEAM: from macos-keychain.sh
#   APPLE_API_KEY_P8, APPLE_API_KEY_ID, APPLE_API_ISSUER_ID: App Store Connect API key

ox_require() {
    for variable in "$@"; do
        eval "value=\${$variable:-}"
        if [ -z "$value" ]; then
            echo "::error::$variable is empty. Store it as a secret of the apple-release environment." >&2
            exit 1
        fi
    done
}

# Developer ID issued by Apple to this team.
ox_requirement() {
    printf '%s' "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = \"$OX_SIGNING_TEAM\""
}

# ox_notarize ZIP WORK_DIR: submit to Apple, wait, and fail unless accepted.
ox_notarize() {
    ox_require APPLE_API_KEY_P8 APPLE_API_KEY_ID APPLE_API_ISSUER_ID
    key="$2/AuthKey.p8"
    (umask 077 && printf '%s\n' "$APPLE_API_KEY_P8" > "$key")
    result="$2/notary.json"
    status=0
    xcrun notarytool submit "$1" --key "$key" --key-id "$APPLE_API_KEY_ID" \
        --issuer "$APPLE_API_ISSUER_ID" --wait --timeout 30m --output-format json > "$result" || status=$?
    cat "$result"; echo
    id=$(jq -r '.id // empty' "$result" 2>/dev/null || true)
    verdict=$(jq -r '.status // empty' "$result" 2>/dev/null || true)
    if [ "$status" -ne 0 ] || [ "$verdict" != Accepted ]; then
        if [ -n "$id" ]; then
            xcrun notarytool log "$id" --key "$key" --key-id "$APPLE_API_KEY_ID" \
                --issuer "$APPLE_API_ISSUER_ID" || true
        fi
        rm -f "$key"
        echo "::error::Apple did not notarize $(basename "$1") (status: ${verdict:-none})." >&2
        exit 1
    fi
    rm -f "$key"
}
