#!/bin/sh
# Install ox, the local model gateway for stock Codex and Claude Code.
#
#   curl -fsSL https://raw.githubusercontent.com/oxstack/ox/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/oxstack/ox/main/install.sh | sh -s -- --dev
#
# Options (flags win over environment variables):
#   --stable                 Install the latest stable release (default)
#   --dev                    Install the rolling development pre-release
#   --desktop                Include OX Desktop and its matching CLI
#   OX_CHANNEL=dev|stable    Same as the flags
#   OX_INSTALL_DIR=DIR       Install directory (default: $HOME/.local/bin)
#   OX_INSTALL_BASE_URL=URL  Download ox-<target>.tar.gz from URL instead of GitHub
#   OX_INSTALL_SKIP_GLIBC_CHECK=1  Skip the Linux glibc version check
#
# The script is wrapped in main() and called on the last line, so a download
# that is cut off part-way never runs a half-received script.

set -eu

REPOSITORY="oxstack/ox"
MIN_GLIBC_MINOR=39

bold=""
dim=""
red=""
green=""
yellow=""
reset=""
work=""
desktop_stage=""

setup_colours() {
    if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-dumb}" != "dumb" ]; then
        bold="$(printf '\033[1m')"
        dim="$(printf '\033[2m')"
        red="$(printf '\033[31m')"
        green="$(printf '\033[32m')"
        yellow="$(printf '\033[33m')"
        reset="$(printf '\033[0m')"
    fi
}

info() {
    printf '%s\n' "$*"
}

step() {
    printf '%s==>%s %s\n' "$bold" "$reset" "$*"
}

warn() {
    printf '%swarning:%s %s\n' "$yellow" "$reset" "$*" >&2
}

die() {
    printf '%serror:%s %s\n' "$red" "$reset" "$*" >&2
    exit 1
}

# macOS: an installation signed with OX's Developer ID only accepts code signed
# with it, so a replaced download and checksum cannot install anything else.
# The installed CLI decides: desktop, host and CLI installations all have one,
# and local, pull request and nightly builds are ad hoc and stay unrestricted.
release_identity='anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists and certificate leaf[field.1.2.840.113635.100.6.1.13] exists and certificate leaf[subject.OU] = "ZN9XKLHU6J"'
signed_release() {
    codesign --verify --strict -R="$release_identity" "$1" 2>/dev/null
}
require_release_identity() {
    [ "$(uname -s)" = Darwin ] || return 0
    if [ -e "$install_dir/ox" ] && signed_release "$install_dir/ox"; then
        signed_release "$1" || die "this download is not signed by the OX release identity, unlike the installed ox."
    fi
}

cleanup() {
    if [ -n "$desktop_stage" ]; then rm -rf "$desktop_stage"; fi
    if [ -n "$work" ]; then
        rm -rf "$work"
    fi
}

usage() {
    cat <<'EOF'
Install ox on macOS or Linux.

Usage: install.sh [--stable|--dev] [--desktop|--host]

  --host     Install the headless session host and CLI, with automatic idle updates
  --desktop  Include the native desktop application, session host and matching CLI
  --stable   Install the latest stable release (default)
  --dev      Install the rolling development pre-release

Environment:
  OX_CHANNEL=stable|dev    Same as the flags
  OX_INSTALL_DIR=DIR       Install directory (default: $HOME/.local/bin)
  OX_INSTALL_BASE_URL=URL  Download ox-<target>.tar.gz from URL instead of GitHub
EOF
}

source_hint() {
    info "Build from source instead:" >&2
    info "  cargo install --locked --git https://github.com/${REPOSITORY}" >&2
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Linux) os_part="unknown-linux-gnu" ;;
        Darwin) os_part="apple-darwin" ;;
        *) die "ox supports macOS and Linux; this system is ${os}." ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch_part="x86_64" ;;
        aarch64 | arm64) arch_part="aarch64" ;;
        *) die "ox supports x86-64 and ARM64; this machine is ${arch}." ;;
    esac
    # A Rosetta shell on Apple silicon reports x86_64. Install the native build.
    if [ "$os" = "Darwin" ] && [ "$arch_part" = "x86_64" ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = "1" ]; then
        arch_part="aarch64"
    fi
    target="${arch_part}-${os_part}"
}

