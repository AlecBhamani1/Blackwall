# Using the current Blackwall development build

Updated 2026-10-06. This guide describes the code in this checkout. It does not imply that a
published installer contains these changes. See [delivery status](DELIVERY_PLAN.md) before release.

Blackwall runs on macOS, Windows 10/11, and Linux (x86-64 and ARM64). Saved access keys, pairing
credentials, and the app-lock verifier use the platform credential store: Keychain on macOS,
Credential Manager on Windows, and the Secret Service (GNOME Keyring, KWallet, or a compatible
provider) on Linux. On Linux, start and unlock a keyring before saving keys; without one, Blackwall
reports that no system keyring is available and does not fall back to plain-text storage. Local data
lives in `.blackwall` in your home folder (`%USERPROFILE%\.blackwall` on Windows).

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
checks model discovery before replacing your current connection. Later launches reuse its stored
credential. The host and client may run different operating systems; pairing uses the same relay
protocol everywhere. Keep Blackwall open, unlocked, and awake on the host with its model service running.
A configured relay is currently required; no default Blackwall-operated relay is provided yet.

Direct model access remains under advanced setup. Save a reachable OpenAI-compatible address and a
friendly name after discovery succeeds. Each connection remembers its selected model. Pairing and
direct connections do not start or reconfigure the model service automatically.

For an authenticated service, expand **Access key**. The native app tests the key before saving it
in the platform credential store, scoped to the service origin. An empty setup field preserves the existing key.
**Settings → Access keys** shows whether the current model service or configured relay has a saved key.
Replace a model key with **Verify and save key**, or remove a stored key after reviewing the confirmation.
To replace a relay key, supply the new token when creating a guest link; it is saved after registration.
Removal affects future requests, not already-running requests or invitations. Revoke guest links
separately. Environment-variable credentials are managed outside this screen. Never put a key in a URL.

**I have an invitation** opens a full HTTPS Blackwall invitation in the default browser. It does not
save the invite secret or add a permanent computer. An expired or revoked invitation needs replacement
by the sender. Guest sharing requires a configured relay; see [sharing](SHARING.md).

## Attach and reference files

Drop screenshots or files anywhere in the conversation, choose files with the paperclip button,
or paste an image into the message box. Files appear as attachments ready to send with your next
message. Wait for the current response to finish before adding files. Image questions require a
vision-capable model.

Type `@` to see files from the current conversation and the pending attachments. Type part of the
filename to filter the list, then click a file or use the arrow keys and Enter or Tab to select it.
Escape closes the list. Press Enter again to send. Names with spaces are quoted automatically,
such as `@"Screenshot 2026-09-30.png"`; distinct files with the same name get numbered choices.
Referencing a previously sent file includes its retained contents with the new message.

Each message allows eight attached and referenced files, at most 15 MB per file and 30 MB total.
Native saved conversations retain file contents for later references. If an older conversation
only retained file metadata, Blackwall asks you to reattach the file before referencing it.

Browser screenshots of the development UI, using a deterministic model fixture:
[conversation drop target](screenshots/chat-file-drop.png) and
[file reference picker](screenshots/chat-file-references.png). These demonstrate the browser flow;
Finder and macOS screenshot-thumbnail drops still require packaged native acceptance.

## Budget and compact conversations

Open **Memory → Advanced · Context window** to set the context limit configured in your selected
model service and the output reserve. Settings apply to the selected connection's future requests;
check the limit when switching models. Defaults are 32,000 context tokens, up to 4,096 generated
tokens, and automatic compaction off. The browser development UI also saves these controls locally.

Type `/context` in the composer or CLI to see estimated prompt and tool-definition tokens, the
output reserve, the 5% safety reserve, and separately labeled server-reported usage when available.
Server usage describes the last conversation request (excluding summary calls), rather than a
running total or an exact measurement of the current checkpoint. Estimates use serialized UTF-8
text bytes divided by three plus message framing, including text attachments. Image inputs use a
4,096-token estimate per image rather than counting encoded image bytes as text. They are not tokenizer
counts and cannot guarantee that every model accepts a request. The configured budget supplements
the existing serialized size ceilings; output requests send an explicit `max_tokens` cap.

Type `/compact` to summarize older assistant/tool context. The two most recent user turns, every
user requirement and correction, initial instructions, and current memory/skill instructions stay
intact. Older assistant/tool material becomes a structured checkpoint covering requirements,
corrections, decisions, changed files, checks, unfinished work, and relevant context. Completed
recent tool-call/result groups remain intact. Optional automatic compaction attempts this at 90%
of the available prompt budget, including tool definitions. Large retained instructions, attachments,
or recent turns can still exceed the budget; shorten them or select an appropriate configured limit.

The full visible conversation and original model transcript remain saved. Checkpoints live in a
separate `contextState.checkpoints` array in the saved session record and resume through the same
transactional session store; no database schema migration is needed. Checkpoint hashes bind them
to their original transcript prefix. Session exports include both originals and checkpoints.
Summaries run without tools. Historical actions are data, never executed during resume, and approval
rules are scoped to their original run. Invalid, failed, or cancelled summaries leave the previous
context intact. Existing saved conversations without checkpoints continue normally.

CLI equivalents:

