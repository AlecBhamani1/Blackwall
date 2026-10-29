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

1. Open **Settings → Model connection → Set up a connection**. First launch opens setup automatically when no working
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

Direct model access remains under advanced setup. Enter a reachable OpenAI-compatible address and
an optional friendly name. Every successful connection is saved, including addresses entered in
Settings' advanced form. **Settings → Model connection** lists saved services with their addresses;
choose **Connect** to switch. Each connection remembers its selected model, including after restarting
the desktop app or browser. Failed switches keep the working connection available. Save up to 20
connections; forget one before adding another when the list is full. Forgetting removes the profile
and keeps its access key. Pairing and direct connections do not start or reconfigure the model service
automatically.

For an authenticated service, expand **Access key**. The native app tests the key before saving it
in the platform credential store, scoped to the service origin. An empty setup field preserves the existing key.
**Settings → Security → Access keys** shows whether a model service or configured relay has a saved key.
Choose a saved model service to manage its key without changing the active connection. Connections
on the same service origin share a key; forgetting one profile does not remove that shared key.
Replace a model key with **Verify and save key**, or remove a stored key after reviewing the confirmation.
To replace a relay key, supply the new token when creating a guest link; it is saved after registration.
Removal affects future requests, not already-running requests or invitations. Revoke guest links
separately. Environment-variable credentials are managed outside this screen. Never put a key in a URL.

**I have an invitation** opens a full HTTPS Blackwall invitation in the default browser. It does not
save the invite secret or add a permanent computer. An expired or revoked invitation needs replacement
by the sender. Guest sharing requires a configured relay; see [sharing](SHARING.md).

## Local models

**Context window.** Before each request, Blackwall asks the service which server it is and what
context window the selected model is loaded with. The probes send no access key and take at most
3 seconds; the first server that answers is used, in this order:

| Server | Route | Reported window |
| --- | --- | --- |
| Ollama (any port) | `/api/version`, then `/api/ps` | the loaded model's `context_length` |
| llama.cpp `llama-server` | `/props` | `default_generation_settings.n_ctx` |
| vLLM | `/v1/models` | the model's `max_model_len` |
| LM Studio | `/api/v0/models/{id}` | `loaded_context_length` |

A reported window replaces the configured limit (capped at 1,000,000 tokens), and a quarter of it
(up to 32,768 tokens) is reserved for output. The server sets the window itself, for example with
Ollama's `OLLAMA_CONTEXT_LENGTH` or llama.cpp's `--ctx-size`; requests cannot change it. A model that
is not loaded, an unknown server, or an unreachable one keeps your configured limit. The detected
server kind is remembered for the rest of the session. **Settings → Model connection** shows it,
for example "Detected llama.cpp · 131,072-token context"; in the CLI, `bw status` and `/context`
print it.

**Reasoning.** Text a model writes inside `<think>…</think>`, or sends in the `reasoning` or
`reasoning_content` fields, is kept apart from the answer, including a tag split across stream
chunks. Reasoning that never closes is not shown as answer text. Reasoning is never sent back to
the model. The desktop shows it collapsed above the answer as **Thought for Ns**; the CLI prints
`[Thought for Ns]`, and `bw run --json` includes `reasoning` and `reasoningMs` on `turn_complete`.
Traces record it only when prompt and output capture is on, as a `reasoning` part of
`gen_ai.output.messages`. It is limited to the same 8 MiB as the rest of a reply.

