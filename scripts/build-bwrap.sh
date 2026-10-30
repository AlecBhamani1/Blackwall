#!/usr/bin/env bash
# Build the bubblewrap (bwrap) release that the Linux desktop packages bundle for sandboxed commands.
# Usage: scripts/build-bwrap.sh <out-dir>
# Writes <out-dir>/bwrap (not setuid, no SELinux) and <out-dir>/COPYING (its LGPL-2.0-or-later text).
# The SHA-256 is that of the v0.13.1 release tarball, matching the hash in the release notes and
# the PGP-signed v0.13.1 tag's tree.
set -euo pipefail

BWRAP_SHA256='0e85c8702248b8dd64144f594147553650cdd0e4c951690345faace79c217c4d'
BWRAP_URL='https://github.com/containers/bubblewrap/releases/download/v0.13.1/bubblewrap-0.13.1.tar.xz'

if [ "$#" -ne 1 ]; then
  printf '%s\n' 'Usage: scripts/build-bwrap.sh <out-dir>' >&2
  exit 2
fi
mkdir -p "$1"
out_dir=$(cd "$1" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
  --output "$work/bubblewrap.tar.xz" "$BWRAP_URL"
printf '%s  %s\n' "$BWRAP_SHA256" "$work/bubblewrap.tar.xz" | sha256sum --check --strict
tar -xJf "$work/bubblewrap.tar.xz" -C "$work"

src="$work/bubblewrap-0.13.1"
meson setup "$work/build" "$src" --buildtype=release \
  -Dselinux=disabled -Dman=disabled -Dtests=false -Dbash_completion=disabled -Dzsh_completion=disabled
meson compile -C "$work/build" bwrap

bwrap="$work/build/bwrap"
if test -u "$bwrap"; then
  printf '%s\n' 'The bubblewrap build is setuid; refusing to bundle it.' >&2
  exit 1
fi
install -m 0755 "$bwrap" "$out_dir/bwrap"
install -m 0644 "$src/COPYING" "$out_dir/COPYING"
"$out_dir/bwrap" --version
