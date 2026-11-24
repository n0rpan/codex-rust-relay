#!/bin/sh
# Build a matching desktop, session host and relay on the host that will run them.
set -eu
cd "$(dirname "$0")/.."
root=$(pwd)
out="$root/desktop/dist"
build="${CARGO_TARGET_DIR:-$root/desktop/target}"
export CARGO_TARGET_DIR="$build"
cargo build --release --locked --manifest-path desktop/Cargo.toml -p ox-desktop -p ox-host
cargo build --release --locked --bin ox
mkdir -p "$out"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
cli_version=$("$build/release/ox" --version)
version=${cli_version#ox }
[ "$cli_version" = "ox $version" ] && [ "$("$build/release/ox-desktop" --version)" = "ox-desktop $version" ] || {
    echo 'Cannot package mismatched desktop and CLI versions.' >&2
    exit 1
}
[ "$("$build/release/ox-host" --version)" = "ox-host $version" ] || { echo "Cannot package a mismatched session host." >&2; exit 1; }
arch=$(uname -m)
case "$(uname -s)" in
    Darwin)
        bundle="$stage/OX Desktop.app"
        mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
        iconset="$stage/OX.iconset"
        mkdir "$iconset"
        for size in 16 32 128 256 512; do
            for scale in 1 2; do
                suffix=''
                if [ "$scale" -eq 2 ]; then suffix='@2x'; fi
                pixels=$((size * scale))
                sips -s format png -z "$pixels" "$pixels" desktop/packaging/ox-desktop.png \
                    --out "$iconset/icon_${size}x${size}$suffix.png" >/dev/null
            done
        done
        iconutil -c icns "$iconset" -o "$bundle/Contents/Resources/OX.icns"
        cp desktop/packaging/Info.plist "$bundle/Contents/"
        cp "$build/release/ox-desktop" "$build/release/ox-host" "$build/release/ox" "$bundle/Contents/MacOS/"
        plutil -replace CFBundleShortVersionString -string "${version%%-*}" "$bundle/Contents/Info.plist"
        plutil -replace CFBundleVersion -string "${GITHUB_RUN_NUMBER:-1}" "$bundle/Contents/Info.plist"
        plutil -lint "$bundle/Contents/Info.plist"
        codesign --force --sign - "$bundle/Contents/MacOS/ox"
        codesign --force --sign - "$bundle/Contents/MacOS/ox-host"
        codesign --force --sign - "$bundle"
        codesign --verify --strict --deep "$bundle"
        ditto -c -k --keepParent "$bundle" "$out/ox-desktop-macos-$arch.zip"
        ;;
    Linux)
        mkdir "$stage/ox-desktop"
        cp "$build/release/ox-desktop" "$build/release/ox-host" "$build/release/ox" "$stage/ox-desktop/"
        cp desktop/packaging/dev.ox.desktop.desktop desktop/packaging/ox-desktop.png "$stage/ox-desktop/"
        (cd "$stage/ox-desktop" && ldd ./ox-desktop && ldd ./ox-host && ldd ./ox) > "$stage/linked"
        if grep -q 'not found' "$stage/linked"; then cat "$stage/linked" >&2; exit 1; fi
        awk '/=>/ {print $1}' "$stage/linked" | LC_ALL=C sort -u > "$stage/ox-desktop/shared-libraries.txt"
        tar -czf "$out/ox-desktop-linux-$arch.tar.gz" -C "$stage" ox-desktop
        ;;
    *) echo 'OX Desktop packages support macOS and Linux.' >&2; exit 1 ;;
esac
case "$(uname -s)" in
    Darwin) file="ox-desktop-macos-$arch.zip"; (cd "$out" && shasum -a 256 "$file" > "$file.sha256") ;;
    Linux) file="ox-desktop-linux-$arch.tar.gz"; (cd "$out" && sha256sum "$file" > "$file.sha256") ;;
esac
