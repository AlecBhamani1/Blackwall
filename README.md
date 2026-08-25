# Blackwall

A local-first agent harness for running **local LLMs** — chat, an agent loop with tool calls, subagents, memory, and skills — with a desktop UI that mirrors the simplicity and readability of Cursor and Codex.

Everything runs locally: the model (any OpenAI-compatible endpoint — Ollama by default), the memory database, the sessions, the skills. No cloud, no connection service.

> **Status:** planning & design — see [`docs/PLAN.md`](docs/PLAN.md) for the full implementation plan, design system, feature specs, and task breakdown.

## Features (v0.1 scope)

- **Chat + agent loop** — streaming responses, shell & file tools, inline approval cards (allow / always / deny)
- **Subagents** — the main agent spawns focused child agents (parallel, foreground/background)
- **Internet access** — gated `web_fetch`/`web_search` with per-host approval and an SSRF guard
- **Security** — passphrase unlock (argon2id), macOS Keychain for keys, workspace jail for file tools
- **Learning** — Hermes-style user + agent memory stores the agent curates itself, recalled via search
- **Memory database** — built-in SQLite (FTS5) by default, or point at a local Postgres
- **Skills** — reusable `SKILL.md` workflows the agent can load, create, and fix on its own
- **Sessions** — resume past conversations, project-scoped, stored locally

## Architecture

```
┌─────────────────────────────────────────────┐
│  ui/        Svelte 5 webview (Tauri 2)      │  presentation only — restyled via theme.css tokens
├─────────────────────────────────────────────┤
│        typed serde event bridge             │  blackwall://event  (the UI contract)
├─────────────────────────────────────────────┤
│  src/core   blackwall-core (headless Rust)  │  model client · agent loop · subagents
│                                                 tools · approvals · web · memory
│                                                 skills · auth · sessions
│  src/app    Tauri shell        src/bw  CLI   │
└─────────────────────────────────────────────┘
```

- `blackwall-core` is framework-free and unit-tested — the TUI, GUI, or CLI are all thin clients over the same event protocol.
- All state lives under `~/.blackwall/` (config, sessions, memory DB, skills); keys live in the macOS Keychain.

## Stack

Rust · Tauri 2 · Svelte 5 · TypeScript · Tailwind CSS 4 · SQLite (rusqlite/FTS5) · tokio · reqwest

## Development

Full build steps land with the scaffold (milestone M0). Conventions: Conventional Commits, `cargo fmt` + `clippy -D warnings` + `cargo deny`, `vitest` for the UI, GitHub Actions CI. See `CONTRIBUTING.md` once present.

## License

Apache-2.0 — see [LICENSE](LICENSE).