**Tool calls written as text.** Some models reply with `<tool_call>{"name":…,"arguments":{…}}</tool_call>`
blocks instead of structured tool calls. Blackwall reads them only when you turn on **This model
writes tool calls as text** for that connection in **Settings → Model connection**, or run
`bw config set text-tool-calls on`. It is off by default. When on, and a reply has no structured
calls, each block outside a code fence that names an offered tool with object arguments becomes a
call (at most 8 per reply) and is removed from the answer; anything else stays text. Text from the
first block onward is shown when the reply ends. **A call read from text always asks for approval**,
even when a saved rule would allow it, and that answer is never remembered for later calls. The
tool's own approvals still apply after that. Child investigations never run calls written as text.

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
When the model service reports the window it loaded the model with, CLI and desktop requests use
that window instead (see [Local models](#local-models)). The same section sets the Agent iteration limit (1–100, default
20) and the command time limit (10–3,600 seconds, default 120). The browser development UI also saves
these controls locally.

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
bw config set iterations 20
bw config set timeout 120
```

During interactive CLI chat, use `/settings context <tokens>`, `/settings output <tokens>`,
`/settings compact on|off`, `/settings iterations <count>`, and `/settings timeout <seconds>`.
`/compact` does not add a user or assistant message. Ctrl+C cancels
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

Chat mode sends messages and attachments to your selected model. In the desktop app, enable
**Web access** in either Chat or Agent to let the model search the public web and read text pages.
Chat needs no project folder and exposes only these two web tools. Web access starts off and resets
when starting or reopening a conversation. Each request shows its destination and purpose for
approval; denied, unreachable, and blocked pages return an error the model can explain. Web content
is untrusted, and local/private network addresses remain blocked. Search uses DuckDuckGo and can
be unavailable; this is text retrieval rather than an interactive browser. The browser preview and
guest links do not expose these desktop tools.

Agent mode additionally offers
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
MCP tool approvals also offer **Always allow**, for that tool or every tool from its server, in this
project or every project. It saves an owner rule in `policy.json`, applies it to the rest of the
run, and allows the call; if the rule cannot be saved, the call does not run and says why. The
server's page in **MCP servers** lists these rules and removes them; a removal applies from the next
run. Removing a server also removes its rules, so a later server with the same name starts asking.

To decide routine actions in advance, add rules to `policy.json` in the Blackwall data folder
(`~/.blackwall/`). Projects cannot supply rules. Each rule has a `decision` (`allow`, `ask`, or
`deny`), a `kind` (`exec`, `file`, or `network`), and one matching field:

```json
{"rules":[
  {"decision":"allow","kind":"exec","command":"cargo test"},
  {"decision":"deny","kind":"file","path":".git"},
  {"decision":"allow","kind":"network","host":"*.docs.rs"}
]}
```

`command` matches the leading words of a shell command, `path` matches a workspace-relative path and
everything below it, and `host` matches a host exactly or, with `*.`, its subdomains. For MCP calls,
an `exec` rule can use `tool` instead: a server name such as `context7` matches all of that server's
tools, and `server__tool` matches one. `tool` rules never match shell commands. Any rule can add
`project` with an absolute folder path, and then applies only to runs in that project. Ask and deny
path rules ignore letter case, as the default macOS and Windows file systems do. Deny outranks ask,
and ask outranks allow; actions that no rule matches still ask, and denied actions never show a
prompt. Commands containing line breaks, `;`, `&`, `|`, backticks, `$`, `(`, `<`, or `>` always ask
instead of matching an allow rule. Edits inside the data folder always ask, even when the project
contains it. Rules load when a run starts. If the file is invalid, every action asks and the prompt
shows the problem. Each rule decision and approval outcome is appended to `policy-audit.jsonl` with
common credential forms redacted; the log rotates at 1 MiB.

To let Agent mode call other programs, such as another coding assistant's command line, list them
in `tools.json` in the same data folder:

```json
{"tools":[
  {"name":"ask_claude","description":"Ask Claude a question","command":["claude","-p","{prompt}"]}
]}
```

Each tool runs its program directly, without a shell, in the project folder. The program must be a
name found in an absolute `PATH` directory or an absolute path. Batch files (`.bat`, `.cmd`) are
refused, and on Windows the program must be an `.exe`. An argument written as a whole `{name}` becomes a text parameter the model
fills in; values that start with `-` (or `/` on Windows) are rejected so the model cannot add
options. Every call needs Exec approval and is matched by rules
against the configured command, so `{"decision":"allow","kind":"exec","command":"claude -p"}` allows
it without a prompt. Calls use the command time limit and output bounds. If the file is invalid, no
owner tools are offered and the run shows a warning.

Local MCP servers that use standard input and output can be listed in `mcp.json` in the same data
folder:

```json
{"servers":[
  {"name":"files","command":["npx","-y","@modelcontextprotocol/server-filesystem","/path/to/folder"],"denyTools":["write_file"]}
]}
```

Each enabled server starts when an Agent run starts, in the data folder, without a shell, and stops
when the run ends; pass a project path as an argument when a server needs one. Programs follow the
same rules as command tools, so on Windows a server needs an `.exe` launcher such as `node` rather
than `npx`. Server names use lowercase letters, digits, and `-`; names added with `bw mcp add` or
the desktop are lowercased, with spaces and underscores as hyphens. Its tools are offered
as `server__tool`, such as `files__read_text_file`; `allowTools` limits a server to the listed
tools, `denyTools` hides tools, and `"enabled":false` keeps a server from starting. Tools whose
names are invalid or already used are skipped with a warning. Every call needs Exec approval, and
rules match the namespaced name, so `{"decision":"allow","kind":"exec","command":"files__read_text_file"}`
allows that tool; server descriptions and hints never grant permission. Calls use the command time
limit, and a server that times out is not used again in that run. Text results are returned to the
model; other content is noted but omitted. Servers inherit your environment without the
credentials Blackwall itself uses. A server that needs an access token lists its environment
variable name under `secrets`; the value lives only in the platform credential store and is given to
that server's process when it starts. If the file is invalid, a secret is missing, or a server does
not start, the run continues without those tools and shows a warning.

In the desktop app, **Settings → AI capabilities → MCP servers** lists the servers with enable and
disable toggles, adds a server from its command line, removes one, and saves or removes its secrets through a
password field. It uses the same validation and credential store as the CLI. **Add server** offers
suggested general-purpose servers (Chrome DevTools, Playwright, Context7, filesystem, sequential
thinking, Git, and `mcp-remote` for servers offered only as a web address). Picking one fills in the
form with a pinned package version; filesystem and remote ask for a folder or address at the end of
the command. The list ships with the app and is never downloaded, but `npx -y` downloads the pinned
package from npm the first time the server starts, without asking again. Suggestions start with
`npx`, so they are not offered on Windows.

The CLI can edit the file without starting a server. `bw mcp add <name> <command> [arguments]` adds
an enabled server, `bw mcp list` shows each server and its command, and `bw mcp enable <name>`,
`bw mcp disable <name>`, and `bw mcp remove <name>` change one entry. Changes are validated before
they are saved, apply from the next Agent run, and an invalid or linked file is reported rather than
replaced.
Tool lists are edited in the file. `bw mcp secret <name> <NAME>` reads a value from standard input,
for example `pbpaste | bw mcp secret jira JIRA_API_TOKEN` on macOS, saves it in the credential store,
and adds the name to the server; it refuses to read from a terminal, where the value would be shown.
Add `--delete` to remove one, and `bw mcp remove` deletes a server's saved secrets.

On macOS, the desktop app finds programs for command tools, MCP servers, and shell commands with
your login shell's `PATH`, so tools installed with Homebrew or npm work when Blackwall is opened from
the Dock. It asks the shell once at startup and keeps the system default if that takes more than
five seconds.

Shell commands and owner-defined command tools use an owner-selected execution profile. In desktop
Agent mode, choose **Commands → Read-only / Workspace-write / Unsandboxed** and **Command network**.
CLI owners use `bw config set profile read-only` (or `workspace-write` / `unsandboxed`) and
`bw config set command-network on|off`; `/settings` offers the same controls. The default is
read-only with command network disabled. Desktop selections reset when switching conversations;
CLI selections persist. Old saved settings acquire the safe defaults.

On macOS, Seatbelt applies before the shell/program starts and is inherited by descendants:

| Profile | Filesystem access | Command network |
| --- | --- | --- |
| Read-only | Read project and system runtime files; no project writes | Disabled unless explicitly enabled |
| Workspace-write | Also write inside the selected project, excluding protected paths | Disabled unless explicitly enabled |
| Unsandboxed | Full account permissions | Always enabled |

Sandboxed commands cannot read or write Blackwall's data directory, credential directories such as
`.ssh`, `.aws`, `.gnupg`, `.config`, or the owner's `Library`. Project metadata and agent configuration
folders (`.git`, `.hg`, `.svn`, `.agents`, `.codex`, `.claude`) are protected at every depth;
`AGENTS.md`, `AGENTS.override.md`, and `CLAUDE.md` remain readable but cannot be changed by commands.
System runtime reads include `/System`, `/usr`, `/bin`, `/sbin`, and `/opt/homebrew`; arbitrary
external source trees, home-installed runtimes and shared temporary folders are not granted. Put
build output and scratch files inside the project when using workspace-write.

Approval still applies to commands, including exact command text and working folder. Approval or
an allow rule never widens the profile. A denied action must be reported to the owner; change the
profile explicitly before proposing it again. A missing/broken macOS backend fails closed with no
unsandboxed retry. Linux and Windows currently fail closed for restricted profiles; full account
execution there requires explicitly choosing unsandboxed. Unsandboxed Unix commands use `/bin/sh`;
Windows uses Windows PowerShell.

Output remains bounded and the command time limit defaults to two minutes. **Stop**, cancellation,
and timeouts terminate the process group or Windows Job Object. Restricted macOS commands cannot
create a new process group/session to escape cleanup: direct `setpgid`/`setsid` calls and
`posix_spawn` are denied. Programs must use fork/exec for subprocesses; programs that require
`posix_spawn` will fail inside restricted profiles. The profile covers shell and owner-defined
command tools; separately configured MCP servers retain their own account permissions and approvals.
File tools remain workspace jailed, with reviewed edits; **Web access** controls the separate web
tools and does not enable command network. Model connections remain available with command network off.

Use **Stop** or Escape to interrupt the run. Child tasks stop with their parent. The agent stops after
the configured iteration limit (20 model/tool iterations by default); each child stops after eight. Pure child batches have a maximum concurrency
of three. The selected model must support OpenAI-compatible tool calls for Agent mode to work well.
Blackwall's instructions and any project guidance reach the model as one system message at the start,
because some chat templates, such as Qwen's on vLLM, reject any other system message.

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

**Skills** manages reusable instructions stored as `~/.blackwall/skills/<name>.md`, or as a folder
`<name>/SKILL.md` with bundled files for an imported skill. Create, edit,
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
Agent runs list each enabled skill's name and description, within an 8,000-byte budget, and the
model loads a skill's instructions with `read_skill` when it is relevant, so larger libraries stay
usable.
They cannot grant tool permissions.

Type `/name` at the start of a message to run a saved skill directly, in an Agent chat, the CLI, or
`bw run`. The agent gets that skill's instructions for the turn, and the rest of the message is the
request, as in `/pdf fill form.pdf from data.json`. In the desktop app, typing `/` lists the
commands and enabled skills. For a skill that should run only this way, check **Run only when I
type /name** in the editor, or add `disable-model-invocation: true` to its frontmatter. Agent runs
do not list such a skill, and `read_skill` cannot load it except in the turn you typed it. Typing
`/name` for a disabled skill answers that it is turned off. A skill cannot take a built-in command's
name, such as `help`, `learn`, or `review`.

Type `/learn` in an Agent conversation to have the model propose a skill from the workflow that
worked, leaving out failed attempts and secrets. The model can only propose: `skill_propose` stores
the proposal with its source session and message under `skills/.proposals`, and nothing is loaded
until the owner approves it. `bw skills` lists pending proposals with a diff against the current
skill, `bw skills approve <id>` saves one and keeps the proposal in `skills/.proposals/approved` as
a record of its source, and `bw skills reject <id>` discards it, even one that can no longer be
read. If the skill changed after the proposal was made, approval is refused so the owner's edit is
kept; reject it and review a fresh proposal. At most 20 proposals can be pending.

In the desktop app, **Skills** lists pending proposals under **Pending review**. Each one shows its
source session and message, the saved skill it would replace, and the proposed description and
instructions, which can be edited before **Approve**. Edits keep the proposed name and pass the same
validation as a saved skill; **Reject** discards the proposal.

To reuse skills written for other agents, choose **Import skills** in the Skills panel or use
`bw skills`. Blackwall reads the `SKILL.md` format used by Claude Code, Codex, and most published
skills: a folder with a `SKILL.md` whose frontmatter has a name and description, and optional
bundled files such as scripts and references. The whole folder is imported, up to 200 files and
5 MB; links and hidden files are left out. The panel lists skills found in `~/.claude/skills`,
`~/.codex/skills`, and `~/.agents/skills`, and **Choose folder…** or **Choose file…** adds others.
`bw skills import` lists the same folders, and `bw skills import <path>` imports a `SKILL.md`, a
skill folder, or a folder of them.

**Suggested** lists skills from the official Anthropic and OpenAI skill repositories and Matt
Pocock's engineering skills, each pinned to a reviewed commit. **Get** (or `bw skills get <name>`;
`bw skills suggested` lists them) downloads that version from GitHub for review; Blackwall goes
online only when you ask.

Before anything is saved, each skill is scanned locally for patterns that often hide prompt
injection or send data away: invisible characters and hidden comments, instructions to ignore you
or hide actions, uploads to remote addresses and paste or webhook services, scripts that read secret
files such as `~/.ssh` or `.env`, long encoded blocks, code that decodes and runs itself, and downloads
piped into a shell. Findings show the file, line, and text. A clean scan reads "Scanned: no issues
found", which means none of these patterns matched, not that a skill is safe. A skill with findings
is imported only if you choose **Allow anyway** (`--allow-anyway` in the CLI); **Block** drops it.
The files are read and scanned again at import, so a change after the preview is caught.
Saved skills and pending `/learn` proposals show the same badge in the Skills panel. Skills are
rescanned each time the panel lists them, so the badge follows edits; `bw skills` shows the scan on
pending proposals.

The preview also shows the license, the bundled files, and what conversion changed. Names are
converted to lowercase with hyphens, descriptions over 500 bytes are shortened, and settings such
as `allowed-tools` are ignored, because skills never grant permissions. `disable-model-invocation`
is kept, so the skill runs only when you type its name. A saved skill is replaced
only when you choose to (`--replace` in the CLI). The agent reads a skill's bundled text files
through `read_skill`, and runs its scripts with the shell tool by their full path, so each command
still needs your approval.

Guest chats never receive owner memory, skills, project tools, or local conversation history.

## Agent traces

Each CLI and desktop Agent run, and each desktop Chat run, records a local trace in
`~/.blackwall/traces` (or the `BLACKWALL_DATA_DIR` data folder) through the OpenTelemetry SDK. A
trace is one JSON file in the OpenTelemetry (OTLP/JSON) format with a span for the run, each model
call, and each tool call, using the OpenTelemetry generative AI names (`invoke_agent`, `chat`,
`execute_tool`). A `spawn_agent` child appears as `invoke_agent subagent` under its tool call,
with its own model and tool calls nested beneath it. Spans keep timings, the model, token counts
the server reports, finish reasons, tool names, errors, and approval requests. Blackwall keeps the
newest 200 traces and never sends them anywhere.

The same switch records usage metrics in `~/.blackwall/metrics/YYYY-MM-DD.jsonl`: token usage and
duration per model call, tool calls by outcome, and runs by outcome, kept for 400 days. Metrics
never include prompts, answers, tool arguments, or paths. See [Telemetry](TELEMETRY.md) for every
signal, attribute, and file format.

`bw traces` lists recent runs with their duration, model and tool calls, tokens, and any failure.
`bw traces show <id>` prints one run as a tree. Turn traces and usage metrics off with
`bw config set traces off`.

By default a trace holds no prompt, answer, tool argument, or tool output text, and a failure shows
only its error class, such as `denied`, `execution`, or `cancelled`. `bw config set
trace-content on` also records tool arguments, tool results, model answers, and failure messages,
each redacted like audit entries (plus secret-named JSON and YAML fields) and cut to 4 KiB. When
the run ends, a value redacted anywhere in it is removed wherever else it appears in that trace,
such as when the model repeats it. Redaction matches common secret names and forms and can miss
others. If a trace with content would pass 4 MiB, its content is left out. Traces with content can
still include private file contents, so turn it on only while investigating a run.

In the desktop app, **Settings → Workflows → Runs** lists the 50 newest runs and opens one as a tree
of model and tool steps, with failures, approval requests, and any captured content. **Record agent
runs** and **Include content** in that panel set the desktop's `tracesEnabled` and `traceContent`
preferences, which apply to Chat runs too. `/compact` requests are not recorded as runs; their
summary calls count only toward usage metrics.

## Usage

**Settings → Workflows → Usage** and `bw usage` total what the local usage metrics recorded for
Today, 7, 30, or 90 days on the desktop, or any 1 to 400 days with `bw usage --days N`. Days are
local days in this computer's time zone; `bw usage --utc` counts UTC days instead. Nothing is estimated in dollars: models run locally, and
command tools such as `claude` or `codex` are billed by their own services.

- **Tiles:** tokens recorded, with runs, sessions, and model calls; run success and how other runs
  ended; median output speed; tokens kept out of context by compaction.
- **Missing from this picture:** history before the first day with metrics, runs whose trace the
  writer queue dropped, and whether recording is off now. Past periods with recording off cannot
  be told apart from days without use, so days with no data are marked "no data" in the chart.
- **Where it went:** tokens by project (the workspace folder's name, never its path; Chat has no
  project) or by model. Selecting a row shows its share of the range, its split across Agent,
  Subagent, and Chat, and its busiest sessions by their neutral IDs. Titles and prompts are never
  recorded. Under the list, the range splits into Agent, Subagent, and Chat tokens, runs, and
  calls. In the Models view each row also shows calls, tokens in and out, median output speed, p90
  call duration, and the share of failed calls.
- **Tokens per day** as stacked input and output bars, with a table view; **What the tokens
  contain**: input, cached input, output, and reasoning, where the model server reports the last
  two; and **Top tools** with error and denied rates.

The page re-reads the files every 2 minutes while it is open and visible, and marks the data
*stale* when the newest point is more than 4 minutes old. A damaged or truncated line is skipped
with one warning per file; the rest of the range still shows. **Export…** copies the range's
metrics and trace files into a new `blackwall-telemetry-<from>-to-<to>` folder you choose, as plain
OTLP JSON any OpenTelemetry Collector can import.

```sh
bw usage                 # last 7 local days as text
bw usage --days 14 --json
bw usage --days 1 --utc  # today in UTC
```

## App lock

Enable **App lock** in **Settings → General → Security** with a passphrase of at least 10 characters.
Blackwall stores a salted Argon2id verifier in the platform credential store. The app locks on reopening, and **Lock now** stops active work and
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
bw usage --days 30
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
| `/review [target]` | Read-only Git review; `--uncommitted`, `--commit <ref>`, or `--base <ref>` ([limits and behavior](GIT_REVIEW.md)) |
| `/new` | Start a new conversation in the current project |
| `/sessions`, `/resume <id>` | List conversations or restore an Agent conversation and its project |
| `/quit`, `/exit` | Exit; EOF also exits |

Type `//` to begin a literal message with `/`. Use `bw commands` to see the catalog outside chat.
`bw config show` reviews settings, and `bw config set <name> <value>` changes them without entering
chat. `bw status` also asks the configured service which server it is and its context window.
`bw config set text-tool-calls on|off` controls [tool calls written as text](#local-models). Saved CLI choices take precedence over saved desktop preferences, then `BLACKWALL_MODEL`,
`BLACKWALL_MODEL_ENDPOINT` (or `OLLAMA_HOST`), then the initial `llama3.2` and local Ollama defaults.
CLI settings are separate from desktop preferences. They are stored in the same local SQLite
database; `BLACKWALL_DATA_DIR` selects a different data folder, including its memory and skills.

For an authenticated service, set `BLACKWALL_MODEL_API_KEY` and the matching
`BLACKWALL_MODEL_ENDPOINT` (or `OLLAMA_HOST`). Environment keys are sent only to that origin, even
after `/endpoint` changes. The CLI does not store model keys or use desktop credential-store entries;
MCP server secrets are the exception, shared through the credential store with desktop.
Never put a key in an endpoint URL. `bw request "prompt"` prints a protocol preview without a model call.

`bw run --json "prompt"` and `bw resume SESSION_ID --json "prompt"` run without prompting, for scripts
and CI. Each agent event except context snapshots is printed to standard output as one JSON object
per line, ending with a `result` line that has the session ID, `status`, the answer, and any error.
Standard input is not read: actions that owner rules allow run, and anything that would show an
approval prompt is denied.
The command exits with a nonzero status when the run fails; errors before the run starts, such as
invalid settings, are reported only on standard error.

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
