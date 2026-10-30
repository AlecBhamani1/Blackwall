# Blackwall

Blackwall is a local-first agent harness with a deliberately simple desktop chat: choose your
model, write a message, attach images or files, and receive a streamed response. The model can run
on this computer or on another machine you control through any OpenAI-compatible endpoint.

> **Development status:** guided setup, durable local conversations, project tools with approvals,
> memory, skills, and an interruptible agent runtime are implemented in this checkout. These changes
> are available in the current beta builds. Release acceptance remains incomplete.
> Persistent pairing is implemented with a configured relay;
> native two-computer acceptance, a default hosted relay, and macOS distribution checks remain open in the [release checklist](docs/RELEASE_CHECKLIST.md).
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
store access keys through the Secret Service, so a keyring such as GNOME Keyring or KWallet must
be running and unlocked. Without one, a data folder with no app lock still opens and a model access
key is kept for the session only. A folder with a lock, or older data whose lock state Blackwall
can't confirm, waits for the keyring. Windows stores access keys in Credential Manager; macOS uses
Keychain.
Windows and macOS builds are not yet code-signed by a platform publisher, so the first launch shows
a SmartScreen or Gatekeeper warning.

## What works now

- Guided setup for local models, another computer, or a browser invitation
- Detection of model servers on this computer, such as Ollama, LM Studio, llama.cpp and vLLM, on
  their usual ports and on other listening loopback ports (1024 and up, at most 64 checked),
  chosen in setup rather than connected automatically; explicit starter-model
  downloads through Ollama; the server's own explanation when a request fails
