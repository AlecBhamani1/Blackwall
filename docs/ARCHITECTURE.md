# Blackwall architecture

This document describes the desktop chat, project agent, durable local data, and guest-sharing
boundaries. `DELIVERY_PLAN.md` records verified work and remaining product/release gaps.

## Runtime flow

```text
Svelte UI
  ├─ discover_models ──────────────┐
  └─ stream_chat + attachments ────┤ Tauri IPC
                                    ▼
                             blackwall-app
                               ├─ validates typed requests
                               ├─ normalizes the endpoint
                               ├─ applies time/size bounds
                               └─ emits blackwall://event
                                    │
                                    ▼
                          OpenAI-compatible endpoint
                          on a user-controlled machine
```

The desktop connects to an already-running model service. Guided setup checks fixed loopback
addresses and can explicitly request an allowlisted Ollama model download; it does not install or
start the server software. Endpoint selection is, in order:

1. an explicit trusted command request;
2. `BLACKWALL_MODEL_ENDPOINT`;
3. `OLLAMA_HOST`; or
4. the implementation-plan fallback `http://localhost:11434/v1`.

Origins without a path receive `/v1`. Only HTTP and HTTPS are accepted, and embedded URL
credentials are rejected. Model keys are resolved from macOS Keychain by normalized origin. `BLACKWALL_MODEL_API_KEY` is
a fallback only for its configured origin. Model clients disable redirects to avoid forwarding
credentials to another service. New keys are tested before replacing an existing Keychain entry.

## Guest share boundary

Guest sharing uses a separately deployable relay; it does not expose the Mac or upstream
OpenAI-compatible server directly. The desktop initiates the only connection from the owner side.

```text
Owner desktop ───── outbound WSS ─────► hosted relay ◄──── HTTPS ──── Guest browser
      │                                      │             Bearer invite key
      ▼                                      │
pinned OpenAI-compatible model               └──── streamed response ────► guest
on the configured machine
```

The relay origin comes from the saved Settings value or `BLACKWALL_RELAY_URL`; moving the service
requires no app rebuild. Registration tokens resolve from explicit input, origin-scoped Keychain,
then an origin-scoped `BLACKWALL_RELAY_TOKEN` fallback. Supplied tokens are saved after successful
registration. Settings exposes credential presence, verified model-key replacement, and deletion
without returning saved secret values to the UI. Public origins must use HTTPS/WSS, with loopback HTTP/WS allowed only for
development. The relay binary listens on a configurable address and is intended to sit behind a
TLS reverse proxy; the included Docker/Caddy deployment is described in [hosted guest
sharing](SHARING.md).

The desktop receives relayed guest bodies over its outbound socket, pins the model a second time,
and reaches the model through the endpoint selected when the invitation was created, with the
same environment/default fallback order as owner chat. The relay and guest never receive the upstream URL or model credential. A dropped
host socket is re-established with the same random host key while the invite remains active.

### Invite authentication

Creating a guest link creates independent random session, host, and guest credentials. The guest
credential is a 256-bit secret encoded in the URL fragment; the relay receives only its salted
digest during registration. The guest page clears the fragment from browser history, retains the
key in memory, and sends it as an `Authorization: Bearer` credential. The relay token and host key
are never placed in a guest URL.

The relay pins the owner's selected model before forwarding, and the desktop pins it again before
calling the upstream. The desktop manages up to four named invitations with separate credentials
and individual revocation. Each invite allows at most `2` guest chat requests in flight (eight per desktop). Expiry, the
owner's per-link **Revoke** action, **Stop sharing** for all links, or loss of the host session invalidates
the affected public route. Active relay sessions are memory-only. Public address ownership is
persisted separately as host-credential digests, preventing another host from claiming an offline
or previously used address. The desktop restores its original unexpired invitation after a relay
process restart. A private SQLite database and the Compose `relay_data` volume retain ownership;
no raw credentials, prompts, or responses are written there. See `SHARING.md` for capacity and
migration requirements.

Authorization and the two-request permit are extracted before Axum buffers the request body, so an
invalid or over-capacity caller cannot consume the 32 MiB body allowance. The browser measures the
exact serialized UTF-8 JSON before sending and trims oldest complete turns when accumulated image
history would cross that limit; the relay and desktop independently enforce request and response
limits. Host loss before headers returns a readable 502 JSON error; a midstream failure terminates
the stream. Abandoning a response releases its pending entry and request permit. Disconnect error
delivery is bounded so a guest that stops reading cannot indefinitely block revocation.

Anyone who obtains an unexpired invite URL has its authority. HTTPS/WSS protects traffic in transit,
but the first protocol is not application-level end-to-end encrypted, so the self-hosted relay is
inside the trust boundary and can observe forwarded chat content. See [hosted guest
sharing](SHARING.md) for deployment and the complete threat model.

## Package boundaries

