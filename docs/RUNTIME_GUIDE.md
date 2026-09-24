# Using the current Blackwall development build

Updated 2026-09-10. This guide describes the code in this checkout. It does not imply that a
published DMG contains these changes. See [delivery status](DELIVERY_PLAN.md) before release.

## Connect a model

1. Open **Settings → Set up a connection**. First launch opens setup automatically when no working
   model connection is found.
2. Choose **Use this computer**. Blackwall checks fixed loopback addresses for Ollama and LM Studio.
3. If a model is installed, choose **Use these models**. If Ollama is missing, choose **Get Ollama**,
   install and open it, then return and choose **Check again**. Blackwall does not install or start
   the model service itself.
4. If Ollama has no models, choose a starter model and **Download and connect**. The download requires
   internet and disk space; progress and a stop control remain visible. Starter models support text.
   Select a vision-capable model for image questions.
5. Choose **Start chatting**. The model selector in the top bar switches among available models.

**Connect a computer** offers persistent pairing. On the model host, open Settings, choose
**Pair another computer**, and create an invitation for the selected model. Paste it on the client,
compare both confirmation codes, and approve on the host. The saved computer's **Connect** button
checks model discovery before replacing your current connection. Later launches reuse its Keychain
credential. Keep Blackwall open, unlocked, and awake on the host with its model service running.
A configured relay is currently required; no default Blackwall-operated relay is provided yet.

Direct model access remains under advanced setup. Save a reachable OpenAI-compatible address and a
friendly name after discovery succeeds. Each connection remembers its selected model. Pairing and
direct connections do not start or reconfigure the model service automatically.

For an authenticated service, expand **Access key**. The native app tests the key before saving it
in macOS Keychain, scoped to the service origin. An empty setup field preserves the existing key.
**Settings → Access keys** shows whether the current model service or configured relay has a saved key.
Replace a model key with **Verify and save key**, or remove a stored key after reviewing the confirmation.
To replace a relay key, supply the new token when creating a guest link; it is saved after registration.
Removal affects future requests, not already-running requests or invitations. Revoke guest links
separately. Environment-variable credentials are managed outside this screen. Never put a key in a URL.

**I have an invitation** opens a full HTTPS Blackwall invitation in the default browser. It does not
save the invite secret or add a permanent computer. An expired or revoked invitation needs replacement
by the sender. Guest sharing requires a configured relay; see [sharing](SHARING.md).

## Share with guests

In Settings, give the guest link an optional name, choose an expiry, and create the link. Copy it or
let the guest scan the QR code. You can keep four links active, each with its own access and expiry.
Choose **Create another guest link** for another person. The active-link list shows names, models,
expiry, and request counts. **Revoke** ends only that link; **Stop sharing** revokes all links.

The relay address is remembered locally. A supplied relay token is saved to macOS Keychain after
successful registration and reused for that relay origin. Guest links stay only in the app's memory;
if a window loses its access code, revoke that individual link and create a replacement.

## Work in a project

Chat mode sends messages and attachments to your selected model. Agent mode additionally offers
project tools. Choose **Agent** and select a project using the native folder picker. The chosen folder
is the authority for file tools; a folder must be selected again after reopening or locking the app.

| Action | Behavior |
| --- | --- |
| Read a file | UTF-8 text, at most 64 KiB, within the selected folder |
| List files | At most 500 directory entries |
| Search files | Literal text search; at most 2,000 entries, 8 MiB scanned, 100 matches |
| Edit a file | Shows a unified diff; writes only after approval; rejects stale source contents |
| Run a command | Shows the exact shell command and working folder; requires approval |
| Read/search the web | Requires the Web access toggle and a separate request approval |
| Delegate an investigation | Read-only child; no commands, edits, web, or further delegation |

For each proposed change, choose **Deny**, **Allow once**, or **Allow identical action this run**.
The last option remembers only the exact action until the current run ends. It does not grant a
permanent project or domain policy. Expired and cancelled approval handles cannot authorize actions.

Shell commands run as your macOS account and are not sandboxed. Their output is bounded, they have a
two-minute limit, and cancellation terminates their process group. Review the command itself when
deciding; the selected project folder does not constrain what an approved shell command can access.

Use **Stop** or Escape to interrupt the run. Child tasks stop with their parent. The agent stops after
20 model/tool iterations; each child stops after eight. Pure child batches have a maximum concurrency
of three. The selected model must support OpenAI-compatible tool calls for Agent mode to work well.

