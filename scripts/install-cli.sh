#!/bin/sh
set -eu

REPOSITORY='https://github.com/AlecBhamani1/Blackwall.git'

if ! command -v cargo >/dev/null 2>&1; then
    printf '%s\n' 'Blackwall CLI installation requires Rust and Cargo.' >&2
    printf '%s\n' 'Install Rust from https://rustup.rs, then run this installer again.' >&2
    exit 1
fi

printf '%s\n' 'Installing Blackwall CLI (bw)...'
cargo install --git "$REPOSITORY" --branch partial --locked --force --package bw
printf '\n%s\n' 'Installed. Run `bw setup` to configure your model connection, then `bw` to chat.'
