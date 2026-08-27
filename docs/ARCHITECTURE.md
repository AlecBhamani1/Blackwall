# Blackwall architecture

This document describes the boundary implemented by the first desktop vertical slice and the
security contract for its first QR/link browser-sharing slice. Future agent, memory, tool, and
expanded sharing systems are specified in `PLAN.md` and build on these contracts.

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

The desktop process never starts, stops, or reconfigures a model server. It is a client of an
already-running endpoint. Endpoint selection is, in order:

1. an explicit trusted command request;
2. `BLACKWALL_MODEL_ENDPOINT`;
3. `OLLAMA_HOST`; or
4. the implementation-plan fallback `http://localhost:11434/v1`.

Origins without a path receive `/v1`. Only HTTP and HTTPS are accepted, and embedded URL
credentials are rejected. Optional Bearer authentication comes from `BLACKWALL_MODEL_API_KEY` and
is not persisted by this scaffold.

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
requires no app rebuild. `BLACKWALL_RELAY_TOKEN` optionally supplies the deployment-wide host
registration credential. Public origins must use HTTPS/WSS, with loopback HTTP/WS allowed only for
development. The relay binary listens on a configurable address and is intended to sit behind a
TLS reverse proxy; the included Docker/Caddy deployment is described in [hosted guest
sharing](SHARING.md).

The desktop receives relayed guest bodies over its outbound socket, pins the model a second time,
and reaches the model through `BLACKWALL_MODEL_ENDPOINT`, then `OLLAMA_HOST`, with localhost only as
a final fallback. The relay and guest never receive the upstream URL or model credential. A dropped
host socket is re-established with the same random host key while the invite remains active.

### Invite authentication

Creating a guest link creates independent random session, host, and guest credentials. The guest
credential is a 256-bit secret encoded in the URL fragment; the relay receives only its salted
digest during registration. The guest page clears the fragment from browser history, retains the
key in memory, and sends it as an `Authorization: Bearer` credential. The relay token and host key
are never placed in a guest URL.

The relay pins the owner's selected model before forwarding, and the desktop pins it again before
calling the upstream. One invite allows at most `2` guest chat requests in flight. Expiry, the
owner's **Stop sharing** action, or loss of the host session invalidates the public route. Relay
session state is memory-only and is cleared by a relay restart.

Authorization and the two-request permit are extracted before Axum buffers the request body, so an
invalid or over-capacity caller cannot consume the 32 MiB body allowance. The browser measures the
exact serialized UTF-8 JSON before sending and trims oldest complete turns when accumulated image
history would cross that limit; the relay and desktop independently enforce request and response
limits.

Anyone who obtains an unexpired invite URL has its authority. HTTPS/WSS protects traffic in transit,
but the first protocol is not application-level end-to-end encrypted, so the self-hosted relay is
inside the trust boundary and can observe forwarded chat content. See [hosted guest
sharing](SHARING.md) for deployment and the complete threat model.

## Package boundaries

- `ui/` owns presentation, interaction, local conversation summaries, attachment preparation, and
  browser-development transport. It has no access to model-host credentials in the native app.
- `src/app/` owns the Tauri shell, network client, endpoint policy, response limits, OpenAI-compatible
  serialization, and translation into typed UI events.
- `src/core/` owns framework-independent protocol types and validation limits. It cannot depend on
  Tauri or the Svelte application.
- `src/relay/` owns the separately deployable public HTTP/WebSocket edge and keeps sessions only in
  memory.
- `src/bw/` is the thin command-line adapter reserved by the plan.

All native streaming events use the `blackwall://event` channel and carry a request identifier.
The UI ignores unrelated or stale streams and can abort its active request.

## Attachments

The UI accepts at most eight attachments, 15 MiB each and 30 MiB combined. Images are sent as data
URLs to vision-capable endpoints. Small supported text files are decoded and enclosed as attachment
context. Other files retain their name, media type, and size, while their opaque binary contents are
not forwarded to a text-only model in this milestone. The Rust protocol independently enforces the
same count and byte limits at the IPC boundary.

Image preview object URLs are short-lived and revoked when removed or when a conversation is
released. Persisted recent conversations contain attachment metadata, not attachment bytes.

## Current persistence and trust model

Recent conversation text is stored in the desktop webview's local storage for this vertical slice.
Durable SQLite sessions, passphrase protection, Keychain-backed secrets, workspace-scoped tools,
and audit records land in later milestones.

The configured model machine and the self-hosted relay are inside the user's trust boundary. Do not
expose the raw model listener to the public internet. The relay keeps endpoint credentials and the
raw model address hidden behind an authenticated, expiring invite, but it can observe the chat
content it forwards.

This is not yet a multi-tenant hosted service. Application-level end-to-end encryption, multiple
named guests, persisted per-guest telemetry, and signed/notarized public desktop distribution remain
later milestones.
