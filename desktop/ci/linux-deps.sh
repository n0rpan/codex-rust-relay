#!/bin/sh
set -eu
set -- clang cmake pkg-config libssl-dev libfontconfig-dev \
    libfreetype-dev libglib2.0-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-x11-dev \
    libxkbcommon-dev libvulkan-dev libasound2-dev mesa-vulkan-drivers xvfb xauth xdotool "$@"
sudo apt-get update -o Acquire::Retries=3
if [ "${OX_CACHE_PACKAGE_DEBS:-}" = 1 ]; then
    archives="${RUNNER_TEMP:?}/ox-package-debs"
    python3 "$(dirname "$0")/apt_archives.py" "$@"
    set -- -o "Dir::Cache::archives=$archives" -o APT::Keep-Downloaded-Packages=true "$@"
fi
sudo apt-get install -y -o Acquire::Retries=3 "$@"