check_glibc() {
    case "$target" in
        *-linux-gnu) ;;
        *) return 0 ;;
    esac
    if [ -n "${OX_INSTALL_SKIP_GLIBC_CHECK:-}" ]; then
        return 0
    fi
    found="$(getconf GNU_LIBC_VERSION 2>/dev/null || true)"
    case "$found" in
        "glibc "*) found="${found#glibc }" ;;
        *)
            if ldd --version 2>&1 | grep -qi musl; then
                printf '%serror:%s %s\n' "$red" "$reset" \
                    "the ox release binaries need glibc 2.${MIN_GLIBC_MINOR} or newer; this system uses musl." >&2
                source_hint
                exit 1
            fi
            warn "could not detect the glibc version; ox needs glibc 2.${MIN_GLIBC_MINOR} or newer."
            return 0
            ;;
    esac
    major="${found%%.*}"
    rest="${found#*.}"
    minor="${rest%%.*}"
    case "${major}${minor}" in
        "" | *[!0-9]*)
            warn "could not read the glibc version '${found}'; ox needs glibc 2.${MIN_GLIBC_MINOR} or newer."
            return 0
            ;;
    esac
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt "$MIN_GLIBC_MINOR" ]; }; then
        printf '%serror:%s %s\n' "$red" "$reset" \
            "the ox release binaries need glibc 2.${MIN_GLIBC_MINOR} or newer; this system has glibc ${found}." >&2
        source_hint
        exit 1
    fi
}

pick_tools() {
    if command -v curl >/dev/null 2>&1; then
        fetcher="curl"
    elif command -v wget >/dev/null 2>&1; then
        fetcher="wget"
    else
        die "install curl or wget, then run this installer again."
    fi
    if command -v sha256sum >/dev/null 2>&1; then
        hasher="sha256sum"
    elif command -v shasum >/dev/null 2>&1; then
        hasher="shasum"
    else
        die "install sha256sum or shasum, then run this installer again."
    fi
    command -v tar >/dev/null 2>&1 || die "install tar, then run this installer again."
}

# fetch URL FILE: returns 0 on success, 44 when the server says the file does
# not exist, and 1 for every other failure.
fetch() {
    case "$1" in
        file:///*)
            local_file=${1#file://}
            [ -f "$local_file" ] || return 1
            cp "$local_file" "$2" || return 1
            return 0
            ;;
    esac
    status=0
    if [ "$fetcher" = "curl" ]; then
        curl --fail --silent --show-error --location --retry 2 \
            --connect-timeout 15 --output "$2" "$1" 2>"$work/fetch.err" || status=$?
        # curl exits 22 for an HTTP status of 400 or above.
        if [ "$status" -eq 22 ]; then
            return 44
        fi
    else
        wget --quiet --tries=3 --timeout=15 --output-document="$2" "$1" 2>"$work/fetch.err" || status=$?
        # wget exits 8 when the server answers with an error status.
        if [ "$status" -eq 8 ]; then
            return 44
        fi
    fi
    if [ "$status" -ne 0 ]; then
        return 1
    fi
    return 0
}

# download BASE: fetch the archive and its checksum from BASE. Same return
# codes as fetch.
download() {
    fetch "$1/${asset}" "$work/${asset}" || return $?
    fetch "$1/${asset}.sha256" "$work/${asset}.sha256" || return $?
    return 0
}

file_sha256() {
    if [ "$hasher" = "sha256sum" ]; then
        sha256sum "$1" | cut -c 1-64
    else
        shasum -a 256 "$1" | cut -c 1-64
    fi
}

path_hint() {
    case ":${PATH}:" in
        *":${install_dir}:"*) return 0 ;;
    esac
    info ""
    info "${yellow}${install_dir} is not on your PATH.${reset} Add it with:"
    info ""
    case "$(basename "${SHELL:-sh}")" in
        zsh)
            info "  echo 'export PATH=\"${install_dir}:\$PATH\"' >> ~/.zshrc"
            info "  source ~/.zshrc"
            ;;
        bash)
            if [ "$(uname -s)" = "Darwin" ]; then
                profile="$HOME/.bash_profile"
            else
                profile="$HOME/.bashrc"
            fi
            info "  echo 'export PATH=\"${install_dir}:\$PATH\"' >> ${profile}"
            info "  source ${profile}"
            ;;
        fish)
            info "  fish_add_path ${install_dir}"
            ;;
        *)
            info "  echo 'export PATH=\"${install_dir}:\$PATH\"' >> ~/.profile"
            info "  . ~/.profile"
            ;;
    esac
}

