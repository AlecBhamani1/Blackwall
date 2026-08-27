# Blackwall

Blackwall is a local-first agent harness with a deliberately simple desktop chat: choose your
model, write a message, attach images or files, and receive a streamed response. The model can run
on this computer or on another machine you control through any OpenAI-compatible endpoint.

> **Current status:** the first working desktop vertical slice is implemented. Chat, remote model
> discovery, streaming, attachments, recent conversations, and the native macOS window work now.
> The first QR/link browser-sharing slice is intentionally narrow and documented in
> [`docs/SHARING.md`](docs/SHARING.md). Agent tools, durable memory, approvals, and broader sharing
> remain roadmap milestones in [`docs/PLAN.md`](docs/PLAN.md).

## What works now

- Native Tauri 2 app with a focused Svelte 5 chat interface
- Model discovery and switching against an OpenAI-compatible endpoint
- Streamed assistant replies with stop and reconnect controls
- Images, pasted photos, text files, and general file attachments
- Drag-and-drop, attachment previews, duplicate detection, and bounded upload limits
- Recent conversations saved on the device
- Sanitized Markdown responses and responsive desktop/mobile-width layouts
- Expiring QR and browser-link guest sharing over the owner's Tailscale interface
- Signed in-app updates from the latest successful `main` build on GitHub
- Typed Rust commands and one versionable `blackwall://event` stream contract

Blackwall does **not** launch or manage Ollama. If Ollama runs on a different machine, point
Blackwall at that machine and leave it running there.

The endpoint can be changed and tested inside the app: open **Settings**, enter the Ollama host or
OpenAI-compatible base URL under **Model connection**, then choose **Save and reconnect**. Blackwall
remembers this override for future launches. This is the recommended setup for a packaged macOS
app because apps opened from Finder do not reliably inherit shell environment variables.

## Run the desktop app

Prerequisites are Node.js 22.12+, npm, the stable Rust toolchain, and the
[Tauri 2 platform prerequisites](https://v2.tauri.app/start/prerequisites/).

```sh
npm install
npm --prefix ui install

export BLACKWALL_MODEL_ENDPOINT=http://your-model-host:11434
npm run desktop
```

An existing `OLLAMA_HOST` is used automatically when no in-app override has been saved, so the
export is unnecessary when that variable already points at the model machine. Both
`http://host:11434` and `http://host:11434/v1` are accepted. Set `BLACKWALL_MODEL_API_KEY` when the
endpoint expects a Bearer token.

For browser-only UI development:

```sh
export BLACKWALL_DEV_MODEL_ENDPOINT=http://your-model-host:11434
npm run dev
```

Then open `http://127.0.0.1:1420`. The browser development server proxies requests to the configured
model host; production desktop traffic goes through the Rust bridge.

## Verify the workspace

```sh
npm run verify
cargo fmt --manifest-path src/Cargo.toml --all -- --check
cargo clippy --manifest-path src/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path src/Cargo.toml --workspace --all-features
cargo deny --manifest-path src/Cargo.toml check
```

Release bundles must be signed for the updater. On the maintainer's Mac, use the private key and
its Keychain password before building:

```sh
export TAURI_SIGNING_PRIVATE_KEY="$HOME/.tauri/blackwall.key"
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="$(security find-generic-password -a "$USER" -s com.blackwall.updater-signing -w)"
npm run desktop:build
```

This signs the updater archive, not the macOS application itself. Public macOS distribution still
requires an Apple Developer signing identity and notarization.

## In-app updates

Blackwall checks for signed updates when the desktop app starts. Open **Settings → App updates** to
check manually, review the available version, and choose **Install update and restart**. Pushes to
`main` publish the continuous update channel through GitHub Actions. Existing installations need
one final manual replacement with an updater-enabled build; later updates install in place. See
[`docs/UPDATES.md`](docs/UPDATES.md) for signing, publishing, and key-recovery details.

## Architecture

```text
┌──────────────────────────────────────────────┐
│ ui/       Svelte 5 chat and attachment UI    │
├──────────────────────────────────────────────┤
│ typed Tauri commands + blackwall://event     │
├──────────────────────────────────────────────┤
│ src/app  endpoint bridge and desktop shell   │
│ src/core protocol and headless domain types  │
│ src/bw   future command-line adapter         │
└──────────────────────┬───────────────────────┘
                       │ HTTPS/HTTP on your network
                       ▼
             OpenAI-compatible model host
```

`blackwall-core` stays independent of Tauri so later CLI, gateway, and test clients can share the
same protocol. The current desktop bridge validates and bounds requests, rejects unsafe endpoint
forms, and streams typed events back to the UI. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
for the implemented boundaries and [`docs/PLAN.md`](docs/PLAN.md) for the full agent and sharing
roadmap.

## First guest-sharing slice

Open **Settings** from the bottom-left of the desktop app. Under **Share your model**, confirm the
pinned model, choose an expiry, and select **Create guest link**. Blackwall presents a QR code and
copy-link control containing the same browser URL. The invite expires, can be revoked at any time,
and never reveals the upstream model address or its credentials.

The gateway listens on port `11435` by default. It advertises the owner's Tailscale IPv4 address
when one is available and otherwise falls back to loopback, which is useful only for an owner-side
preview. Guest devices must already be able to reach the advertised address; Blackwall does not yet
automate Tailscale Serve or Funnel.

```sh
# The model may run on a different machine; Blackwall does not launch Ollama.
export BLACKWALL_MODEL_ENDPOINT=http://model-machine:11434/v1
# Or use an existing OLLAMA_HOST instead.

export BLACKWALL_SHARE_PORT=11435                       # optional
export BLACKWALL_SHARE_PUBLIC_URL=https://chat.example # optional advertised URL override
npm run desktop
```

Each invite is bound to one pinned model and permits at most `2` guest chat requests in flight.
**Stop sharing** revokes the invite and closes the share surface. See
[`docs/SHARING.md`](docs/SHARING.md) for the token flow, network assumptions, and current limits.

Named guest accounts, persisted guest telemetry, automatic Serve/Funnel setup, and signed public
desktop distribution are later work—not capabilities of this slice.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
