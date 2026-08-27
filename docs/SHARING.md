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
export BLACKWALL_RELAY_TOKEN='replace-with-a-long-random-secret'
cargo run --release --manifest-path src/Cargo.toml --package blackwall-relay
```

Put a TLS reverse proxy that supports WebSocket upgrades in front of `127.0.0.1:8787`. Public relay
URLs must use HTTPS; plain HTTP is accepted only for loopback development. The relay token is
optional at the process level, but a public deployment should always set one so strangers cannot
register sessions and consume relay capacity.

## Configure the Mac

Open **Settings → Share your model** and enter:

- **Hosted relay URL:** the origin only, such as `https://relay.example.com`. It is saved locally
  and can be changed whenever the relay moves.
- **Relay token:** the value configured on the relay. It is held only for the current Blackwall
  process and is never placed in a guest URL.

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
5. Choose **Stop sharing** to close the host connection and remove the session from the relay.

The desktop automatically reconnects an interrupted outbound connection while the invite remains
active. Guests may see a temporary unavailable response during that reconnect window. Relay
restarts clear all sessions; create a new invite if the relay process is restarted.

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
in memory, and sends it in the `Authorization` header. The relay never writes sessions, keys,
prompts, or responses to disk.

## Security and limits

- The relay forces the owner-selected model on both the public and host sides.
- An invite permits at most two guest chat requests in flight.
- Guest request bodies are limited to 32 MiB and model responses to 64 MiB.
- Invitations expire after at most seven days and are removed immediately when the host stops or
  disconnects without reconnecting.
- The model endpoint and its API key remain on the Mac and are never sent to the relay or guest.
- The deployment token authorizes hosts to register; the invite key separately authorizes guests.
- Public deployments must use TLS. Do not expose the relay's plain HTTP listener directly.

The first relay protocol is encrypted in transit by HTTPS/WSS but is not end-to-end encrypted at
the application layer. A relay operator can observe guest request and response contents while
forwarding them. Run the relay on infrastructure you trust. Application-level end-to-end
encryption and named guest accounts remain future work.
