# Browser guest sharing

Blackwall's first sharing slice lets one owner hand an expiring browser chat to a guest without
giving that guest the upstream model URL or credentials. It is deliberately a small, revocable
capability—not an account system or public hosting platform. The surrounding process and trust
boundaries are diagrammed in [the architecture guide](ARCHITECTURE.md).

## Owner flow

1. Open **Settings** from the bottom-left of the Blackwall desktop app.
2. Under **Share your model**, confirm the model pinned for this invite and select a 15-minute,
   1-hour, or 8-hour expiry.
3. Select **Create guest link**. Blackwall starts the guest gateway and creates the invite.
4. Let the guest scan the QR code or select **Copy Link** and send it through a trusted channel.
   Both controls contain the exact same browser URL and grant the same authority.
5. Use **Stop sharing** when the conversation is over or immediately if the link may have leaked.

The gateway does not launch Ollama. It forwards authenticated guest chat to the same configured
OpenAI-compatible upstream used by the owner, which may run on a different machine.
Blackwall must remain open and connected while a guest is chatting.

## Network selection

The gateway's default port is `11435`. Blackwall advertises a Tailscale IPv4 address when the owner
machine has one. The guest device must be able to reach that address, normally by being on the same
tailnet.

Blackwall does not trust CGNAT addressing alone: it correlates the address on a local interface with
the current node reported by the read-only `tailscale status` command. If Tailscale is stopped,
unavailable, or reports a different address, sharing safely falls back to loopback. This status
check never runs `tailscale up`, Serve, Funnel, or any command that changes the tailnet.

When no Tailscale IPv4 is available, Blackwall falls back to a loopback URL. Loopback is suitable
for checking the guest page on the owner machine, but a QR code containing loopback will not work on
a phone or another computer—the address would point back to the guest device itself.

Environment controls:

| Variable | Purpose |
|---|---|
| `BLACKWALL_SHARE_PORT` | Gateway listen/advertise port; defaults to `11435`. |
| `BLACKWALL_SHARE_PUBLIC_URL` | Overrides the advertised browser base URL used by the QR and copied link. |
| `BLACKWALL_MODEL_ENDPOINT` | Preferred OpenAI-compatible upstream, including a model on another machine. |
| `OLLAMA_HOST` | Upstream fallback when `BLACKWALL_MODEL_ENDPOINT` is unset. |

`BLACKWALL_SHARE_PUBLIC_URL` is an advertisement override, not networking automation. The operator
must make that URL route to the gateway. Apart from the bounded read-only status check above, this
slice does not run Tailscale commands, create Serve or Funnel routes, configure DNS/TLS, or open
firewall ports.

## Invite and authentication flow

```text
Create guest link
    │
    ├─ generate 256 cryptographically random bits
    ├─ store expiring, revocable share state in the owner process
    └─ build one URL: http(s)://advertised-host:11435/guest#key=<secret>
                         │
                  QR and Copy Link
                         │
                         ▼
                  guest loads page
                         │  URL fragments are not sent in the HTTP request
                         ├─ read secret once
                         ├─ clear fragment with history.replaceState
                         └─ keep secret in page memory
                                  │
                                  ▼
                  Authorization: Bearer <secret>
                         on guest chat requests
```

The fragment keeps the invite secret out of the initial request target and ordinary server access
logs. The guest page must clear it before navigating or loading any nonessential resources. The
page should remain self-contained—no analytics, third-party scripts, fonts, or remote images that
could become another disclosure path. The Bearer value must never be logged.

The secret expires even if the owner forgets to stop sharing. **Stop sharing** revokes it
immediately and closes the share surface. Reloading after the fragment has been cleared does not
reconstruct the secret; the guest must use the original still-valid invite again.

## Model and resource boundaries

- The owner pins exactly one model when starting the share. The gateway applies that model
  server-side and does not honor a guest-supplied replacement.
- The guest does not receive model discovery results, the upstream URL, or upstream Bearer keys.
- At most `2` guest chat requests may be in flight for the invite. Additional requests receive a
  busy/rate-limit response instead of creating an unbounded queue.
- Authentication and the concurrency check happen before Blackwall reads a chat request body.
  Serialized requests are capped at 32 MiB; the guest page measures the actual UTF-8 JSON and drops
  the oldest complete conversation turns when accumulated image history would exceed that cap.
- The first slice is browser chat. It does not grant access to the owner's files, shell, agent
  tools, memory, skills, or desktop session history.
- Invite state is runtime-scoped. It is not a persisted audit log, billing record, or named guest
  profile.

## Threat model

The invite is a Bearer capability: anyone holding an unexpired copy can use it. A guest can forward
the link, and a screenshot of the QR code is also a copy. Use a trusted channel, keep expiry short
enough for the intended conversation, and revoke promptly after use or suspected disclosure.

Tailscale limits who can reach the default gateway address, subject to the owner's tailnet ACLs,
but possession of an invite remains the application-level authorization check. A public reverse
proxy or manually configured Funnel expands the reachable attack surface and should provide TLS;
it does not change the invite's Bearer semantics.

The gateway protects the raw model listener from guests, but guest prompts and model responses still
pass through the owner's Blackwall process and upstream model. The owner is responsible for the
upstream machine, its availability, and the data policy appropriate to invited users.

## Deliberately deferred

The following are later milestones and should not be inferred from the first slice:

- automatic Tailscale Serve or Funnel setup and teardown;
- multiple independently named guests or per-guest policy;
- persisted usage telemetry, transcripts, quotas, or audit history;
- a hosted relay or Blackwall cloud service; and
- signed, notarized public macOS distribution and automatic updates.
