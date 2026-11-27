# Sourced by the Linux release publisher after downloading this run's artifacts.
# macOS ships only the Developer ID signed and notarized archives (desktop.yml,
# job sign-macos), never the ad hoc ones the package job tested.
append_desktop_assets() {
    local entry directory archive
    for entry in \
        ubuntu-24.04:linux-x86_64.tar.gz \
        ubuntu-24.04-arm:linux-aarch64.tar.gz \
        signed-macos-26:macos-arm64.zip \
        signed-macos-26-intel:macos-x86_64.zip; do
        directory="dist/ox-desktop-${entry%%:*}"
        archive="ox-desktop-${entry#*:}"
        (cd "$directory" && sha256sum --check --strict "$archive.sha256")
        files+=("$directory/$archive" "$directory/$archive.sha256")
    done
}
