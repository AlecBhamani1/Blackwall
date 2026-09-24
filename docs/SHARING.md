# Hosted guest sharing

Blackwall publishes temporary browser chats through a relay that runs on a separate, publicly
reachable machine. The Mac never accepts an inbound guest connection: it opens an outbound
WebSocket to the relay, receives authenticated guest requests there, sends them to the configured
OpenAI-compatible model, and streams responses back over the same connection.

The relay address is configuration, not a build-time constant. Moving to another server only
requires changing **Settings → Share your model → Hosted relay URL**. The saved URL applies to
future shares; an active share remains attached to the relay where it was created.

## Deploy the relay

The included deployment runs `blackwall-relay` behind Caddy so public traffic uses HTTPS/WSS.
It expects a domain whose DNS points to the relay machine and inbound ports 80 and 443.

```sh
cd deploy/relay
cp .env.example .env
# Edit .env with your domain and a long random registration token.
docker compose up --detach --build

curl https://relay.example.com/health
```

The health response should be `{"status":"ok"}`. Caddy obtains and renews the TLS certificate and
proxies both HTTPS requests and WebSocket upgrades to the relay container.

To run the relay binary without Docker:

```sh
export BLACKWALL_RELAY_BIND=127.0.0.1:8787
export BLACKWALL_RELAY_DATA_DIR=./blackwall-relay-data
export BLACKWALL_RELAY_TOKEN='replace-with-a-long-random-secret'
cargo run --release --manifest-path src/Cargo.toml --package blackwall-relay
```

Put a TLS reverse proxy that supports WebSocket upgrades in front of `127.0.0.1:8787`. Public relay
URLs must use HTTPS; plain HTTP is accepted only for loopback development. The relay token is
optional at the process level, but a public deployment should always set one so strangers cannot
register sessions and consume relay capacity.

## Verify a relay image locally

With Node 22.12 or newer and Docker running, execute from the repository root:

```sh
docker build --tag blackwall-relay:review --file deploy/relay/Dockerfile .
node deploy/relay/smoke.mjs blackwall-relay:review
```

The smoke check creates a uniquely named container and temporary volume, publishes only a random
loopback port, verifies non-root execution and private storage, registers an invitation, and
replaces the container while preserving its volume. It then verifies that an impostor is rejected,
the original owner can reconnect, and raw credentials are absent from the database. Its temporary
container and volume are removed afterward. This does not deploy a public relay or test public TLS.

## Configure the Mac

Open **Settings → Share your model** and enter:

- **Hosted relay URL:** the origin only, such as `https://relay.example.com`. It is saved locally
  and can be changed whenever the relay moves.
- **Relay token:** the value configured on the relay. After successful registration it is saved
  to macOS Keychain for this relay origin and reused when the input is blank. It is never placed
  in a guest URL. Manage removal under **Settings → Access keys**.

The same values can be supplied when launching from a terminal:

```sh
export BLACKWALL_RELAY_URL=https://relay.example.com
export BLACKWALL_RELAY_TOKEN='the-token-from-the-relay-server'
npm run desktop
```

Values entered in Settings take precedence. Blackwall must remain open and able to reach both the
relay and the model endpoint while guests are chatting.

## Owner flow

1. Confirm the pinned model and relay configuration under **Share your model**.
2. Select a 15-minute, 1-hour, or 8-hour expiry.
3. Choose **Create guest link**. Blackwall establishes the outbound relay session before showing
   the invite.
4. Send the link or QR code through a trusted channel.
5. Choose **Revoke** beside a named link to end its access, or **Stop sharing** to close all host
   connections and remove their sessions from the relay.

The desktop automatically reconnects an interrupted outbound connection while the invite remains
active. Guests may see a temporary unavailable response during that reconnect window. The host
also restores the same invitation after a relay process restart, provided the invite has not expired.
Reconnection never resends a chat prompt automatically. The host and relay exchange heartbeats
at 15-second intervals and stop a connection after 45 seconds without incoming traffic; writes
and blocked response queues have a five-second deadline. Real sleep/wake and network switching
still require device acceptance.

## Request flow

```text
Guest browser                         Hosted relay                   Owner Mac
      │                                    │                            │
      │  HTTPS + Bearer invite key         │                            │
      ├───────────────────────────────────►│                            │
      │                                    │  existing outbound WSS     │
      │                                    ├───────────────────────────►│
      │                                    │                            ├─ pinned model
      │                                    │◄───────────────────────────┤  streamed response
      │◄───────────────────────────────────┤                            │
```

Creating a share generates three independent random values on the Mac:

- a session identifier used in the public URL;
- a host key used only to authenticate reconnects; and
- a 256-bit guest key placed in the URL fragment as `#key=bw1_…`.

The relay receives only a salted digest of the guest key during registration. The guest page reads
the raw key from the fragment, immediately removes the fragment from browser history, keeps the key
in memory, and sends it in the `Authorization` header. The relay keeps active sessions and guest-key
digests in memory. It writes only public session identifiers and SHA-256 host-ownership digests to
its ownership database; raw host/guest keys, deployment tokens, prompts, and responses are not persisted.

### Durable ownership and operations

The binary opens `ownership.sqlite3` in `BLACKWALL_RELAY_DATA_DIR` (default:
`./blackwall-relay-data`). The directory is private (`0700`) and the database is `0600` on Unix.
Use a dedicated directory owned by the relay process. The Compose deployment mounts a named
`relay_data` volume at `/data` so container replacement preserves ownership. Startup fails if
storage cannot be opened or has an unsupported newer schema; registration fails if a claim cannot
be committed. There is no automatic fallback to an empty registry.