Tool entries are collapsed by default and can be expanded to inspect results. Restored transcripts
show past actions without executing them again. A fresh model turn currently receives the prior prose
and attachments; historical tool-call protocol messages are not replayed across separate turns.

## Conversations and saved data

The desktop app keeps up to 500 conversations in `~/.blackwall/blackwall.sqlite3`, including supported
attachment contents. The old browser recent-chat list is migrated transactionally once and retained
as a fallback copy. Browser-only development keeps its smaller, metadata-oriented localStorage history.

Use the delete control beside a conversation to remove it. **Export chat** writes JSON containing the
conversation, tool history, and retained attachments to a location selected through the native dialog.
Exports may contain private conversation material and should be stored where you intend.

If a save fails, Blackwall displays **Retry saving** and **Export chat**. Pending operations retain
order, including deletions. New turns pause until saving recovers. Keep the app open while retrying or
exporting. Locking will not clear unsaved conversation state while writes remain pending. A process
crash can still lose work since the last successful save; there is no continuous draft journal.

## Memory and skills

**Memory** contains only facts you explicitly save in that panel. Enable **Use saved memories in chat**
to include a bounded selection in future chat and agent requests. Memory is off by default. Search,
edit, and delete are available; automatic fact extraction and a Postgres memory adapter are not included.
The advanced context-window setting changes the displayed estimate, not the model server's configured
context length or a guaranteed token budget.

**Skills** manages reusable instructions stored as `~/.blackwall/skills/<name>.md`. Create, edit,
enable/disable, and delete skills in the panel. Two starter workflows are seeded once:
`repo-orientation` and `bugfix-triage`. Removing one does not recreate it on the next launch.

```markdown
---
name: project-review
description: Review a project change before finishing.
enabled: true
---
Read the relevant project guidance. Inspect the diff, verify the behavior,
and report concrete findings with file paths.
```

Names use lowercase letters, numbers, and hyphens, up to 64 characters. A file must be valid UTF-8
Markdown with YAML frontmatter and fit within 32 KiB. Malformed skills are reported and skipped.
Enabled skills enter agent context when they fit the shared 8,000-byte budget.
They cannot grant tool permissions. Skills are currently managed by the user; agent-generated skill
creation and automatic learning are not implemented.

Guest chats never receive owner memory, skills, project tools, or local conversation history.

## App lock

Enable **App lock** in Settings with a passphrase of at least 10 characters. Blackwall stores a salted
Argon2id verifier in macOS Keychain. The app locks on reopening, and **Lock now** stops active work and
sharing after flushing pending conversation saves. Failed attempts incur a short delay, increasing
after repeated failures. Changing or removing the lock requires the current passphrase.

This is an access control for the desktop app. It does not encrypt the database, attachments, skills,
or exports, and does not stop the same macOS account from reading them or using the CLI. FileVault
protects the disk when the account/device is locked. There is no built-in forgotten-passphrase recovery
flow; do not enable the lock without retaining the passphrase securely.

## CLI

```sh
cargo run --manifest-path src/Cargo.toml -p bw -- run "Explain this project's entry points"
cargo run --manifest-path src/Cargo.toml -p bw -- sessions
cargo run --manifest-path src/Cargo.toml -p bw -- resume SESSION_ID "Continue the investigation"
```

The CLI uses the current folder as its workspace, the same core tools and approval rules, native
sessions, enabled memory, and skills. Type `yes` to authorize the exact displayed action once;
other input denies it. Ctrl+C stops the run. Web access is disabled in the CLI. Successful answers
are saved; interrupted CLI runs do not yet persist a partial transcript.

Set `BLACKWALL_MODEL`, `BLACKWALL_MODEL_ENDPOINT` (or `OLLAMA_HOST`), and optionally
`BLACKWALL_MODEL_API_KEY`. The CLI currently uses environment model credentials, not the desktop's
Keychain connection selection. `bw request "prompt"` prints a protocol preview without calling a model.

## Release and remote-access gates

A release still needs native Keychain/dialog/lock testing in a packaged app, clean-machine installation,
Apple signing/notarization, a signed-updater upgrade test, and model-tool compatibility checks.

Persistent pairing is implemented with a configured relay. Remote release gates still include
packaged host-consent/revocation checks, an operated default relay, and two-computer tests across
separate networks. TLS protects transport; application-level end-to-end encryption is
not implemented. [The delivery plan](DELIVERY_PLAN.md) tracks these as open work.