- Context-window detection for Ollama (any port), llama.cpp, vLLM, and LM Studio; model reasoning
  kept out of answers and shown collapsed; opt-in reading of tool calls a model writes as text,
  which always ask first ([Local models](docs/RUNTIME_GUIDE.md#local-models))
- Named saved connections with remembered models, tested replacements, and managed access keys in
  the platform credential store (Keychain, Windows Credential Manager, or Secret Service)
- Host-approved persistent computer pairing between any mix of macOS, Windows, and Linux, with
  per-device stored credentials, reconnect, and individual removal
- Streaming chat, images/files, Markdown, stop controls, and JSON conversation export
- Native SQLite history with attachment contents, migration, and ordered save retries
- Optional desktop passphrase lock that stops active work and guest sharing
- Agent mode with a native project picker, bounded file read/list/search, reviewed edits, and shell approval
- Owner-selected command profiles with macOS filesystem isolation and default-off command network
- Owner allow, ask, and deny rules for agent actions, strictest or first-match ordered, with a
  redacted local audit log that names the deciding rule
- Per-project `.blackwall/project.json` settings: deny and ask rules apply right away, and allow
  rules wait until you trust the file's exact contents; any edit asks again
  ([Project settings](docs/RUNTIME_GUIDE.md#project-settings))
- Owner-defined command tools (argv or JSON on stdin, with typed argument schemas), HTTP tools,
  and MCP servers (local commands or streamable HTTP) behind the same approvals, with their secrets kept in the platform
  credential store ([Runtime guide](docs/RUNTIME_GUIDE.md))
- Opt-in approved web reads/search and subagents with your chosen steps and tools (up to three
  read-only children at once), or delegated to a command tool such as `claude -p --permission-mode
  plan` that always asks and shows the whole prompt ([Subagents](docs/RUNTIME_GUIDE.md#subagents))
- Long command output trimmed to its start and end under a configurable cap (64 KiB by default)
- Local OpenTelemetry traces and usage metrics (tokens, durations, tool and run outcomes) for Chat,
  Agent, and subagent runs, kept in the data folder ([Telemetry](docs/TELEMETRY.md))
- Optional OTLP/HTTP export of those traces and metrics to an OpenTelemetry Collector, Grafana
  Alloy, Jaeger, or Langfuse: off by default, https only (except loopback), header values in the
  credential store, and never slowing a run ([Export](docs/TELEMETRY.md#export))
- A Usage page and `bw usage`: tokens by project, model, and day, run success, output speed,
  compaction savings, and top tools, read from those local metrics
- Reviewed agent learning and user-managed local memory, plus reusable Markdown skills that load on
  demand, run directly with `/name`, and can be proposed from a successful run with `/learn`
- Interactive CLI with guided setup, saved settings, `/commands`, and the same core agent runtime
- Four independently revocable named QR/browser invitations, saved relay credentials, and updater support

For everyday setup, open **Settings → Set up a connection → Use this computer**. Blackwall detects
an existing model service, guides you to install Ollama if needed, and offers a small model download
only after you choose it. It does not install, start, or reconfigure model-server software itself.

For another computer, create a pairing invitation in Settings on the model host. Paste it into
**Connect a computer** on your other computer, compare the code on both screens, and approve on the
host. The host and client can run different operating systems: for example, a Linux DGX Spark can
host its model for a Windows laptop. The saved computer reconnects using its own stored credential.
The host must stay open, unlocked, and awake. A configured relay is currently required; direct model URLs remain under advanced setup.
Temporary guest invitations open browser chat and have their own expiry and removal controls.

See [the user and operator guide](docs/RUNTIME_GUIDE.md) for project tools, data storage, app-lock
limits, memory, skills, and troubleshooting.

## Use Blackwall in your terminal

On macOS, Linux, or Windows with a POSIX shell (such as Git Bash), install the latest CLI with
Rust 1.90+ and Cargo available:

```sh
curl -fsSL https://raw.githubusercontent.com/AlecBhamani1/Blackwall/partial/scripts/install-cli.sh | sh
```

The installer works from any directory and installs `bw` onto Cargo's executable path. Then run:

```sh
bw setup
bw
```

If Rust is not installed, follow the link printed by the installer to install it first. To build
from a local checkout instead, use `cargo install --locked --path src/bw`. In a terminal, `bw setup`
lists the model servers it finds on this computer to choose from; `bw setup --list` only prints
them.

Setup asks for an existing OpenAI-compatible service address and its model ID, then saves your
choices. Type `/commands` during chat to see the available controls. Change settings with
`/model <id>`, `/endpoint <url>`, `/web on|off`, `/memory on|off`, or `/settings <name> <value>`.
Use `bw memory pending` (or `/memory pending`) to review agent suggestions, then approve,
reject, edit, or forget them. See [reviewed learning](docs/RUNTIME_GUIDE.md#reviewed-learning) for scopes and controls.
Use `bw project status` to see a project's `.blackwall/project.json`, whether it is trusted, and
what applies now; `bw project trust` shows it and asks before trusting, and `bw project untrust`
undoes that. See [Project settings](docs/RUNTIME_GUIDE.md#project-settings).
Use `bw config set output-bytes <n>` for the command output cap and `bw config set
subagent-steps|subagent-tools|subagent-runs-on <value>` for subagents; see
[Subagents](docs/RUNTIME_GUIDE.md#subagents).
Use `bw mcp` to add, list, enable, disable, or remove MCP servers and save their secrets.
Servers can be local commands or streamable HTTP addresses:
`bw mcp add docs --url https://mcp.example.com/mcp --header Authorization=DOCS_TOKEN`, then
`pbpaste | bw mcp secret docs DOCS_TOKEN`. Header values always come from saved secrets.
In the desktop app, open **Settings → AI capabilities → MCP servers** to manage them.
Settings groups related tools together; search by purpose or choose **Sort by → Name (A–Z)**.
Command and HTTP tools are edited in `tools.json`; `bw tools secret <tool> <NAME>` saves an HTTP
tool header value, piped on stdin, for that tool's origin.
`/learn` asks the agent to propose a skill from the conversation; `bw skills` shows pending
proposals with a diff, and `bw skills approve <id>` or `bw skills reject <id>` decides each one.
`bw skills import <path>` brings in `SKILL.md` skills from Claude Code, Codex, and other agents,
and `bw skills get <name>` downloads a suggested one; each is scanned for hidden instructions and
data leaks first. Type `/name` to run a saved skill directly, as in `/pdf fill form.pdf`.
Use `/settings` to review them. Changes apply to the next turn and future CLI launches.
`bw status` shows the detected model server and its context window. For models that write tool
calls as text, `bw config set text-tool-calls on` reads them; each such call always asks first.
`bw traces` lists recent agent runs with their model and tool calls; `bw traces show <id>` prints
one as a tree; the desktop **Settings → Workflows → Runs** panel shows the same traces. See
[Agent traces](docs/RUNTIME_GUIDE.md#agent-traces).
`bw usage [--days N] [--utc] [--json]` totals tokens, runs, sessions, and tools for the last N
local days (1–400, default 7), by project and model; **Settings → Workflows → Usage** shows the same numbers with a chart and a
per-project session breakdown. See [Usage](docs/RUNTIME_GUIDE.md#usage).
`bw config set otlp-endpoint <url|off>` (plus `otlp-protocol`, `otlp-signals`, and
`otlp-include-project`) exports them over OTLP/HTTP; `bw telemetry header <name>` stores a header
value piped on stdin in the credential store, and `bw telemetry test` sends a test span.
`bw config set usage-retention-days <30-3650|forever>` sets how long usage metrics are kept
(400 days by default). See [Telemetry export](docs/RUNTIME_GUIDE.md#telemetry-export).

For development, `npm run cli -- setup` and `npm run cli` run the CLI without installing it.
`bw run "prompt"` runs one task; `bw sessions` lists conversations; `bw resume <id>` continues
an Agent conversation in its saved project. Add `--json` to `bw run` or `bw resume` for scripts and
CI: events print as JSON lines, and anything owner rules leave to an approval prompt is denied.
`--timeout SECS` (10–86400) limits the whole run, and the exit code says why it ended (0 complete,
1 other error, 2 usage error, 3 an approval was denied, 4 step limit, 5 model error, 6 context
budget, 7 timeout, 130 interrupted). See
the [CLI guide](docs/RUNTIME_GUIDE.md#cli) for credentials, approvals, and configuration precedence.

Use `bw review` or `/review` in CLI or desktop Agent chat to inspect Git changes without agent tools. Choose `--uncommitted`,
`--commit <ref>`, or `--base <ref>`; add `--json` for structured findings and separate hypotheses.
See [Git review](docs/GIT_REVIEW.md) for target semantics, evidence limits, and isolation.

Saved message search works offline with `bw sessions search "search words"`, `/recall search words`
in the CLI, or **Search history** and `/recall` in desktop. Results include bounded excerpts and
session/message references; project and updated-date filters help narrow the history. Agent recall
is off by default; enable it with `bw config set recall on` or the desktop search panel.
See [Conversation recall](docs/CONVERSATION_RECALL.md) for limits, migration, and privacy.

## Project working agreements

CLI and desktop Agent tasks discover `AGENTS.md` guidance, with `AGENTS.override.md` taking
precedence in each directory. Source lists and discovery warnings are visible in both clients.
Extra files such as `CLAUDE.md` can be read after `AGENTS.md`, and the per-file and total limits
(8 and 32 KiB by default) can be changed, in **Memory → Advanced · Instructions** or with
`bw config set instruction-files|instruction-file-kib|instruction-total-kib <value>`. Use `/init` to review a starter instruction file before
writing it. See
[Project instructions](docs/PROJECT_INSTRUCTIONS.md) for ordering, nested scopes, bounds,
and approval behavior.

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
choose **Stable** or **Beta**, review the available version, and choose **Install update and restart**.
Beta delivers signed preview builds from `partial` after CI passes, before features reach main.
Switch back to Stable to install the latest production release. Approved
`partial` → `main` promotions publish a versioned release after production CI and signed builds pass.
Existing installations need
one final manual replacement with an updater-enabled build; later updates install in place. See
[`docs/UPDATES.md`](docs/UPDATES.md) for signing, publishing, and key-recovery details.

## Contributing and releases

`partial` is the default integration branch; `main` is production. Branch from `partial`, link
work to an issue and version milestone, and merge through required CI checks. Promote a batch
with a `partial` → `main` PR containing a new version, release notes, and acceptance approval.
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

`blackwall-core` stays independent of Tauri so the desktop, CLI, and tests share the same runtime. The current desktop bridge validates and bounds requests, rejects unsafe endpoint
forms, and streams typed events back to the UI. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
for context and container diagrams, including command tools, MCP servers, and telemetry export, and
the implemented boundaries. The original plans are kept in [`docs/archive/`](docs/archive/).

## First guest-sharing slice

Open **Settings** from the bottom-left of the desktop app. Under **Share your model**, confirm the
pinned model, enter the HTTPS origin of a hosted relay, choose an expiry, and select **Create guest
link**. Blackwall presents a QR code and copy-link control containing the same browser URL. The
invite expires, can be revoked at any time, and never reveals the upstream model address or its
credentials. The host computer makes an outbound WebSocket connection, so guest access needs no VPN,
shared Wi-Fi, inbound port, or public IP on the host.

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

## Contributing

Start new work from `origin/partial` and open a focused PR into `partial`. After reviewing its
current code, a maintainer can apply `ready-to-merge`; the queue updates the branch, waits for
required CI, and merges reviewed PRs one at a time. New source pushes require another review.
See the [development and production workflow](docs/REPOSITORY_WORKFLOW.md#automatically-merge-reviewed-development-prs)
for setup, paused-queue recovery, and production releases.

## License

Apache-2.0 — see [`LICENSE`](LICENSE).