- `ui/` owns presentation, interaction, local conversation summaries, attachment preparation, and
  browser-development transport. User-entered credential drafts exist in input controls; saved
  Keychain values are never returned to the UI.
- `src/app/` owns the Tauri shell, network client, endpoint policy, response limits, OpenAI-compatible
  serialization, and translation into typed UI events.
- `src/core/` owns the model transport, bounded agent/tool runtime, approvals, storage, memory,
  skills, web policy, and sharing protocol. It does not depend on Tauri or Svelte.
- `src/relay/` owns the separately deployable public HTTP/WebSocket edge and keeps sessions only in
  memory.
- `src/bw/` runs and resumes core agent tasks from the command line with explicit terminal approvals.

Model/tool events use `blackwall://event`; setup download progress uses `blackwall://download`.
Both carry a request identifier.
The UI ignores unrelated or stale streams and can abort its active request.

## Attachments

The UI accepts at most eight attachments, 15 MiB each and 30 MiB combined. Images are sent as data
URLs to vision-capable endpoints. Small supported text files are decoded and enclosed as attachment
context. Other files retain their name, media type, and size, while their opaque binary contents are
not forwarded to a text-only model in this milestone. The Rust protocol independently enforces the
same count and byte limits at the IPC boundary.

Image preview object URLs are short-lived and revoked when removed or when a conversation is
released. Native sessions retain supported image/text payloads; opaque unsupported binary file contents
are not stored. Browser-only development history keeps metadata without the full attachment payload.

## Current persistence and trust model

Native history, preferences, and user-approved memory use versioned SQLite. Markdown skills live
in a restricted local directory. Keychain holds model/relay secrets and the optional app-lock verifier.
The data lifecycle and lock limits are described below.

The configured model machine and the self-hosted relay are inside the user's trust boundary. Do not
expose the raw model listener to the public internet. The relay keeps endpoint credentials and the
raw model address hidden behind an authenticated, expiring invite, but it can observe the chat
content it forwards.

Persistent pairing uses typed SQLite device records, per-address native Keychain services,
five-minute host-approved exchanges, and renewable host tunnels. Permanent relay revocation
markers reject stale reconnects. Pending approval records cannot start tunnels. One shared semaphore
limits guest and paired model work to eight requests per desktop process.

Native two-computer pairing acceptance, application-level end-to-end encryption, an operated default
relay, persisted guest telemetry, and signed/notarized public desktop distribution remain open. Named
temporary invitations are implemented; they are not permanent guest accounts.

## Agent execution and authority

`core::model` decodes bounded OpenAI-compatible streamed content and fragmented tool calls.
`core::agent` runs at most 20 model/tool iterations. File tools use a directory capability selected
through the native folder picker; absolute paths, parent traversal, `.git`, and escapes outside the
project are rejected. Literal search is bounded and skips dependency folders. Edits display a unified
diff and compare the original file again before an atomic replacement.

Shell actions display the exact command and working directory. They run with the user's account
permissions, **without an OS sandbox**. Each command has a two-minute deadline, separate 64 KiB
stdout/stderr limits, and a process-group guard that kills descendants on cancellation. Known model,
relay, and signing secrets are removed from the inherited environment. Other account access remains
possible and is covered by the explicit command approval.

`core::approvals` binds decisions to an active request and exact action. “Allow identical action this
run” expires when the run ends. Dropping a pending action invalidates its decision handle. Child
investigations are read-only, limited to eight turns, and cannot execute commands, browse, edit, or
spawn more children. Pure child batches run up to three concurrently; mutations remain sequential.

Web access is disabled by default. Every request and redirect needs approval. The client disables
proxies and automatic redirects, validates all resolved IPs, pins DNS results, rejects private and
reserved ranges, and bounds response time and body size. Retrieved content is labeled untrusted.
The current search implementation uses DuckDuckGo's HTML endpoint and can be unavailable or blocked.

## Local data and app lock

`~/.blackwall/blackwall.sqlite3` stores versioned sessions, preferences, and FTS5 memory. Native history
retains supported attachment payloads. Migration is transactional and marked once; old browser data
is retained as a fallback copy. Preferences patches use an immediate SQLite transaction. The UI queues
failed writes in order and exposes retry/export without discarding the pending operations.

`~/.blackwall/skills/*.md` contains validated YAML-frontmatter Markdown skills. Enabled instructions
enter agent context under a bounded budget. Only user-saved facts enter memory; automatic learning
and the optional Postgres adapter are not implemented. Guests never receive owner memory, skills,
workspace tools, or conversation history.

The optional desktop lock stores a salted Argon2id verifier in Keychain, rate-limits failed attempts,
and gates native data/model/tool commands. Locking cancels registered runs/downloads and revokes the
active shares; the UI flushes saves before clearing its conversation state. It does not encrypt the
SQLite database or protect against the same macOS account reading files or using `bw`. Native dialogs
recheck lock state after selection. Actions already accepted before a lock may finish; the lock is
not a filesystem or process isolation boundary.
