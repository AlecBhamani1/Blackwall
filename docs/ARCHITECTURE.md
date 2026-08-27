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

Guest sharing adds a narrow HTTP gateway beside the desktop bridge; it does not expose the upstream
OpenAI-compatible server directly.

```text
Owner desktop
  └─ Settings (bottom-left) → Share your model → Create guest link
                                    │
                                    ├─ QR code ────────┐
                                    └─ Copy Link ──────┤ same browser URL
                                                       ▼
Guest browser ── Tailscale IPv4 or loopback ── share gateway :11435
       │               Authorization: Bearer <invite secret> │
       │                                                     ▼
       └──────────────── streamed chat ───────── pinned OpenAI-compatible model
                                                       on the configured machine
```

The gateway uses `BLACKWALL_SHARE_PORT` when set and otherwise listens on port `11435`. Its default
advertised host is the owner's Tailscale IPv4 address. If no Tailscale IPv4 is available, it falls
back to loopback; that fallback supports same-machine preview and is not reachable from a phone or
another computer. `BLACKWALL_SHARE_PUBLIC_URL` overrides the URL placed in the QR/link but does not
install or configure a proxy, Tailscale Serve, or Funnel.

On macOS, the exact-address listener is accepted through a bounded standard-socket compatibility
bridge into the loopback Axum server. This avoids a `kqueue` readiness issue observed with some
Tailscale `utun` interfaces while preserving the important boundary: the public socket is still
bound only to the selected Tailscale IPv4 address, never to a wildcard interface. The bridge caps
simultaneous connections, applies socket timeouts, atomically rejects late connections during
shutdown, closes both sides of every tracked relay, and joins its workers before sharing is
reported stopped.

The gateway reaches the model through `BLACKWALL_MODEL_ENDPOINT`, then `OLLAMA_HOST`, with the plan's
localhost value only as a final fallback. In the intended deployment the upstream may be on another
owner-controlled machine. Blackwall never launches a local Ollama process, and the guest never
receives the upstream URL, model credentials, or the full upstream model catalog; the authenticated
models route exposes only the host-pinned model.

### Invite authentication

Creating a guest link creates an expiring, cryptographically random 256-bit secret. The QR code and
copy-link control encode the same URL with that secret in the URL fragment. Fragments are not
included in the browser's initial HTTP request. The guest page reads the fragment once, immediately
clears it from the visible URL
and browser history entry, keeps it only in memory, and sends it as an `Authorization: Bearer`
credential on authenticated gateway requests.

The gateway pins the owner's selected model in server-side share state and ignores guest attempts
to select another model. One invite allows at most `2` guest chat requests in flight; excess work
is rejected as busy rather than queued without bound. Expiry and the owner's **Stop sharing** action
both invalidate the invite. Authentication state, guest prompts, and usage counters are not a
durable guest-identity or telemetry system in this slice.

Authorization and the two-request permit are extracted before Axum buffers the request body, so an
invalid or over-capacity caller cannot consume the 32 MiB body allowance. The browser measures the
exact serialized UTF-8 JSON before sending and trims oldest complete turns when accumulated image
history would cross that limit; the gateway independently enforces the same hard ceiling.

Anyone who obtains an unexpired invite URL has its authority. Tailscale supplies private network
reachability but does not replace the invite credential. The owner should revoke a link that was
forwarded, photographed, or otherwise exposed. See [browser guest sharing](SHARING.md) for the
complete first-slice flow and threat model.

## Package boundaries

- `ui/` owns presentation, interaction, local conversation summaries, attachment preparation, and
  browser-development transport. It has no access to model-host credentials in the native app.
- `src/app/` owns the Tauri shell, network client, endpoint policy, response limits, OpenAI-compatible
  serialization, and translation into typed UI events.
- `src/core/` owns framework-independent protocol types and validation limits. It cannot depend on
  Tauri or the Svelte application.
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

The configured model machine is inside the user's trust boundary. Do not expose its raw listener to
the public internet. The guest gateway keeps endpoint credentials and the raw model address hidden
behind an authenticated, expiring invite.

This is not yet a general public hosting service. Automatic Tailscale Serve/Funnel configuration,
multiple named guests, persisted per-guest telemetry, and signed/notarized public desktop
distribution remain later milestones.
