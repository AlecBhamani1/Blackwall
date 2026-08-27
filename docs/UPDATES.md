# App updates

Blackwall uses Tauri's signed updater and a continuous GitHub Release named `main`. The installed
desktop app checks `https://github.com/AlecBhamani1/Blackwall/releases/download/main/latest.json`
at startup and exposes a manual check under **Settings → App updates**. An available update is only
downloaded after the user selects **Install update and restart**.

## Publishing from main

Every push to `main` runs `.github/workflows/release-main.yml`. The workflow assigns an increasing
SemVer using the GitHub run number, builds both Apple Silicon and Intel macOS bundles, signs the
updater artifacts, and publishes them to the `main` release with `latest.json`.

The repository must have the `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` Actions secrets. The matching public key is embedded in
`src/app/tauri.conf.json`. The current private key is stored only on the maintainer's machine at
`~/.tauri/blackwall.key`, with its password in macOS Keychain under
`com.blackwall.updater-signing`; keep an encrypted backup. Losing either means existing installs
cannot trust updates signed with a replacement key.

Updater signing verifies Blackwall's artifacts, but public macOS distribution should additionally
use Apple Developer ID signing and notarization before treating the channel as a public release.

## First installation

Existing builds that predate the updater need one final manual installation of an updater-enabled
release. After that, later `main` builds can be installed from inside Blackwall.
