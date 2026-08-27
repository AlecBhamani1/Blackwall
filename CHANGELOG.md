# Changelog

All notable changes to Blackwall will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

- Add signed in-app updates backed by continuous macOS builds from GitHub `main`.
- Explicitly bundle the Blackwall logo for the app, Dock, and installer icons.

### Added

- A native Tauri desktop chat connected to a user-configured OpenAI-compatible model endpoint.
- Streaming responses, model discovery and selection, stop controls, and locally saved recent chats.
- Image, text, and file attachments through the picker, drag-and-drop, and pasted images, with
  bounded size and count limits.
- A focused Svelte interface with responsive navigation, Markdown rendering, and a tokenized dark
  visual system.
- A typed Rust protocol and a single typed desktop event channel for UI/core communication.
- Tailnet-only guest sharing with expiring QR/browser-link invites, a pinned model, authenticated
  streaming, and an immediate host-side stop control.
- Repository quality gates for formatting, linting, testing, dependency policy, and commit
  conventions.
