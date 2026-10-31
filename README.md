# Blackwall

Blackwall is a local-first agent harness with a simple desktop chat. Choose a model, write a
message, attach images or files, and receive a streamed response. The model can run here or on
another machine you control through any OpenAI-compatible endpoint.

> **Development status:** guided setup, durable local conversations, approved project tools,
> memory, skills, and an interruptible agent runtime are in current beta builds. Release acceptance
> is incomplete. Persistent pairing works with a configured relay. Native two-computer acceptance,
> a default hosted relay, and macOS distribution checks remain open in the
> [release checklist](docs/RELEASE_CHECKLIST.md).
> Linux and Windows builds are new and still need physical-device acceptance.

## Download

[Download the latest build](https://github.com/AlecBhamani1/Blackwall/releases/latest) for your
computer:

| Platform | File | Notes |
| --- | --- | --- |
| macOS, Apple Silicon | `Blackwall_<version>_aarch64.dmg` | |
| macOS, Intel | `Blackwall_<version>_x64.dmg` | |
| Windows 10/11, x64 | `Blackwall_<version>_x64-setup.exe` | Uses WebView2, which Windows 11 includes |
| Linux, x86-64 | `Blackwall_<version>_amd64.AppImage` or `_amd64.deb` | AppImage updates in place |
| Linux, ARM64 (for example NVIDIA DGX Spark) | `Blackwall_<version>_aarch64.AppImage` or `_arm64.deb` | AppImage updates in place |

Linux builds target Ubuntu 22.04 or newer (and equivalent distributions) with WebKitGTK 4.1. They
store access keys through the Secret Service, which needs an unlocked keyring such as GNOME Keyring
or KWallet. Without one, a data folder with no app lock still opens and a model access key is kept
for the session only. A folder with a lock, or older data whose lock state Blackwall can't confirm,
waits for the keyring. Read-only and Workspace-write commands run under the bubblewrap that ships
in the AppImage and `.deb` (Workspace-write needs Linux 5.11 or newer). On Ubuntu 24.04 and later,
AppArmor stops the AppImage's bubblewrap, so use the `.deb` there for sandboxed commands. Windows
uses Credential Manager and macOS uses Keychain. Windows and macOS builds are not yet
publisher-signed, so first launch shows a SmartScreen or Gatekeeper warning.

## What works now

- Guided setup for local models, another computer, or a browser invitation
- Detection of local Ollama, LM Studio, llama.cpp, and vLLM servers, selected during setup rather
  than connected automatically; optional Ollama starter-model downloads; server error messages
- Context-window detection for Ollama (any port), llama.cpp, vLLM, and LM Studio; model reasoning
  kept out of answers and shown collapsed; opt-in reading of tool calls a model writes as text,
  which always ask first ([Local models](docs/RUNTIME_GUIDE.md#local-models))
- Named connections with remembered models, tested replacements, and access keys in the platform
  credential store
- Host-approved persistent computer pairing between any mix of macOS, Windows, and Linux, with
  per-device stored credentials, reconnect, and individual removal
- Streaming chat, images/files, Markdown, stop controls, and JSON conversation export
- Native SQLite history with attachment contents, migration, and ordered save retries
- Optional desktop passphrase lock that stops active work and guest sharing
- Agent mode with a native project picker, bounded file read/list/search, reviewed edits, and shell approval
- Owner-selected command profiles with macOS filesystem isolation and default-off command network
- Owner allow, ask, and deny rules for agent actions, strictest or first-match ordered, with a
  redacted local audit log that names the deciding rule
- Per-project `.blackwall/project.json` settings: deny and ask rules apply immediately; allow rules
  wait for trust and edits require trust again
  ([Project settings](docs/RUNTIME_GUIDE.md#project-settings))
- Owner-defined command, HTTP, and MCP tools behind the same approvals, with secrets in the platform
  credential store ([Runtime guide](docs/RUNTIME_GUIDE.md))
- Opt-in approved web access and configurable subagents, local or delegated to an approved command
  tool ([Subagents](docs/RUNTIME_GUIDE.md#subagents))
- Long command output trimmed to its start and end under a configurable cap (64 KiB by default)
- Local OpenTelemetry traces and usage metrics (tokens, durations, tool and run outcomes) for Chat,
  Agent, and subagent runs, kept in the data folder ([Telemetry](docs/TELEMETRY.md))
- Optional OTLP/HTTP export to an OpenTelemetry Collector, Grafana Alloy, Jaeger, or Langfuse,
  with configurable signals and stored headers ([Export](docs/TELEMETRY.md#export))
- A Usage page and `bw usage` for local token, run, speed, compaction, and tool metrics
- Reviewed agent learning, local memory, and reusable Markdown skills run with `/name` or proposed
  with `/learn`
- Interactive CLI with guided setup, saved settings, `/commands`, and the same core agent runtime
- Four independently revocable named QR/browser invitations, saved relay credentials, and updater support

For everyday setup, open **Settings → Set up a connection → Use this computer**. Blackwall finds an
existing model service, guides you to install Ollama if needed, and offers a model download only
when chosen. It does not install, start, or reconfigure model-server software.

For another computer, create a pairing invitation in Settings on the model host. Paste it into
**Connect a computer** on your other computer, compare the code on both screens, and approve on the
host. Host and client may use different operating systems. Saved computers reconnect with their own
credentials. The host must stay open, unlocked, and awake. Pairing currently needs a configured
relay; direct model URLs remain under advanced setup. Temporary guest invitations open browser chat
and have separate expiry and removal controls.

See [the user and operator guide](docs/RUNTIME_GUIDE.md) for project tools, data storage, app-lock
limits, memory, skills, and troubleshooting.

## Use Blackwall in your terminal

On macOS, Linux, or Windows with a POSIX shell (such as Git Bash), install the latest CLI with
Rust 1.90+ and Cargo available:

```sh
curl -fsSL https://raw.githubusercontent.com/AlecBhamani1/Blackwall/partial/scripts/install-cli.sh | sh
```

The installer works from any directory and puts `bw` on Cargo's executable path. Then run:

```sh
bw setup
bw
```

If Rust is missing, follow the installer's link. To build from a local checkout, use
`cargo install --locked --path src/bw`. The `bw setup` command lists local model servers for
selection; `bw setup --list` only prints them.

Setup saves an existing OpenAI-compatible service address and model ID. Type `/commands` during
chat for controls. Change settings with
`/model <id>`, `/endpoint <url>`, `/web on|off`, `/memory on|off`, or `/settings <name> <value>`.
Use `bw memory pending` or `/memory pending` to review agent suggestions. See
[reviewed learning](docs/RUNTIME_GUIDE.md#reviewed-learning) for the approval, edit, scope, and
removal controls. Use `bw project status`, `bw project trust`, and `bw project untrust` to inspect
and manage `.blackwall/project.json` trust. See
[Project settings](docs/RUNTIME_GUIDE.md#project-settings). Set the command output cap with
`bw config set output-bytes <n>`. Configure subagents with
`bw config set subagent-steps|subagent-tools|subagent-runs-on <value>`;
[Subagents](docs/RUNTIME_GUIDE.md#subagents) covers their values and limits. Use `/settings` to
review settings; changes apply to the next turn and future CLI launches.

Use `bw mcp` or **Settings → AI capabilities → MCP servers** to manage local-command and HTTP
servers and their saved secrets, and **Settings → Agent → Command tools** to manage command tools
(`claude`, `codex` and `copilot` are suggested). Edit HTTP tools in `tools.json`;
`bw tools secret <tool> <NAME>` saves an HTTP header value from standard input. The Runtime guide
covers MCP commands, headers, and secret handling. Settings supports purpose search and
**Sort by → Name (A–Z)**.

Use `/learn` to propose a skill, `bw skills` to review proposals, and `bw skills approve <id>` or
`bw skills reject <id>` to decide them. Use `bw skills import <path>` to import `SKILL.md` skills
and `bw skills get <name>` to download a suggested one. `bw skills list` lists saved skills, and
`bw skills remove <name>` removes one even when its size prevents loading. Blackwall scans them
before use. Run a
saved skill with `/name`, such as `/pdf fill form.pdf`.

`bw status` shows the model server and context window. `bw config set text-tool-calls on` reads
textual tool calls and asks before each one. `bw traces` lists runs; `bw traces show <id>` and
**Settings → Workflows → Runs** show details. See
[Agent traces](docs/RUNTIME_GUIDE.md#agent-traces).

`bw usage` and **Settings → Workflows → Usage** show local usage. See
[Usage](docs/RUNTIME_GUIDE.md#usage) for flags, ranges, and breakdowns.

Configure OTLP/HTTP with `bw config set otlp-endpoint <url|off>`, `otlp-protocol`, `otlp-signals`,
and `otlp-include-project`. Use `bw telemetry header <name>` for stored headers and
`bw telemetry test` for a test span. Set retention with
`bw config set usage-retention-days <value>`. See
[Telemetry export](docs/RUNTIME_GUIDE.md#telemetry-export) for values, defaults, and bounds.

For development, `npm run cli -- setup` and `npm run cli` run the CLI without installing it.
`bw run "prompt"` runs one task; `bw sessions` lists conversations; `bw resume <id>` continues an
Agent conversation in its saved project. The `--json` and `--timeout SECS` options support scripts
and CI. See the [CLI guide](docs/RUNTIME_GUIDE.md#cli) for their behavior, bounds, exit codes,
credentials, approvals, and configuration precedence.

Use `bw review` or `/review` in CLI or desktop Agent chat to inspect Git changes without agent
tools. See [Git review](docs/GIT_REVIEW.md) for targets, flags, evidence limits, and isolation.

Search saved messages offline with `bw sessions search "search words"`, `/recall search words` in
the CLI, or **Search history** and `/recall` in desktop. Agent recall is off by default; enable it
with `bw config set recall on` or the desktop search panel. See
[Conversation recall](docs/CONVERSATION_RECALL.md) for filters, result limits, migration, and
privacy.

## Project working agreements

CLI and desktop Agent tasks discover `AGENTS.md`, with `AGENTS.override.md` taking precedence in
each directory. Both clients show sources and warnings. Configure extra files and size limits in
**Memory → Advanced · Instructions** or with `bw config set
instruction-files|instruction-file-kib|instruction-total-kib <value>`. Use `/init` to review a
starter file before writing it. See [Project instructions](docs/PROJECT_INSTRUCTIONS.md) for
ordering, nested scopes, defaults, bounds, and approval behavior.

## Run the desktop app

Prerequisites are Node.js 22.12+, npm, the stable Rust toolchain, and the
[Tauri 2 platform prerequisites](https://v2.tauri.app/start/prerequisites/).
On Ubuntu or Debian, `./scripts/install-linux-deps.sh` installs the WebKitGTK, D-Bus, and bundling
libraries. On Windows, install the Visual Studio C++ build tools and WebView2, then set the variables
below with `$env:NAME = 'value'` in PowerShell.

```sh
npm install
npm --prefix ui install

export BLACKWALL_MODEL_ENDPOINT=http://your-model-host:11434
export BLACKWALL_RELAY_URL=https://relay.example.com       # optional until sharing
export BLACKWALL_RELAY_TOKEN='your-relay-registration-token'
npm run desktop
```

Without a saved override, Blackwall uses `OLLAMA_HOST`; no export is needed when it already points
to the model machine. Both `http://host:11434` and `http://host:11434/v1` work. Set
`BLACKWALL_MODEL_API_KEY` for a Bearer token.

For browser-only UI development:

```sh
export BLACKWALL_DEV_MODEL_ENDPOINT=http://your-model-host:11434
npm run dev
```

Open `http://127.0.0.1:1420`. The development server proxies to the model host. Production desktop
traffic uses the Rust bridge.

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

Blackwall checks for signed updates at startup. Open **Settings → App updates** to choose **Stable**
or **Beta**, review the version, and select **Install update and restart**. Existing installations
need one manual replacement with an updater-enabled build. Later updates install in place. See
[`docs/UPDATES.md`](docs/UPDATES.md) for channel behavior, signing, publishing, and key recovery.

## Contributing and releases

`partial` is the default integration branch; `main` is production. Branch from `partial`, link an
issue and version milestone, and merge through required CI. Promote a batch with a `partial` →
`main` PR, new version, release notes, and acceptance approval.
See [CONTRIBUTING.md](CONTRIBUTING.md) and the [repository workflow](docs/REPOSITORY_WORKFLOW.md).

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

`blackwall-core` stays independent of Tauri so desktop, CLI, and tests share one runtime. The
desktop bridge bounds requests, rejects unsafe endpoints, and streams typed UI events. See
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for diagrams and implemented boundaries. Original
plans remain in [`docs/archive/`](docs/archive/).

## First guest-sharing slice

Open **Settings** from the desktop's bottom-left. Under **Share your model**, confirm the model,
enter the hosted relay's HTTPS origin, choose an expiry, and select **Create guest link**. Blackwall
shows the browser URL as a QR code and copyable link. Invites expire, can be revoked, and never
reveal the model address or credentials. The host uses an outbound WebSocket, so it needs no VPN,
shared Wi-Fi, inbound port, or public IP.

```sh
# The model may run on a different machine; Blackwall does not launch Ollama.
export BLACKWALL_MODEL_ENDPOINT=http://model-machine:11434/v1
# Or use an existing OLLAMA_HOST instead.

export BLACKWALL_RELAY_URL=https://relay.example.com
export BLACKWALL_RELAY_TOKEN='the-token-configured-on-the-relay'
npm run desktop
```

Each invite is bound to one model. **Revoke** removes one invite; **Stop sharing** removes all
active invites. See [`docs/SHARING.md`](docs/SHARING.md) for Docker/Caddy deployment, token flow,
and current limits.

Named guest accounts, end-to-end application encryption, persisted guest telemetry, and signed
public desktop distribution are later work—not capabilities of this slice.

## Contributing

Start from `origin/partial` and open a focused PR into `partial`. After review, a maintainer can
apply `ready-to-merge`. The queue updates the branch, waits for required CI, and merges reviewed PRs
one at a time. New source pushes require another review.
See the [development and production workflow](docs/REPOSITORY_WORKFLOW.md#automatically-merge-reviewed-development-prs)
for setup, paused-queue recovery, and production releases.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