```sh
bw config set context 32000
bw config set output 4096
bw config set compact on
```

During interactive CLI chat, use `/settings context <tokens>`, `/settings output <tokens>`, and
`/settings compact on|off`. `/compact` does not add a user or assistant message. Ctrl+C cancels
summary generation. The original transcript remains subject to the conversation store's size limit;
compaction releases model prompt space, not disk space.

Scripted browser verification: [budget controls](screenshots/context-budget.png) and
[separate estimates and server usage](screenshots/context-usage.png).

## Share with guests

In Settings, give the guest link an optional name, choose an expiry, and create the link. Copy it or
let the guest scan the QR code. You can keep four links active, each with its own access and expiry.
Choose **Create another guest link** for another person. The active-link list shows names, models,
expiry, and request counts. **Revoke** ends only that link; **Stop sharing** revokes all links.

The relay address is remembered locally. A supplied relay token is saved to the platform credential store after
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

Shell commands run as your user account and are not sandboxed. macOS and Linux run them with
`/bin/sh`; Windows runs them in Windows PowerShell. Their output is bounded, they have a two-minute
limit, and cancellation terminates their process group or Windows Job Object, including children
whose parent shell has already exited. Review the command itself when
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
Argon2id verifier in the platform credential store. The app locks on reopening, and **Lock now** stops active work and
sharing after flushing pending conversation saves. Failed attempts incur a short delay, increasing
after repeated failures. Changing or removing the lock requires the current passphrase.

This is an access control for the desktop app. It does not encrypt the database, attachments, skills,
or exports, and does not stop the same user account from reading them or using the CLI. Disk
encryption (FileVault, BitLocker, or LUKS) protects the disk when the account/device is locked. There is no built-in forgotten-passphrase recovery
flow; do not enable the lock without retaining the passphrase securely.

## CLI

```sh
cargo install --locked --path src/bw
bw setup
bw
bw run "Explain this project's entry points"
bw sessions
bw resume SESSION_ID
bw resume SESSION_ID "Continue the investigation"
```

`bw` or `bw chat` opens an interactive conversation in the current folder. Setup asks for your
already-running model service address and model ID; it saves all choices together after valid input.
Setup does not test connectivity or launch/download a model. `npm run cli -- setup` and
`npm run cli` provide the same commands from a development checkout.

| Interactive command | Behavior |
| --- | --- |
| `/commands`, `/help` | List commands and their arguments |
| `/settings` | Show effective settings and the current project |
| `/settings <name> <value>` | Save a model, endpoint, web, or memory setting |
| `/model <id>`, `/endpoint <url>` | Change the model or service for the next turn |
| `/web on\|off`, `/memory on\|off` | Enable or disable web tools or saved memory injection |
| `/workspace <path>` | Start a new conversation in another project; relative paths use the current project |
| `/new` | Start a new conversation in the current project |
| `/sessions`, `/resume <id>` | List conversations or restore an Agent conversation and its project |
| `/quit`, `/exit` | Exit; EOF also exits |

Type `//` to begin a literal message with `/`. Use `bw commands` to see the catalog outside chat.
`bw config show` reviews settings, and `bw config set <name> <value>` changes them without entering
chat. Saved CLI choices take precedence over saved desktop preferences, then `BLACKWALL_MODEL`,
`BLACKWALL_MODEL_ENDPOINT` (or `OLLAMA_HOST`), then the initial `llama3.2` and local Ollama defaults.
CLI settings are separate from desktop preferences. They are stored in the same local SQLite
database; `BLACKWALL_DATA_DIR` selects a different data folder, including its memory and skills.

For an authenticated service, set `BLACKWALL_MODEL_API_KEY` and the matching
`BLACKWALL_MODEL_ENDPOINT` (or `OLLAMA_HOST`). Environment keys are sent only to that origin, even
after `/endpoint` changes. The CLI does not store keys or use desktop credential-store entries.
Never put a key in an endpoint URL. `bw request "prompt"` prints a protocol preview without a model call.

The CLI shares core tools, approval rules, enabled memory, skills, and native sessions with desktop.
Type `yes` to authorize the displayed action once; other input or EOF denies it. Web tools are off
initially and every enabled web request still needs approval. Ctrl+C stops the active turn and
returns to chat; at the idle prompt it exits. User turns are saved before work starts, and partial
answers are retained on graceful cancellation or errors. A crash can still lose streamed text.
New sessions record Agent mode and their project. Resume restores that project even when launched
elsewhere; desktop Chat conversations cannot be resumed with CLI tools. Old `cli_` sessions without
project metadata use the current project. Saved transcript prose and attachments are reused; past
tool-call protocol messages are not replayed. The desktop app lock does not restrict the CLI.

## Release and remote-access gates

A release still needs native credential-store/dialog/lock testing in a packaged app on each
platform, clean-machine installation, Apple signing/notarization, Windows code signing, a signed-updater upgrade test, and model-tool compatibility checks.

Persistent pairing is implemented with a configured relay. Remote release gates still include
packaged host-consent/revocation checks, an operated default relay, and two-computer tests across
separate networks. TLS protects transport; application-level end-to-end encryption is
not implemented. [The delivery plan](DELIVERY_PLAN.md) tracks these as open work.
