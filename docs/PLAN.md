# Blackwall — Local Agent Harness (GUI) Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** Build Blackwall, a macOS-first GUI desktop app that runs local LLMs (Ollama by default, any OpenAI-compatible endpoint) through a chat + agent loop with subagent spawning, gated internet access, local-only memory/skills/learning, self-hosted relay sharing, and tight security — styled after Cursor/Codex: minimal chrome, maximum signal, excellent readability.

**Architecture:** Tauri 2 app. A headless Rust `blackwall-core` library owns the agent (model client, agent loop, subagents, tools, approvals, web, memory, skills, relay host, sessions) and talks to UI exclusively through typed serde events. The Svelte 5 webview is a thin, restylable presentation layer. Sessions, memory, configuration, model access, and keys remain local; guest traffic crosses a separately deployed relay chosen by the user.

**Tech Stack:**
- Rust 1.8x, Tauri 2, `tokio`, `reqwest` (stream), `axum` (hosted relay + tests), `tokio-tungstenite`, `serde`/`serde_json`, `similar` (diffs), `rusqlite` (bundled SQLite) + `tokio-postgres` (external memory DB), `keyring` (Keychain), `argon2`, `qrcode` (invite QR), `thiserror`, `anyhow`
- Svelte 5 (TS), Vite, Tailwind CSS 4, `vitest` + `@testing-library/svelte`
- Quality: `cargo fmt`, `clippy -D warnings`, `cargo deny`, lefthook pre-commit, commitlint, GitHub Actions CI, conventional commits, signed-tags release

**Repo:** `https://github.com/AlecBhamani1/Blackwall.git` — base commit `0bc124f` (Apache-2.0) on `AlecBhamani1/design-local-agent-harness`. Implement on `main`. Push access granted.

---

## Design System (Cursor + Codex)

Shared DNA: **no decorative chrome, always-visible state, one accent color, muted body text, strong type hierarchy.**

### Token table (single source of truth — `ui/src/theme.css`)

| Token | Value | Use |
|---|---|---|
| `--bg-base` | `#0e0f13` | App background |
| `--bg-panel` | `#14161c` | Sidebar, composer, status bar |
| `--bg-elevated` | `#1a1d25` | Tool cells, code blocks, hover |
| `--bg-user-msg` | `#1e222c` | User prompt blocks |
| `--border` | `#262a35` | 1px separators |
| `--text-primary` | `#e8eaed` | Agent prose, headings |
| `--text-muted` | `#8b90a0` | Meta, timestamps, hints |
| `--text-faint` | `#565b6b` | Placeholders, kbd hints |
| `--accent` | `#39c5a8` | Teal, used *sparingly*: focus, active dot, kbd chips |
| `--ok` | `#4ade80` | Success, diff `+`, connected client dot |
| `--err` | `#f87171` | Errors, diff `-` |
| `--warn` | `#fbbf24` | Waiting-for-approval, spinners |
| `--focus-ring` | `#39c5a866` | 2px ring |
| `--subagent-bg` | `#14201f` | Subagent section tint (subtle teal-shift) |

### Typography
- UI: system stack, body 13.5px / 1.55. Section labels 13px semibold, small-caps tracking.
- Code: `"SF Mono", ui-monospace`, 12.5px.
- Hierarchy: user msg on `--bg-user-msg`; agent prose normal; **bold white** only for agent headers (Codex pattern).

### Layout
```
┌──────────────────────────────────────────────────────────┐
│ ⌘ [◆ Blackwall]  project ▾   model ▾  ●  📡  🔒  ⚙       │
├──────────────┬───────────────────────────────────────────┤
│ Sidebar      │  Chat transcript                           │
│  ● new chat  │   • agent markdown (muted prose)           │
│  Sessions:   │   ┌ tool cell ▸ shell npm test ✓ 2.1s ┐    │
│  Memory     │   └──────────────────────────────────┘     │
│  Skills     │   [user block]                              │
│  Sharing 📡 │   ┌ subagent section (tinted, indented)    │
│              │   │  ▸ researching web… ✓ summary returned │
│              │   └────────────────────────────────────┐   │
│              │ [approval card: cmd / diff / web host   │   │
│              │  Enter=Allow A=Always Esc=Deny]         │   │
│              │ ┌─────────────────────────────────────┐  │
│              │ │ Composer  "Ask Blackwall to do…"    │  │
│              │ └─────────────────────────────────────┘  │
│              │ ● local-model  ~/proj  ctx 43%/32k  esc   │
└──────────────┴───────────────────────────────────────────┘
```
Readability rules: tool cells **collapsed one-liners** by default (expand on click); approvals are **inline cards** (exact command/diff/host + kbd hints); **subagent work renders as a tinted, indented section** with a status pill (running ✓ / ✗) and its result summarized back into the main flow; status strip always shows model, cwd, context %, state dot (green=running / amber=waiting / red=error). One accent color; color encodes state only.