# Keep the GUI, session host and gateway together in one immutable installation.
# OX Desktop links the system's X11, Wayland, Vulkan and audio libraries. A
# desktop install has them; a minimal or server install may not.
die_missing_libraries() {
    missing=""
    if command -v ldd >/dev/null 2>&1; then
        missing="$(ldd "$1" 2>/dev/null | awk '/not found/ {print $1}' | tr '\n' ' ')"
    fi
    if [ -n "$missing" ]; then
        die "OX Desktop needs system libraries that are not installed: ${missing}
  On Ubuntu or Debian: sudo apt install libxkbcommon-x11-0 libxkbcommon0 libwayland-client0 libvulkan1 mesa-vulkan-drivers libasound2t64 libfontconfig1
  Then run the installer again. The terminal-only CLI (without --desktop) needs none of these."
    fi
    die "the packaged OX Desktop could not start."
}

prepare_desktop() {
    packages="$install_dir/.ox-desktop"
    mkdir -p "$packages"
    case "$os" in
        Darwin) binary_subdir="OX Desktop.app/Contents/MacOS" ;;
        *) binary_subdir=ox-desktop ;;
    esac
    for existing in "$packages/$actual".*; do
        if [ -f "$existing/.ready" ] && [ -x "$existing/$binary_subdir/ox-host" ] && [ -x "$existing/$binary_subdir/ox-desktop" ]; then
            require_release_identity "$existing/OX Desktop.app"
            desktop_binary_dir="$existing/$binary_subdir"
            if [ "$host_only" = 0 ]; then "$desktop_binary_dir/ox-desktop" --version >/dev/null || die_missing_libraries "$desktop_binary_dir/ox-desktop"; fi
            return
        fi
    done
    desktop_stage="$(mktemp -d "$packages/$actual.XXXXXX")"
    if [ "$os" = Darwin ]; then
        ditto -x -k "$work/$asset" "$desktop_stage"
        codesign --verify --strict --deep "$desktop_stage/OX Desktop.app"
        require_release_identity "$desktop_stage/OX Desktop.app"
    else
        tar -xzf "$work/$asset" -C "$desktop_stage"
    fi
    desktop_binary_dir="$desktop_stage/$binary_subdir"
    helper_version="$("$desktop_binary_dir/ox" --version)" || die "the packaged ox could not start."
    if [ "$host_only" = 0 ]; then
        desktop_version="$("$desktop_binary_dir/ox-desktop" --version)" || die_missing_libraries "$desktop_binary_dir/ox-desktop"
        [ "${desktop_version#ox-desktop }" = "${helper_version#ox }" ] || die "the packaged desktop and CLI versions differ."
    fi
    [ "$("$desktop_binary_dir/ox-host" --version)" = "ox-host ${helper_version#ox }" ] || die "the packaged host and CLI versions differ."
    touch "$desktop_stage/.in-use"
    touch "$desktop_stage/.ready"
    desktop_stage=""
}

replace_link() {
    # Rename over the link itself, including Finder's link to a directory.
    if [ -d "$2" ] && [ ! -L "$2" ]; then
        die "Cannot replace the existing directory at $2."
    fi
    ln -s "$1" "$2.ox-link.$$"
    if [ "$os" = Darwin ]; then mv -fh "$2.ox-link.$$" "$2"; else mv -fT "$2.ox-link.$$" "$2"; fi
}

# A running session host keeps the old binary. Let its running turns finish
# (bounded), then start the new one, so an update never cuts a turn short.
restart_host() {
    if [ -x "$install_dir/ox-host" ] &&
        "$install_dir/ox-host" status 2>/dev/null | grep -q '^OX host: running'; then
        step "Restarting the OX session host on the new version (running turns finish first)"
        "$install_dir/ox" host restart ||
            warn "could not restart the session host. Run: ox host restart"
    fi
}