Ownership remains reserved after disconnect and expiry. Otherwise, somebody who knows an old
public invitation address could claim it for another host. The registry has a hard limit of
100,000 addresses; existing owners can reconnect at that limit, while new addresses are refused.
Monitor this limit and provision a reviewed migration before reaching it. Do not prune ownership
rows or discard the volume while reusing the public relay origin. Back up the database with SQLite's
backup facilities or while the relay is stopped. Do not run multiple relay replicas behind a load
balancer: active tunnels remain process-local, and shared ownership storage alone does not provide
routing between replicas.

Existing installations must preserve the new volume on future deployments. Addresses created by
an older relay before this registry existed cannot be recovered from that old process's memory after
it stops; retire old invitations and create fresh ones during migration. Durable address ownership
now also protects persistent paired addresses. Schema version 2 adds permanent revocation markers
and migrates the version 1 ownership registry without discarding reservations.

## Security and limits

- The relay forces the owner-selected model on both the public and host sides.
- An invite permits at most two guest chat requests in flight. The desktop permits four active
  named invites and four hosted paired computers. One process-wide budget permits at most eight
  combined guest/paired requests, so adding paired computers does not multiply model capacity.
- Guest request bodies are limited to 32 MiB and model responses to 64 MiB.
- Invitations expire after at most seven days. Expiry ends unfinished requests as well as new access.
  Host disconnection removes the live session; the ownership reservation remains protected.
- The model endpoint and its API key remain on the Mac and are never sent to the relay or guest.
- The deployment token authorizes hosts to register; the invite key separately authorizes guests.
- Public deployments must use TLS. Do not expose the relay's plain HTTP listener directly.

The first relay protocol is encrypted in transit by HTTPS/WSS but is not end-to-end encrypted at
the application layer. A relay operator can observe guest request and response contents while
forwarding them. Run the relay on infrastructure you trust. Application-level end-to-end
encryption and named guest accounts remain future work.

## Current desktop integration

Guided setup accepts a full invitation and opens it in the default browser without retaining its
secret. Persistent computer pairing uses its own desktop flow and per-device credentials.
Named direct model connections remain available through advanced setup.

The desktop resolves an upstream model key from macOS Keychain when starting a share. Environment
model and relay credentials are used only for their configured origin. Model redirects are disabled.
Locking Blackwall stops guest links and paired host tunnels. Guests never receive the owner's memory, skills, project
tools, saved conversations, or Keychain credentials.

The current UI still needs a relay origin and, where configured, a registration token. No default
Blackwall-operated relay is deployed by this change. Four named invitations can now be active together and revoked separately. **Stop sharing** revokes
every active invitation. Relay tokens are saved in Keychain after successful registration. Native two-computer acceptance and default relay operation remain open
in [the delivery plan](DELIVERY_PLAN.md).


## Persistent computer pairing

On the model host, use **Settings → Connect another computer → Pair another computer**. Give the
host a friendly name, review the selected model, and configure the relay under advanced settings.
On the client, choose **Connect a computer**, paste the complete pairing invitation, and name the
client. Compare the three-group confirmation code on both screens, check the confirmation box on
the host, and approve. The client saves the computer and offers **Connect** after approval.

The invitation expires after five minutes and binds to the first candidate. A candidate creates its
own random credential; the host and relay receive only its salted digest. The invitation cannot
be used to recover that credential or add another device after approval. Pairing permits model
requests only. The relay remains trusted with forwarded chat contents; there is no application-level
end-to-end encryption.

Both Macs store typed metadata in SQLite and independent raw secrets in native Keychain accounts
scoped to the full paired endpoint, including the unique device address. Two computers on the same
relay therefore have separate credentials. On relaunch or unlock, approved host records reconnect
using that same address and a renewable one-day registration lease. Reconnect uses bounded backoff
with jitter and never resends a prompt. The selected model is retained without silent substitution.

**Remove** on the host first records removal locally and stops the tunnel. The relay then commits
a permanent revocation marker and disconnects that device's active requests. If the relay is offline,
the card says removal is pending and retries; other computers and guest links remain available.
A stale host credential cannot reopen a revoked address after relay restart. A second removal of an
already revoked card deletes its local record and Keychain entry. Client-side removal deletes only
that Mac's saved connection and credential; host-side revocation is separate.

An interrupted approval remains a visible **Approval incomplete** record and never automatically
starts a tunnel. If the original exchange is still open, **Finish pairing** retries the same approval;
otherwise remove it and pair again. Lost client responses retain the same pending credential for
retry during the exchange lifetime. Definitively rejected new invitations can be replaced immediately.
Unfinished exchanges are memory-only and expire on relay restart; completed device records survive.

Pairing API version 1 is exposed under `/v1/pairing`, with a capability endpoint for older-relay
detection. Limits are 4 KiB request bodies, 256 exchanges, five wrong invitation guesses, and 1,200
exchange requests per minute across the deployment. The global cap works behind a TLS proxy without
trusting client-supplied address headers; public operators still need suitable edge abuse controls.
The desktop stores at most 20 device records and hosts at most four active/pending pairings. Removed
relay reservations remain within the permanent 100,000-address ledger capacity.
