#!/bin/sh
set -eu
sudo apt-get update -o Acquire::Retries=3
sudo apt-get install -y -o Acquire::Retries=3 clang cmake pkg-config libssl-dev libfontconfig-dev \
    libfreetype-dev libglib2.0-dev libwayland-dev libx11-xcb-dev libxcb1-dev \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-x11-dev \
    libxkbcommon-dev libvulkan-dev libasound2-dev mesa-vulkan-drivers xvfb xauth xdotool "$@"