install_desktop_entry() {
    # Every public entry resolves through this single package pointer. Future
    # updates can switch the app, host, CLI and resources with one rename.
    generation=${desktop_binary_dir%/"$binary_subdir"}
    if "$desktop_binary_dir/ox" update --help | grep -q -- '--stage '; then
        "$desktop_binary_dir/ox" update --select-package
        desktop_binary_dir="$packages/current/$binary_subdir"
        replace_link "$desktop_binary_dir/ox" "$install_dir/ox"
    else
        # A legacy CLI updates itself in place. Keep its public copy outside
        # the immutable package (especially the signed macOS app).
        replace_link "$generation" "$packages/current"
        desktop_binary_dir="$packages/current/$binary_subdir"
        cp "$desktop_binary_dir/ox" "$install_dir/.ox-install.$$"
        mv -f "$install_dir/.ox-install.$$" "$install_dir/ox"
    fi
    if [ -x "$desktop_binary_dir/ox-host" ]; then replace_link "$desktop_binary_dir/ox-host" "$install_dir/ox-host"; fi
    if [ "$host_only" = 1 ]; then
        touch "$packages/headless"
        info "Installed OX session host at $install_dir/ox-host"
        return
    fi
    rm -f "$packages/headless"
    replace_link "$desktop_binary_dir/ox-desktop" "$install_dir/ox-desktop"
    if [ "$os" = Darwin ]; then
        bundle="${desktop_binary_dir%/Contents/MacOS}"
        mkdir -p "$HOME/Applications"
        if [ -d "$HOME/Applications/OX Desktop.app" ] && [ ! -L "$HOME/Applications/OX Desktop.app" ]; then
            info "Kept the existing Finder bundle. The new bundle is at $bundle."
        else
            replace_link "$bundle" "$HOME/Applications/OX Desktop.app"
        fi
    else
        data="${XDG_DATA_HOME:-$HOME/.local/share}"
        icon_dir="$data/icons/hicolor/1024x1024/apps"
        mkdir -p "$data/applications" "$icon_dir"
        replace_link "$desktop_binary_dir/ox-desktop.png" "$icon_dir/dev.ox.desktop.png"
        # Desktop-entry quoting has two layers. Escape field codes and special
        # characters so graphical launches also work with custom install paths.
        OX_GUI_EXEC="$install_dir/ox-desktop" awk '
            /^Exec=/ {
                path = ENVIRON["OX_GUI_EXEC"]
                printf "Exec=\""
                for (i=1; i<=length(path); ++i) {
                    ch=substr(path,i,1)
                    if (ch=="%") printf "%%%%"
                    else if (ch=="\\") { for (j=0; j<4; ++j) printf "%c", 92 }
                    else {
                        if (ch=="\"" || ch=="$" || ch=="`") printf "\\\\"
                        printf "%s", ch
                    }
                }
                print "\""
                next
            }
            { print }
        ' "$desktop_binary_dir/dev.ox.desktop.desktop" > "$data/applications/dev.ox.desktop.desktop.tmp.$$"
        mv -f "$data/applications/dev.ox.desktop.desktop.tmp.$$" "$data/applications/dev.ox.desktop.desktop"
    fi
    info "Installed OX Desktop at $install_dir/ox-desktop"
}