### Restylability
All visual decisions live in `theme.css` tokens + Tailwind referencing them. Full theme swap = edit ~20 hex values + build. `core` has zero styling concerns.

---

## Repository layout (senior-engineer standard)

```
Blackwall/
├── LICENSE                      # Apache-2.0
├── README.md                    # quickstart, badges, architecture summary
├── CONTRIBUTING.md              # dev setup, commit/release conventions
├── CHANGELOG.md                 # keep-a-changelog format
├── rustfmt.toml                 # edition 2021, max_width 100
├── .clippy.toml
├── deny.toml                    # cargo-deny: bans, duplicates, sources, advisories
├── .lefthook.yml                    # hooks: fmt, clippy, test on Rust; vitest on ui
├── commitlint.config.cjs        # Conventional Commits enforced
├── .github/workflows/ci.yml     # test/clippy/deny/fmt + tag→release
├── docs/
│   ├── ARCHITECTURE.md          # layer diagram, event protocol, data flow, security model
│   └── SHARING.md               # hosted relay deployment, invite/QR flow, guest setup
├── src/
│   ├── Cargo.toml               # workspace: core, app (tauri), bw (cli)
│   ├── core/                    # blackwall-core: framework-free, unit-tested
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── protocol.rs      # AgentEvent + Request/Decision types (the UI contract)
│   │       ├── config.rs        # ~/.blackwall/config.json, validated, versioned
│   │       ├── model.rs         # OpenAI-compatible streaming client (Ollama default :11434)
│   │       ├── agent.rs         # agent loop state machine
│   │       ├── subagents.rs     # spawn/track/limit child agent runs
│   │       ├── approvals.rs     # approval policy + gating (commands, files, network)
│   │       ├── tools/
│   │       │   ├── mod.rs       # Tool trait + registry
│   │       │   ├── shell.rs
│   │       │   ├── files.rs
│   │       │   ├── web.rs       # fetch/search w/ SSRF guard + size/time caps
│   │       │   ├── memory.rs    # remember/recall/curation tools
│   │       │   ├── skills.rs    # view/create/patch skill tools
│   │       │   └── subagent.rs  # spawn_agent tool
│   │       ├── memory/
│   │       │   ├── mod.rs       # MemoryStore trait + learning pipeline (inject/reflect)
│   │       │   ├── sqlite.rs    # built-in FTS5 store (default)
│   │       │   └── postgres.rs  # external-DB adapter (native local, no cloud)
│   │       ├── share/
│   │       │   ├── mod.rs       # ShareHub: client registry, keygen, lifecycle
│   │       │   ├── relay.rs     # outbound WebSocket host and SSE passthrough
│   │       │   └── relay_protocol.rs # versioned host/relay message contract
│   │       ├── skills.rs        # discovery, frontmatter, prompt index, CRUD
│   │       ├── auth.rs          # passphrase (argon2id), keyring-backed, unlock flow
│   │       └── session.rs       # JSONL persistence + resume
│   │   └── tests/               # integration: scripted-backend loop, golden diffs
│   ├── app/                     # Tauri crate: tauri.conf.json, commands.rs, events bridge
│   ├── relay/                   # separately deployable public HTTP/WebSocket relay
│   └── bw/                      # thin CLI (blackwall run "…") proving core is headless
├── skills/                      # bundled example skills shipped with the app
└── ui/
    ├── package.json             # svelte 5, vite, tailwind 4, vitest, testing-library
    └── src/
        ├── theme.css
        ├── App.svelte
        └── lib/
            ├── store.ts         # svelte stores: cells, run state, approvals, memory, skills
            ├── ipc.ts           # typed wrappers over @tauri-apps/api
            └── components/      # Sidebar, ChatView, MessageCell, ToolCell, DiffCell,
                                 # SubagentSection, Composer, ApprovalCard, WebRequestCard,
                                 # StatusBar, SettingsPanel, MemoryPanel, SkillsPanel,
                                 # SharePanel, InviteModal, LockScreen
```

