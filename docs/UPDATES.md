# App updates

Each approved version has its own GitHub Release, such as `v0.1.3`, with version-specific downloads
and release notes. The [latest release](https://github.com/AlecBhamani1/Blackwall/releases/latest)
is the download destination. The old `main` release is retained as a compatibility feed for apps
that already check `https://github.com/AlecBhamani1/Blackwall/releases/download/main/latest.json`.
Do not delete that release or move its tag. Its feed points to the latest approved version's files.

The desktop app checks at startup and exposes a manual check under **Settings → App updates**.
An available update is downloaded only after the user selects **Install update and restart**.

## Publishing an approved version

Merging into `main` runs CI but does **not** publish a release. Publishing is an explicit maintainer
operation using the **Publish versioned release** workflow (`.github/workflows/release-main.yml`).

1. Complete the applicable [native and distribution acceptance checks](NATIVE_ACCEPTANCE.md).
   Updater signatures authenticate artifacts; Apple Developer ID signing/notarization, clean-machine
   installation, signed upgrades, and physical-device checks require their own acceptance evidence.
2. Choose a stable version newer than the live updater version, without a `v` prefix (for example
   `0.1.4`). Commit meaningful release notes in `docs/releases/0.1.4.md`, merge, and wait for CI on
   that exact main commit to pass. Version numbers compare numerically; `0.1.10` follows `0.1.9`.
3. Open **Actions → Publish versioned release → Run workflow**, select **main**, enter the version,
   and check the acceptance confirmation. This explicitly authorizes publication after validation.
4. The workflow creates a draft tied to the selected commit and builds Apple Silicon and Intel
   installers and signed updater archives. Both builds must finish before publication. An existing
   published version cannot be rebuilt, and an existing tag cannot be reassigned.
5. The final job verifies all six files against their GitHub SHA-256 digests, verifies both updater
   entries reference those archives and match their signature files, then publishes `v<version>`
   as Latest. It updates the old `main/latest.json` feed only after publishing the complete release.
6. Confirm both architecture downloads and the app's update check against the published build.

The workflow overrides the Tauri application version for the build. It no longer derives public
versions from Actions run numbers. The source commit and release notes are recorded on each release.
GitHub asset digest checks protect download integrity; matching the updater manifest to signature
files does not replace Tauri's cryptographic signature verification during installation.

## Failure and retry

A failed build leaves a draft and the existing updater feed untouched. Use **Re-run failed jobs**
to finish it. The two builds run sequentially because Tauri merges their updater entries into one
manifest. Release runs are also serialized so an older run cannot overwrite a newer update feed.

Publishing the release and replacing the compatibility feed are separate GitHub operations. If the
feed upload fails after publication, re-run the failed publish job. It validates the published
assets again and repairs the feed without rebuilding or overwriting the versioned release files.
A completed publication can also be retried safely when the feed already matches it. The workflow
refuses to downgrade the feed. Do not restart the entire workflow for an already-published version.

GitHub replaces an uploaded `latest.json` by deleting and recreating that single asset; there can
be a brief unavailable-feed interval. An interrupted replacement is repairable by rerunning the
publish job. Already-installed apps retain their installed version if an update check fails.

## Signing credentials

The repository must have the `TAURI_SIGNING_PRIVATE_KEY` and
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD` Actions secrets. The matching public key is embedded in
`src/app/tauri.conf.json`. The private key is stored only on the maintainer's machine at
`~/.tauri/blackwall.key`, with its password in macOS Keychain under
`com.blackwall.updater-signing`; keep an encrypted backup. Losing either means existing installs
cannot trust updates signed with a replacement key.

## Existing main-channel builds

Version 0.1.3 was originally published on September 24, 2026. Its versioned release reuses the
original application files and signatures. The compatibility release and its older assets remain
available so existing download links and installed apps continue to work.

Builds predating the updater need one manual installation of an updater-enabled release. Later
approved versions can then be installed from inside Blackwall.
