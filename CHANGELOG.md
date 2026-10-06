# Changelog

All notable changes to Blackwall will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Budget model requests for tool definitions, generated output, and a safety reserve. Add `/context`
  and `/compact` to the CLI and chat composer, with optional automatic compaction, retained original
  transcripts, and separate resumable checkpoints that never restore historical approvals.

- Add a one-line installer for the latest development Blackwall CLI, available from any directory.

## [0.1.4] - 2026-10-01

Milestone: [v0.1.4](https://github.com/AlecBhamani1/Blackwall/milestone/1).
Release details and acceptance limits: [0.1.4 notes](docs/releases/0.1.4.md).

### Added

- Interactive `bw` chat and `bw setup`, with persisted CLI settings and `/commands`, `/model`,
  `/endpoint`, `/settings`, `/web`, and `/memory` controls. Resume restores the saved Agent project;
  graceful cancellation retains partial answers, and environment access keys stay scoped to their origin.

- Run Blackwall on Windows 10/11 (x64) and Linux (x86-64 and ARM64, including NVIDIA DGX Spark),
  with installers and signed in-app updates. Access keys, pairing credentials, and the app lock use
  Windows Credential Manager or the Linux Secret Service, and Agent-mode shell commands run in
  Windows PowerShell on Windows. Pairing and guest links work between any mix of macOS, Windows,
  and Linux computers.

- Select Stable or Beta in App updates to test signed builds from `partial` before production,
  with a saved channel preference and an explicit return to stable.

- Browse project files in a right sidebar in Agent mode, with expandable folders, name and
  content search, refresh controls, and bounded text previews inside the selected workspace.
- Render inline and display LaTeX equations in assistant messages with bundled offline fonts.
- Drop screenshots or files anywhere in the conversation to attach them, and use `@` to
  select and reference files from the current chat, including names with spaces.

- A protected `partial` integration branch and checked production promotions, tracked by version
  milestones and release notes, with automatic versioned publication after production CI.
- Both-architecture desktop build checks before promotion, dependency vulnerability review,
  weekly dependency maintenance, issue/PR templates, and reproducible GitHub repository policy.

- Copy buttons on individual Markdown code blocks, with success and failure feedback.

### Fixed

- Keep Blackwall running on macOS when the main window is closed, preserving active work and
  restoring the window from the Dock. Quit and Cmd+Q still exit the app.

- Stop Windows shell descendants on cancellation, timeout, and command completion even when
  their parent PowerShell process has already exited.

- Separate Chat and Agent histories, lock conversation mode and project after the first message,
  restore each Agent conversation’s project, and clear the project when starting a new chat.

### Changed

- Reviewed development pull requests enter a checked merge queue before reaching `partial`.
- Update Rust dependencies while preserving existing stored hashes; upgrade Vitest and coverage
  together to version 5, and retain TypeScript 6 until the Svelte checker supports its successor.

## [0.1.3] - 2026-09-24

- Add signed in-app updates backed by continuous macOS builds from GitHub `main`.
- Explicitly bundle the Blackwall logo for the app, Dock, and installer icons.
- Replace direct private-network guest sharing with a configurable, self-hosted HTTPS relay and
  outbound-only desktop connection.

### Added

- Persistent computer pairing with five-minute invitations, matching-code host consent, per-device
  Keychain credentials, saved computer cards, automatic tunnel recovery, and durable individual revocation.
- Relay pairing capability detection, bounded exchanges, and a shared eight-request guest/device budget.

- Guided local/remote/invitation setup, local service detection, and cancellable Ollama starter downloads.
- Named saved connections and origin-scoped model credentials in macOS Keychain.
- Native SQLite sessions and attachment retention, legacy migration, full-text memory, and JSON export.
- Optional Argon2 passphrase lock with Keychain verification data and stopped work on lock.
- A bounded agent runtime, project file tools, reviewed edits, approved shell/web actions, and restricted child tasks.
- Persistent Markdown skills and memory management screens; real CLI run/resume execution.
- Access-key status, verified model-key replacement, and explicit model/relay-key removal in Settings.
- Up to four named guest invitations with independent revocation and Keychain-backed relay tokens.

### Fixed

- Keep a stable pairing address through failed saves; serialize activation with locking, retain
  authorization until database workers finish, and complete lock cleanup after caller cancellation.

- Persist relay address ownership across host disconnect and process restart, rejecting offline takeover.
- Restore the same active invitation after relay restart without replaying completed prompts.
- Enforce expiry on unfinished relay requests; add heartbeat and write deadlines to stalled tunnels.
- Preserve active invitations if a cancelled new-share operation interrupts expired-link cleanup.
- Add a persistent relay deployment volume and update its Rust builder to the locally verified compiler series.

- Failed connection replacements no longer discard a working connection or its model catalog.
- Deleting active conversations and stopping old streams no longer resurrect chats or contaminate newer turns.
- Failed native writes retain their order for retry, including subsequent deletions.
- Sharing resolves saved model keys; environment credentials are restricted to their configured origin.
- Approval cancellation invalidates pending actions; stale file edits are rejected.

- Interrupted agent runs now finalize visible tool and child activity.
- Failed save flushes during locking preserve composer drafts; named connections retain their own model choice.
- Host loss before a guest response now returns a readable error; abandoned responses release relay state and capacity.

### Existing foundation

- A native Tauri desktop chat connected to a user-configured OpenAI-compatible model endpoint.
- Streaming responses, model discovery and selection, stop controls, and locally saved recent chats.
- Image, text, and file attachments through the picker, drag-and-drop, and pasted images, with
  bounded size and count limits.
- A focused Svelte interface with responsive navigation, Markdown rendering, and a tokenized dark
  visual system.
- A typed Rust protocol and a single typed desktop event channel for UI/core communication.
- Hosted guest sharing with expiring QR/browser-link invites, a pinned model, authenticated
  streaming, automatic host reconnects, and an immediate host-side stop control.
- Repository quality gates for formatting, linting, testing, dependency policy, and commit
  conventions.