### Senior-engineer repo conventions (enforced from day one, in CI + hooks)
- **Errors:** `thiserror` typed errors in `core`; mapped to `AgentEvent::Error`; `anyhow` only at the `app`/`bw` edges. No `unwrap` outside `#[cfg(test)]` (clippy lint).
- **Async:** all I/O on tokio; tool execution always timeout-capped; agent loop max-iteration guard.
- **Config:** versioned (`"v": 1`) JSON with defaults + validation; unknown keys warn, not panic.
- **Testing:** unit per module; integration tests with a `ScriptedBackend` (no network); golden-file tests for diff rendering; vitest component tests for `ToolCell`, `DiffCell`, `ApprovalCard`, `store.ts` event mapping. Coverage reported in CI.
- **Deps:** `cargo deny` (advisories, bans, dupes); minimal, documented dependencies.
- **Commits:** Conventional Commits (commitlint via lefthook). Squash-merged PRs; CHANGELOG updated per release.
- **Docs:** `docs/ARCHITECTURE.md` maintained as the security + data-flow reference; every public type in `protocol.rs` has a doc comment (it's the UI contract).
- **Releases:** semver tags `vX.Y.Z` → CI builds `.dmg`+`.app`, writes updater `latest.json`, GitHub Release.

---

## Feature specs (Alec's requirements)

### A. Subagents (main agent spawns children)
- `spawn_agent` tool: args `{goal, context, tools?, background?}`. Child runs its own agent loop with its own conversation, a (default-restricted) toolset — always gets `read_file`, optional `shell`/`web` per call — and streams `AgentEvent`s tagged with `agent_id` + `parent_id`.
- Concurrency: up to **3 parallel** children (`tokio`-bounded); foreground children block the parent turn, background children report on completion.
- Result: child's final message returns as a `tool_result` to the parent; UI renders children as a **tinted indented section** with status pill; a failed child returns a structured error, never crashes the turn.
- **All-kill switch:** interrupting the parent interrupts its children (task tree).

### B. Internet access (opt-in, gated)
- `web_fetch` tool: GET-only, per-request **approval card** (host, method, purpose from agent), max 256KB body, 30s timeout, 2 redirects. `web_search` via a user-configured local/proxy search endpoint (key in Keychain) — or direct DuckDuckGo HTML when no provider set.
- **SSRF guard (default deny):** block loopback/link-local/private IP ranges *unless* the host is on the network allowlist (Ollama itself is never hit via this tool).
- Per-session allowlist of fetched hosts (same "Always allow" pattern as commands); all network activity visible in the transcript.

### C. Security — who can get into the AI
1. **App lock:** first run sets a passphrase → `argon2id` hash in `~/.blackwall/config.json`; on launch a LockScreen until unlock. Unlock token stored in macOS **Keychain** (`keyring`) when "remember this Mac" is checked; otherwise passphrase every launch.
2. **Key storage:** every endpoint/search key lives in Keychain, never in the config JSON.
3. **Data locality:** `~/.blackwall/` holds `config.json`, `sessions/`, `memory.sqlite` (default DB), `share.json`. File permissions 0600/0700.
4. **Agent-level:** approval policy (command allowlist, file writes default-ask, network default-ask) persisted to config; scope = session (and optional per-project overrides under `projects/<slug>/policy.json`).
5. **Workspace jail:** file tools resolve paths under the chosen project dir; `..` escapes denied. Shell runs in that cwd.

### D. Learning (Hermes-style)
- Two memory stores, exactly like Hermes: **user profile** (who Alec is, preferences) and **agent memory** (environment facts, conventions, lessons). Declarative facts, compact, injected into the system prompt every turn with a char budget (e.g. 2,200 + 1,375).
- **Agent curation tools:** `memory_add / memory_replace / memory_remove` (target: user|agent) — the agent writes its own learnings during/after tasks.
- **Reflection:** on session end, if the session was > N turns and had corrections/errors, the agent is prompted to extract durable facts into memory (explicit in transcript, user can veto).
- **Recall tool:** `memory_search(query)` → FTS5 top-k with snippets, so long-running context isn't re-injected whole.
- UI: **Memory panel** (editable entries per store, char usage bar, delete/edit).

### E. Memory database (built-in + native external)
- `MemoryStore` trait: `upsert`, `delete`, `search`, `list`.
- **Built-in (default):** SQLite file with FTS5 — zero config, local.
- **External (native):** config field `memory.backend = { kind: "postgres", url: "postgres://user@localhost:5432/blackwall" }` — `tokio-postgres` adapter, **local connections only** (no TLS-to-cloud requirement; `sslmode=disable|prefer`). Schema bootstrap with a versioned migrations module (also used by the SQLite store). No connection service — the DB process runs wherever Alec points it.
- Sessions stay JSONL regardless of memory backend (resume never depends on DB).

### F. Skills
- Skill = directory `SKILL.md` with YAML frontmatter (`name`, `description`, `tags`) + optional `references/`, `scripts/`, `templates/`.
- Discovery: `~/.blackwall/skills/**` (user) + bundled `skills/` in the repo (shipped with the app).
- **Index injection:** name+description of every skill into the system prompt (truncated ~57 chars each, Hermes-style) so the agent knows what's loadable without bloat.
- **Agent tools:** `skill_view(name, file?)`, `skill_manage(action=create|patch|delete|write_file, …)` — the agent creates/fixes skills on hard-won workflows (exactly the Hermes loop).
- UI: **Skills panel** (list, search, view, edit in OS editor, enable/disable).

### G. Local model sharing (guests via QR / invite link through a hosted relay)
Guests can use the owner's model from any network without exposing the Mac, requiring shared Wi-Fi,
or installing a VPN client.

- **Outbound host transport:** Blackwall opens an authenticated WebSocket to a separately deployed
  `blackwall-relay` process. Flow: `guest → HTTPS relay → existing outbound WSS → Mac → model`.
- **Configurable deployment:** the relay origin is saved in Settings and can be changed without an
  app rebuild. A deployment-wide registration token limits who may publish sessions. Docker/Caddy
  files provide a self-hosted HTTPS/WSS deployment on another machine.
- **Invites:** each share generates independent random session, host-reconnect, and 256-bit guest
  credentials. The guest key appears once in the URL fragment; only its salted digest is registered
  with the relay.
- **Host controls:** each share pins one model regardless of the guest payload, permits two
  concurrent guest chats, expires automatically, and can be stopped immediately by the host.
- **Security:** the model endpoint and API key never leave the Mac. Request/response sizes are
  bounded on the relay and host. The first protocol uses HTTPS/WSS but not application-level E2EE,
  so the user-operated relay is inside the trust boundary.
- **Future client:** an iPhone app can consume the same public HTTPS API and invite without hosting
  a listener or sharing a local network with the Mac.

---

## Tasks (TDD where code; milestone gates)

### M0 — Scaffold
**T1. Seed `main`.** From current branch: `git checkout -b main`, add `rustfmt.toml`, `.clippy.toml`, `deny.toml`, `.lefthook.yml`, `commitlint.config.cjs`, `CONTRIBUTING.md`, `CHANGELOG.md` stubs. Verify: `git log` clean, hooks installed via `lefthook install` (`npx lefthook install`). Push `main`.
**T2. Tauri+Svelte scaffold.** `npm create tauri-app@latest -- --template svelte-ts` → restructure to `src/app` + `ui/` per layout. Cargo workspace: `core` (lib), `app` (tauri), `bw` (bin stub). Verify: `npm run tauri dev` opens window; `cargo test` green. Commit.

### M1 — Design system + static shell
**T3. Theme tokens.** `ui/src/theme.css` = full token table (incl. `--subagent-bg`). Verify token renders. Commit.
**T4. App shell (static).** `App.svelte` + `Sidebar/StatusBar/Composer/ChatView` with hard-coded sample cells including one tool one-liner and one subagent section. Verify via screenshot vs layout diagram. Commit.

### M2 — Core agent (headless, TDD, no UI deps)
**T5. Protocol.** `protocol.rs`: `AgentEvent` (assistant_delta, tool_call, tool_result, approval_request{kind: exec|file|network|subagent}, subagent_status, turn_complete, error, memory_updated, skill_updated, share_client_*), request/decision types; doc-comment every variant. TDD: serde round-trips. Commit.
**T6. Model client.** `model.rs`: OpenAI-compatible SSE streaming (`base_url` default `http://localhost:11434/v1`, `model` default from config, key optional for local). TDD against `axum` canned-SSE server (deltas → tool_calls → finish; multi-turn). Commit.
**T7. Config + workspace root.** `config.rs`: schema v1, `~/.blackwall/config.json`, validated defaults (endpoint=Ollama, context window default 32k, subagent_max=3, configurable relay origin, memory backend=sqlite). TDD: defaults, missing-file, bad-version, unknown-key-warn. Commit.
**T8. Agent loop.** `agent.rs` + `ScriptedBackend` trait: stream deltas, execute tool calls, append results, loop until stop (max 20 iters), interrupt oneshot. TDD: event-order assertions incl. error propagation. Commit.
**T9. Tools: shell + files + diffs.** `tools/shell.rs` (timeout 120s, 64KB cap, exit code), `tools/files.rs` (workspace jail), `diff.rs` unified-diff via `similar`. TDD incl. jail-escape (`../x`) and golden diff test. Commit.
**T10. Approvals.** `approvals.rs`: policy (command-prefix allowlist, file-write ask, network ask, per-session + per-project), oneshot gating, "always" persistence, denial = structured `tool_result`. TDD: allow/deny/always matrix. Commit.
**T11. Sessions.** `session.rs`: JSONL append under `~/.blackwall/projects/<sha1(cwd)>/sessions/`, list/load/resume. TDD round-trip. **Gate: `cargo test -p blackwall-core` + clippy clean; `bw run "echo hi"` works headless against Ollama (Alec's machine).**

### M3 — Memory + learning + skills
**T12. MemoryStore + SQLite FTS5.** `memory/mod.rs` trait, `memory/sqlite.rs` (FTS5, char budgets, targets), migrations module. TDD: upsert/search/prefix/replace/remove. Commit.
**T13. Postgres adapter.** `memory/postgres.rs` + shared migrations; TDD with a local test PG via `BLACKWALL_TEST_PG_URL` (`#[ignore]` when unset). Commit.
**T14. Learning pipeline.** System-prompt injection (user+agent stores, budget, formatting), memory tools (`memory_add/replace/remove/search`), end-of-session reflection prompt. TDD: injection formatting, budget truncation, tool round-trips to store. Commit.
**T15. Skills.** `skills.rs`: discovery, frontmatter parse (malformed ⇒ skipped + warned), prompt index, `skill_view`/`skill_manage` tools; 2 bundled skills (`bugfix-triage`, `repo-orientation`) in `src/../skills/` as examples. TDD: discovery, index truncation, create→patch→delete lifecycle. **Gate: agent in a real session can recall a fact from last session and create a skill that persists to `~/.blackwall/skills`.**

### M4 — Subagents + Web
**T16. Subagent runtime.** `subagents.rs` + `tools/subagent.rs`: spawn (fg/bg), concurrency cap 3, event tagging, result-to-parent, interrupt propagation, structured failure. TDD with `ScriptedBackend`: parent sees child's final message; cap respected; interrupt kills children. Commit.
**T17. Web tools.** `tools/web.rs`: `web_fetch` (GET, caps, redirects), SSRF guard, approval gating, host allowlist; `web_search` (provider-agnostic; DDG HTML fallback). TDD: SSRF cases (127.0.0.1, 10.x, 192.168.x, CGNAT, allowlist override), size/timeout caps. **Gate: live run — agent researches a topic on the web with one approval, spawns a subagent that summarizes it.**

### M5 — Local model sharing (core + relay)
**T18. Hosted relay.** `src/relay` (axum): host WebSocket registration, Bearer invite authentication, `/v1/chat/completions` streaming passthrough, model pinning, two-request cap, `/v1/models`, `/health`, and bounded in-memory sessions. TDD with a real socket pair: key valid/invalid, pin enforced, cap 429, stream completes. Commit.
**T19. ShareHub outbound transport.** `share/mod.rs` + `share/relay.rs`: random invite/session/reconnect credentials, salted guest-key digest, configurable HTTPS relay origin, deployment token, reconnect loop, response caps, QR link, and immediate host revocation. Include Docker/Caddy deployment. **Gate: a second device on cellular opens an invite through the hosted relay, receives a stream from the Mac's pinned model, and loses access when the host stops sharing.**

### M6 — Security
**T20. Auth.** `auth.rs`: argon2id passphrase, Keychain integrate (`keyring`), 0600/0700 perms, unlock API; TDD: hash/verify, keyring mock, bad-path perms. UI: **LockScreen** (first-run setup flow too). Commit.
**T21. Key storage.** Endpoint + search keys in Keychain only; config stores key *ids*. UI: Settings keys fields write via `save_key` command; TDD round-trip. Commit.

### M7 — UI wiring
**T22. Bridge.** `app/src/commands.rs`: `start_session/send_message/resolve_approval/interrupt/list_sessions/load_session/settings/memory_*/skills_*/unlock/share_*`. Single event channel `blackwall://event` carrying `AgentEvent` (incl. `share_client_*`). Verify: devtools receives events. Commit.
**T23. Transcript.** `store.ts` (event→cells mapping), `MessageCell`, `ToolCell` (collapsed one-liner), `DiffCell`. vitest: store mapping + ToolCell/DiffCell. Live: stream a real turn. Commit.
**T24. Approvals in UI.** `ApprovalCard` (exec/file) + `WebRequestCard` (host/method), kbd shortcuts, amber run-state while pending, allowlist effect. vitest + live matrix (allow/always/deny × 3 kinds). Commit.
**T25. Subagent UI.** `SubagentSection` (tinted, indented, status pill, expandable child transcript). Live: spawn visible. Commit.
**T26. Status bar + context meter.** model, cwd, ctx% (server `usage` when available else 4-chars≈1-token, labeled "est"), state dot, `📡 N` active-guest indicator. Commit.
**T27. Sidebar + resume.** sessions grouped by project, resume renders full history. Commit.
**T28. Memory panel + Skills panel + Settings.** settings (endpoint URL/model/key, context window, memory backend selector + postgres URL, allowlists, subagent cap); memory CRUD UI; skills list/view/toggle. Commit.
**T29. Sharing panel + invite modal.** `SharePanel` (top-bar icon 📡 + sidebar entry): saved relay origin, non-persistent deployment token, global stop control, client status, pinned-model dropdown, last seen, request/token telemetry, disconnect, and reset key. `InviteModal`: QR, copyable hosted browser link, and key-shown-once warning. `docs/SHARING.md` covers relay deployment and trust. Live: the Mac invites a second device over cellular, the host pins a different model than requested, usage accrues, and host disconnect returns an unavailable/unauthorized response. **Gate: full sharing e2e through the separately hosted relay; screenshot for Alec.**

### M8 — Ship
**T30. Branding + packaging.** Name **Blackwall**, identifier `com.blackwall.app`, icons, `.dmg`/`.app`, updater plugin pointed at GitHub Releases, `latest.json`. Verify: install on clean volume. Commit.
**T31. CI + release.** `ci.yml`: fmt/clippy(-D warnings)/deny/test (core)+vitest on PR; tag→dmg+release+latest.json (+ optional notarization job gated on secrets). First tag `v0.1.0`. Verify: green CI, assets on release page.
**T32. Docs + `bw` CLI polish.** `docs/ARCHITECTURE.md` (layers, event protocol, security model, data flow), README quickstart + badges + shortcuts table + theming pointer; `bw run/resume` + `bw share list/invoke` flags finished. Commit + tag `v0.1.0`.

---

## Risks & tradeoffs
- **Local model latency** → full streaming + spinner affordances; no busy-wait UI.
- **Context %** → use server-reported `usage` when present; else labeled estimate.
- **Tauri sandbox vs spawned shell** → sandbox off in v0.1 with documented note; per-command sandboxing is post-v0.1.
- **Unnotarized builds** → Gatekeeper prompt once; add `APPLE_ID` secrets later.
- **Postgres adapter** → ships behind config selector; default path is SQLite so the MVP never needs PG.
- **Subagent token cost on local models** → restricted default toolset + context-passing discipline in the prompt; background children keep parent responsive.
- **Sharing = new attack surface** → relay is the only public guest entry (auth → pin → caps → proxy); per-invite in-flight caps protect the host GPU; keys are revocable and shown once.
- **Relay trust and availability** → user chooses and can move the relay; sessions are memory-only; TLS is mandatory publicly; application-level E2EE is a future hardening step.

## Open questions (defaults in use)
1. Ollama as bundled default endpoint confirmed; LM Studio/vLLM/llama.cpp = change settings (done by design).
2. Memory DB: built-in SQLite default + Postgres as the "other database" — if you had DuckDB or another engine in mind, say so before M3.
3. `bw` CLI in v0.1 (nice-to-have, cheap, proves headless core) — keep or drop?
4. **Sharing transport:** self-hosted relay is the only mode; its origin is selected in Settings and can be changed without rebuilding the app.
5. Do your guests mostly run Blackwall too, or will most be plain OpenAI-compatible clients? Both are first-class (deep link + plain snippet), this only affects where I lean the UX copy.

## Definition of done (v0.1.0)
`cargo test` + `clippy -D warnings` + `cargo deny` + vitest all green · clean-volume `.dmg` install works · e2e: unlock → open project → agent uses shell+files+web+subagent with approvals → memory/skills persist across relaunch · resume works · theme change = one file · sharing e2e: guest on another network connects through the hosted relay, host pins model, usage accrues, and disconnect is enforced.