main() {
    setup_colours
    desktop=0
    host_only=0
    explicit_kind=0
    channel="${OX_CHANNEL:-}"
    for argument in "$@"; do
        case "$argument" in
            --desktop) desktop=1; explicit_kind=1; host_only=0 ;;
            --host) desktop=1; explicit_kind=1; host_only=1 ;;
            --dev) channel="dev" ;;
            --stable) channel="stable" ;;
            -h | --help)
                usage
                exit 0
                ;;
            *)
                usage >&2
                die "unknown option: ${argument}"
                ;;
        esac
    done
    explicit_channel="$channel"
    case "$channel" in
        "") channel="stable" ;;
        stable | dev) ;;
        *) die "OX_CHANNEL must be stable or dev, not '${channel}'." ;;
    esac

    [ -n "${HOME:-}" ] || [ -n "${OX_INSTALL_DIR:-}" ] || die "set HOME or OX_INSTALL_DIR."
    install_dir="${OX_INSTALL_DIR:-$HOME/.local/bin}"
    case "$install_dir" in
        /*) ;;
        *) die "OX_INSTALL_DIR must be an absolute path, not '${install_dir}'." ;;
    esac

    if [ -L "$install_dir/.ox-desktop/current" ]; then desktop=1; fi
    if [ "$explicit_kind" = 0 ] && [ -f "$install_dir/.ox-desktop/headless" ]; then host_only=1; fi
    detect_target
    check_glibc
    pick_tools
    asset="ox-${target}.tar.gz"
    if [ "$desktop" = 1 ]; then
        case "$target" in
            x86_64-*-linux-gnu) asset=ox-desktop-linux-x86_64.tar.gz ;;
            aarch64-*-linux-gnu) asset=ox-desktop-linux-aarch64.tar.gz ;;
            x86_64-apple-darwin) asset=ox-desktop-macos-x86_64.zip ;;
            aarch64-apple-darwin) asset=ox-desktop-macos-arm64.zip ;;
        esac
    fi

    work="$(mktemp -d "${TMPDIR:-/tmp}/ox-install.XXXXXX")" || die "could not create a temporary directory."
    trap cleanup EXIT
    trap 'exit 130' INT
    trap 'exit 143' TERM

    stable_base="https://github.com/${REPOSITORY}/releases/latest/download"
    dev_base="https://github.com/${REPOSITORY}/releases/download/dev"
    result=0
    if [ -n "${OX_INSTALL_BASE_URL:-}" ]; then
        base="${OX_INSTALL_BASE_URL%/}"
        source_name="$base"
        step "Downloading ox for ${target} from ${base}"
        download "$base" || result=$?
    elif [ "$channel" = "dev" ]; then
        source_name="the dev pre-release"
        step "Downloading the ox dev build for ${target}"
        download "$dev_base" || result=$?
    else
        source_name="the latest stable release"
        step "Downloading ox for ${target}"
        download "$stable_base" || result=$?
        if [ "$result" -eq 44 ] && [ -z "$explicit_channel" ]; then
            info "${yellow}No stable ox release is published yet. Installing the dev build instead.${reset}"
            channel="dev"
            source_name="the dev pre-release"
            result=0
            download "$dev_base" || result=$?
        fi
    fi
    if [ "$result" -eq 44 ]; then
        if [ -z "${OX_INSTALL_BASE_URL:-}" ] && [ "$channel" = "stable" ]; then
            die "no stable ox release for ${target} is published yet. Run this installer again with --dev."
        fi
        die "${source_name} has no ox build for ${target}. See https://github.com/${REPOSITORY}/releases"
    elif [ "$result" -ne 0 ]; then
        if [ -s "$work/fetch.err" ]; then
            cat "$work/fetch.err" >&2
        fi
        die "could not download ox from ${source_name}. Check your connection and try again."
    fi

    step "Verifying the SHA-256 checksum"
    expected="$(sed -n '1s/^\([0-9a-fA-F]\{64\}\).*$/\1/p' "$work/${asset}.sha256" | tr 'A-F' 'a-f')"
    [ -n "$expected" ] || die "the published checksum for ${asset} is not a SHA-256 digest."
    actual="$(file_sha256 "$work/${asset}")"
    if [ "$expected" != "$actual" ]; then
        die "checksum mismatch for ${asset} (expected ${expected}, got ${actual}). Nothing was installed."
    fi

    if [ "$desktop" = 1 ]; then
        prepare_desktop
        install_desktop_entry
    else
        mkdir "$work/unpacked"
        tar -xzf "$work/${asset}" -C "$work/unpacked" || die "could not unpack ${asset}."
        source_binary="$work/unpacked/ox"
        [ -f "$source_binary" ] || die "${asset} does not contain the ox binary."
        chmod 755 "$source_binary"
        require_release_identity "$source_binary"
        step "Installing to ${install_dir}/ox"
        mkdir -p "$install_dir" || die "could not create ${install_dir}."
        if [ -d "${install_dir}/ox" ]; then
            die "${install_dir}/ox is a directory. Remove it or set OX_INSTALL_DIR."
        fi
        # Stage inside the destination directory so the final rename is atomic and
        # a running ox keeps its old file until it exits.
        staged="${install_dir}/.ox-install.$$"
        cp "$source_binary" "$staged" || die "could not write to ${install_dir}. Set OX_INSTALL_DIR to a directory you own."
        chmod 755 "$staged"
        if ! mv -f "$staged" "${install_dir}/ox"; then
            rm -f "$staged"
            die "could not replace ${install_dir}/ox."
        fi
        mkdir -p "$install_dir/.ox-cli-update"
        if [ "$(uname -s)" = "Darwin" ] && command -v xattr >/dev/null 2>&1; then
            xattr -d com.apple.quarantine "${install_dir}/ox" 2>/dev/null || true
        fi
    fi

    if ! installed="$("${install_dir}/ox" --version 2>&1)"; then
        printf '%serror:%s %s\n' "$red" "$reset" \
            "${install_dir}/ox was installed but does not run on this system:" >&2
        info "  ${installed}" >&2
        source_hint
        exit 1
    fi

    info ""
    info "${green}Installed ${installed}${reset} ${dim}(${install_dir}/ox)${reset}"
    if [ "$desktop" = 1 ]; then
        restart_host
    fi
    path_hint
    info ""
    info "Next step:"
    info ""
    if [ "$host_only" = 1 ]; then info "  ${bold}ox host install${reset}"; info "  ${bold}ox host pair${reset}";
    elif [ "$desktop" = 1 ]; then info "  ${bold}ox desktop .${reset}"; else info "  ${bold}ox setup${reset}"; fi
    info ""
    if [ "$channel" = "dev" ] && [ -z "${OX_INSTALL_BASE_URL:-}" ]; then
        if [ "$desktop" = 1 ]; then
            info "${dim}OX Desktop downloads updates in the background. You can also run ox update.${reset}"
        else
            info "${dim}This is a development build. ox update keeps it on the dev channel.${reset}"
        fi
    fi
}

main "$@"
