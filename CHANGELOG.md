# Changelog

All notable changes to Blackwall will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Add signed in-app updates backed by continuous macOS builds from GitHub `main`.
- Explicitly bundle the Blackwall logo for the app, Dock, and installer icons.
- Replace direct private-network guest sharing with a configurable, self-hosted HTTPS relay and
  outbound-only desktop connection.

### Added

- Copy buttons on individual Markdown code blocks, with success and failure feedback.
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
