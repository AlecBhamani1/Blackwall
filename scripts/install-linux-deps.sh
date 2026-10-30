#!/usr/bin/env bash
# Install the Ubuntu/Debian libraries needed to build and bundle the Linux desktop app:
# WebKitGTK for Tauri, D-Bus for Secret Service credentials, AppImage tooling, and the tools that
# build the bundled bubblewrap (scripts/build-bwrap.sh).
set -euo pipefail
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  build-essential file libayatana-appindicator3-dev libdbus-1-dev librsvg2-dev \
  libssl-dev libwebkit2gtk-4.1-dev libxdo-dev patchelf xdg-utils meson ninja-build libcap-dev
