# Blackwall

Blackwall is a local-first agent harness with a deliberately simple desktop chat: choose your
model, write a message, attach images or files, and receive a streamed response. The model can run
on this computer or on another machine you control through any OpenAI-compatible endpoint.

> **Development status:** guided setup, durable local conversations, project tools with approvals,
> memory, skills, and an interruptible agent runtime are implemented in this checkout. These changes
> are not yet a published release. Persistent pairing is implemented with a configured relay;
> native two-computer acceptance, a default hosted relay, and macOS distribution checks remain open in [the delivery plan](docs/DELIVERY_PLAN.md).

## Download for macOS

[Download the latest macOS build](https://github.com/AlecBhamani1/Blackwall/releases/tag/main).
Choose the `aarch64` DMG for Apple Silicon Macs or the `x64` DMG for Intel Macs.

## What works now

- Guided setup for local models, another computer, or a browser invitation
- Detection of local Ollama and LM Studio; explicit starter-model downloads through Ollama
- Named saved connections with remembered models, tested replacements, and managed Keychain access keys
- Host-approved persistent computer pairing, per-device Keychain credentials, reconnect, and individual removal
- Streaming chat, images/files, Markdown, stop controls, and JSON conversation export
- Native SQLite history with attachment contents, migration, and ordered save retries
- Optional desktop passphrase lock that stops active work and guest sharing
- Agent mode with a native project picker, bounded file read/list/search, reviewed edits, and shell approval
- Opt-in approved web reads/search and up to three concurrent read-only child investigations
- User-managed local memory and reusable Markdown skills
- CLI `bw run`, `bw resume`, and `bw sessions` using the same core agent runtime
- Four independently revocable named QR/browser invitations, saved relay credentials, and updater support

For everyday setup, open **Settings → Set up a connection → Use this computer**. Blackwall detects
an existing model service, guides you to install Ollama if needed, and offers a small model download
only after you choose it. It does not install, start, or reconfigure model-server software itself.

For another computer, create a pairing invitation in Settings on the model host. Paste it into
**Connect a computer** on your other Mac, compare the code on both screens, and approve on the host.
The saved computer reconnects using its own Keychain credential. The host must stay open, unlocked,
and awake. A configured relay is currently required; direct model URLs remain under advanced setup.
Temporary guest invitations open browser chat and have their own expiry and removal controls.

See [the user and operator guide](docs/RUNTIME_GUIDE.md) for project tools, data storage, app-lock
limits, memory, skills, and troubleshooting.

## Run the desktop app

Prerequisites are Node.js 22.12+, npm, the stable Rust toolchain, and the
[Tauri 2 platform prerequisites](https://v2.tauri.app/start/prerequisites/).

```sh
npm install
npm --prefix ui install

export BLACKWALL_MODEL_ENDPOINT=http://your-model-host:11434
export BLACKWALL_RELAY_URL=https://relay.example.com       # optional until sharing
export BLACKWALL_RELAY_TOKEN='your-relay-registration-token'
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
│ src/core agent, tools, storage, and protocol │
│ src/bw   command-line run/resume adapter     │
└───────────┬──────────────────────┬────────────┘
            │ HTTPS/HTTP           │ outbound WSS
            ▼                      ▼
 OpenAI-compatible model     hosted relay ◄── HTTPS ── guest
 host you control
```

`blackwall-core` stays independent of Tauri so the desktop, CLI, and tests share the same runtime. The current desktop bridge validates and bounds requests, rejects unsafe endpoint
forms, and streams typed events back to the UI. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
for the implemented boundaries and [`docs/PLAN.md`](docs/PLAN.md) for the full agent and sharing
roadmap.

## First guest-sharing slice

Open **Settings** from the bottom-left of the desktop app. Under **Share your model**, confirm the
pinned model, enter the HTTPS origin of a hosted relay, choose an expiry, and select **Create guest
link**. Blackwall presents a QR code and copy-link control containing the same browser URL. The
invite expires, can be revoked at any time, and never reveals the upstream model address or its
credentials. The Mac makes an outbound WebSocket connection, so guest access needs no VPN, shared
Wi-Fi, inbound port, or public IP on the Mac.

```sh
# The model may run on a different machine; Blackwall does not launch Ollama.
export BLACKWALL_MODEL_ENDPOINT=http://model-machine:11434/v1
# Or use an existing OLLAMA_HOST instead.

export BLACKWALL_RELAY_URL=https://relay.example.com
export BLACKWALL_RELAY_TOKEN='the-token-configured-on-the-relay'
npm run desktop
```

Each invite is bound to one pinned model and permits at most `2` guest chat requests in flight.
**Revoke** removes one invite; **Stop sharing** removes all active invites. See
[`docs/SHARING.md`](docs/SHARING.md) for the included Docker/Caddy deployment, token flow, and
current limits.

Named guest accounts, end-to-end application encryption, persisted guest telemetry, and signed
public desktop distribution are later work—not capabilities of this slice.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
