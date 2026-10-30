# Using the current Blackwall development build

Updated 2026-10-28. This guide describes the code in this checkout. It does not imply that a
published installer contains these changes. See the [release checklist](RELEASE_CHECKLIST.md) before release.

Blackwall runs on macOS, Windows 10/11, and Linux (x86-64 and ARM64). Saved access keys, pairing
credentials, and the app-lock verifier use the platform credential store: Keychain on macOS,
Credential Manager on Windows, and the Secret Service (GNOME Keyring, KWallet, or a compatible
provider) on Linux. On Linux, start and unlock a keyring before saving keys; without one, Blackwall
reports that no system keyring is available and does not fall back to plain-text storage. Local data
lives in `.blackwall` in your home folder (`%USERPROFILE%\.blackwall` on Windows).

## Connect a model

1. Open **Settings → Model connection → Set up a connection**. First launch opens setup automatically when no
   connection is saved: it lists every model server found on this computer, with its address, models
   and loaded model, and connects only to the one you choose (see [Finding servers on this
   computer](#finding-servers-on-this-computer)). A saved connection is reconnected without asking.
2. Choose **Connect** on a found server, or **Use this computer** for guided setup.
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

For direct model access, choose **Enter a server address** in setup, or type the address in the
**Server address** field of **Settings → Model connection** and choose **Connect**. Enter a reachable
Ollama or OpenAI-compatible address and an optional friendly name. Every successful connection is
saved. **Settings → Model connection** starts with **Set up a connection** and the servers found on
this computer, then the address field, then saved services with their addresses; choose **Connect** to switch. Each connection remembers its selected model, including after restarting
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

### Finding servers on this computer

Setup, **Settings → Model connection** and `bw setup` look for model servers on this computer only.
Blackwall asks the ports model servers commonly use (11434 Ollama, 1234 LM Studio, 8080 and 8081
llama.cpp, 8000 vLLM, 5000 and 5001 text-generation-webui and koboldcpp, 18040, 4891 GPT4All, 1337
Jan) and every other port listening on a loopback or all-interfaces address, read from
`/proc/net/tcp` and `/proc/net/tcp6` on Linux, `lsof` on macOS and `netstat` on Windows (2 seconds at
most). Ports below 1024 are skipped and at most 64 ports are asked. Each is asked at once, within 1.5
seconds, with GET requests only and no access key, on `127.0.0.1` (or `[::1]` for a port bound only
there). Responses are read up to 1 MiB. The network is never scanned, and servers on other computers
are added by address.

A server is listed when it answers Ollama's `/api/version` (its models from `/api/tags`, loaded
models from `/api/ps`) or an OpenAI-compatible `/v1/models`; its kind, loaded model and context
window are then detected as in the table below. Ollama's own per-model runner processes are not
listed separately. If the listening-port table is unavailable, the common ports are still asked.
`bw setup --list` prints what was found without changing settings; interactive `bw setup` shows the
list first and accepts a number at the endpoint prompt.

**Errors from the server.** When a model server rejects a request with an explanation, such as
Ollama's "model requires more system memory than is available", the chat error shows it, named after
the server when known. Ollama, llama.cpp, vLLM and OpenAI error bodies are read; control characters
are removed, access keys and credentials in addresses are redacted, and the text is clipped to 300
characters.

**Context window.** Before each request, Blackwall asks the service which server it is and what
context window the selected model is loaded with. The probes send no access key and take at most
3 seconds; the first server that answers is used, in this order:

| Server | Route | Reported window |
| --- | --- | --- |
| Ollama (any port) | `/api/version`, then `/api/ps` | the loaded model's `context_length` |
| llama.cpp `llama-server` | `/props` | `default_generation_settings.n_ctx` |
| vLLM | `/v1/models` | the model's `max_model_len` |
| LM Studio | `/api/v0/models/{id}` (or the `/api/v0/models` list) | `loaded_context_length` |

A reported window replaces the configured limit (capped at 1,000,000 tokens), and a quarter of it
(up to 32,768 tokens) is reserved for output. The server sets the window itself, for example with
Ollama's `OLLAMA_CONTEXT_LENGTH` or llama.cpp's `--ctx-size`; requests cannot change it. A model that
is not loaded, an unknown server, or an unreachable one keeps your configured limit. The detected
server kind, including "unknown", is remembered for the rest of the session, so later requests ask
only that server's route, or nothing; a known server that misses one answer keeps its kind. HTTPS
services on other computers, such as cloud APIs, are never probed by requests and keep your
configured limit. Opening **Settings → Model connection** or running `bw status` detects again,
cloud services included. **Settings → Model connection** shows it,
for example "Detected llama.cpp · 131,072-token context"; in the CLI, `bw status` and `/context`
print it.

**Reasoning.** Text a model writes inside `<think>…</think>`, or sends in the `reasoning` or
`reasoning_content` fields, is kept apart from the answer, including a tag split across stream
chunks. Reasoning that never closes is not shown as answer text. Reasoning is never sent back to
the model. The desktop shows it collapsed above the answer as **Thought for Ns**; the CLI prints
`[Thought for Ns]`, and `bw run --json` includes `reasoning` and `reasoningMs` on `turn_complete`.
Traces record it only when prompt and output capture is on, as a `reasoning` part of
`gen_ai.output.messages`. Each reply's reasoning is limited to the same 8 MiB as the rest of the
reply, and a run keeps at most 256 KiB of it, ending with "[Reasoning truncated]" when cut.

**Tool calls written as text.** Some models reply with `<tool_call>{"name":…,"arguments":{…}}</tool_call>`
blocks instead of structured tool calls. Blackwall reads them only when you turn on **This model
writes tool calls as text** for that connection in **Settings → Model connection**, or run
`bw config set text-tool-calls on`. It is off by default. When on, and a reply has no structured
calls, each block outside a code fence that names an offered tool with object arguments becomes a
call (at most 8 per reply) and is removed from the answer; anything else stays text. Text from the
first block onward is shown when the reply ends. **A call read from text always asks for approval**,
even when a saved rule would allow it. The prompt names what the tool does (a file change, a web
request, or a command), shows the arguments (cut to 4 KiB), and offers only **Allow once** or
**Deny**; the answer is never remembered for later calls. The tool's own approvals still apply
after that, except that an approved web read or search does not ask a second time for its request. Child investigations never run calls written as text.

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
20), the command time limit (10–3,600 seconds, default 120) and the command output cap (4,096–1,048,576
bytes, default 65,536; `bw config set output-bytes`). The browser development UI also saves
these controls locally.

Command output over the cap is trimmed instead of failing: the model gets the first 40% and the
last 60% of the cap with a `[… N bytes omitted …]` line between them, and the tool result is marked
`truncated` (the desktop says "Output trimmed" next to, not inside, the output). Cuts never split a
character, and HTTP tool responses are trimmed only after saved secrets are redacted. A command
whose standard output or error passes 64 MiB is stopped. Shell commands and owner command tools
are trimmed; file reads keep their hard 64 KiB limit, and MCP results their own limits.

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
| Delegate a subtask | A child with the [subagent tools](#subagents) you allow (read-only by default); never further delegation |

For each proposed change, choose **Deny**, **Allow once**, or **Allow identical action this run**.
The last option remembers only the exact action until the current run ends. It does not grant a
permanent project or domain policy. Expired and cancelled approval handles cannot authorize actions.
MCP tool approvals also offer **Always allow**, for that tool or every tool from its server, in this
project or every project. It saves an owner rule in `policy.json`, applies it to the rest of the
run, and allows the call; if the rule cannot be saved, the call does not run and says why. The
server's page in **MCP servers** lists these rules and removes them; a removal applies from the next
run. Removing a server also removes its rules, so a later server with the same name starts asking.

To decide routine actions in advance, add rules to `policy.json` in the Blackwall data folder
(`~/.blackwall/`). A project can add its own rules too; see [Project settings](#project-settings).
Each rule has a `decision` (`allow`, `ask`, or `deny`), a `kind` (`exec`, `file`, or `network`), and
one matching field:

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
tools, and `server__tool` matches one. `tool` rules never match shell commands. A `network` rule
can use `tool` instead of `host` to name one HTTP command tool from `tools.json` (see below).
Any rule can add `project` with an absolute folder path, and then applies only to runs in that
project. Ask and deny path rules ignore letter case, as the default macOS and Windows file systems do. By default
(`"mode": "strictest"`), deny outranks ask, and ask outranks allow. With `"mode": "ordered"` next to
`rules`, the first matching rule in the file decides instead, so an earlier allow can make an
exception to a later rule; see [ordered rules](#ordered-and-strictest-rules). Actions that no rule
matches still ask, and denied actions never show a prompt. Commands containing line breaks, `;`, `&`, `|`, backticks, `$`, `(`, `<`, or `>` always ask
instead of matching an allow rule. Edits inside the data folder always ask, even when the project
contains it. Rules load when a run starts. If the file is invalid, every action asks and the prompt
shows the problem. Each rule decision and approval outcome is appended to `policy-audit.jsonl` with
common credential forms redacted; the log rotates at 1 MiB. Each record's `rule` names the rule that
decided, as `{"source":"user"|"project","index":N,"mode":"strictest"|"ordered"}` (`index` counts
from 0 in that file's `rules`), or is `null` when no rule matched and the action asked by default.

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

There is no `bw` or desktop editor for `tools.json`; edit the file, and changes apply from the next
run. Tools without the fields below load and behave exactly as before.

**Standard input.** `"stdin":"json"` writes `{"arguments":{…}}`, the call's arguments, to the
program's standard input and then closes it, so long text such as a prompt is not limited by the
8,000-byte argument cap or the option check (up to 256 KiB of JSON). The approval prompt shows the
input's size, its SHA-256, and its first 2,000 characters. For example, a small wrapper lets
`claude -p` read its prompt from stdin JSON:

```sh
#!/bin/sh
# ~/bin/ask-claude: read {"arguments":{"prompt":…}} and pass the prompt to Claude on stdin.
python3 -c 'import json,sys; sys.stdout.write(json.load(sys.stdin)["arguments"]["prompt"])' \
  | exec claude -p --permission-mode plan
```

```json
{"tools":[
  {"name":"ask_claude","description":"Ask Claude a question","command":["/Users/me/bin/ask-claude"],
   "stdin":"json",
   "parameters":{"type":"object","properties":{"prompt":{"type":"string","description":"The question"}},"required":["prompt"]}}
]}
```

**Environment.** A command tool starts with a short allow-listed environment, not Blackwall's own
(see [Child environments](#child-environments)). To pass your own variables through, list their
names: `"env":["GITHUB_TOKEN","HTTPS_PROXY"]`. Names only, never values; at most 32 distinct names,
and the variable must be set where Blackwall runs. Only you can write this list, and HTTP tools
take no `env`. A command tool run in a restricted (sandboxed) profile ignores `env` and gets only
the sandbox set; the names apply when it runs unsandboxed. A name that loads code into a program
(`LD_*`, `DYLD_*`, `PYTHONPATH`, `NODE_OPTIONS`, `BASH_ENV` and similar), `PATH`, or one of
Blackwall's own secrets is refused, and the file is invalid.

**Argument schemas.** Without `parameters`, each placeholder is a required text parameter. A tool
with `"stdin":"json"` or `"body":"json"` and no `parameters` then takes exactly those names, each a
string; anything else is refused, since the whole object reaches the program or service. With
`parameters`, the tool declares its own schema, a subset of JSON Schema: `"type":"object"` with up
to 64 `properties`, each of `type` `string`, `number`, `integer` or `boolean`, with an optional
`description` and `enum` list, plus `required`. `additionalProperties` may only be `false`, which is
also the default. Arguments that are not an object, miss a required name, add a name the schema does
not list, or have the wrong type or a value outside `enum` are refused before any approval. An
`integer` also accepts a whole-number float such as `3.0`, written as `3`. Every
placeholder must be a required parameter; numbers and booleans fill a placeholder as JSON writes
them.

**HTTP tools.** `"kind":"http"` sends one request instead of running a program:

```json
{"tools":[
  {"name":"gitlab_issue","description":"Read a GitLab issue","kind":"http",
   "url":"https://gitlab.example.com/api/v4/projects/{project}/issues/{iid}",
   "headers":{"PRIVATE-TOKEN":"{secret:GITLAB_TOKEN}"},
   "parameters":{"type":"object","properties":{"project":{"type":"integer"},"iid":{"type":"integer"}},"required":["project","iid"]}},
  {"name":"jira_comment","description":"Comment on a Jira issue","kind":"http","method":"POST","body":"json",
   "url":"https://example.atlassian.net/rest/api/2/issue/{key}/comment",
   "headers":{"Authorization":"{secret:JIRA_AUTH}"},
   "parameters":{"type":"object","properties":{"key":{"type":"string"},"body":{"type":"string"}},"required":["key","body"]}}
]}
```

- `url` must be `https`, or plain `http` only to the literal `127.0.0.1` or `[::1]`, with no
  sign-in part or `#fragment`. Placeholders may appear only in path segments and query values,
  never in the scheme, host, port or a query name, and each value is percent-encoded; a whole
  segment of `.` or `..` is refused, and so is a resolved URL over 8 KiB.
- `method` is `GET` (the default) or `POST`. `"body":"json"` (POST only) sends the arguments as a
  JSON object.
- `headers` values may only be `{secret:NAME}`, and a name may appear only once, ignoring case. Save the value, piped so it is never shown, with
  `op read "op://Private/GitLab/token" | bw tools secret gitlab_issue GITLAB_TOKEN` (`--delete`
  removes it). It is stored in the credential store under `com.blackwall.tools` for the URL's origin
  (`scheme://host:port`), so pointing the tool at another host never sends it; save it again for
  the new origin. Put any scheme in the value itself, such as `Bearer …` or `Basic …`. Saved values
  are replaced with `[redacted]` in responses.
- Every call asks for Network approval. The prompt shows the method and resolved URL, with
  secret-looking query values hidden (names such as `token`, `key` or `sig`, or ending in `_key` or
  `_token`), the header names, any body, and a SHA-256 of the request. A call the model wrote as
  text asks first as Network and then still shows this prompt.
- Network rules match the URL's host, so
  `{"decision":"allow","kind":"network","host":"api.github.com"}` allows a tool without `headers`
  or a body without a prompt. A tool that sends `headers` or a JSON body is allowed only by a rule
  that names it, `{"decision":"allow","kind":"network","tool":"gitlab_issue"}`; with only a host
  allow it still asks. Deny and ask rules for the host apply to every tool, and beat a tool allow.
- Proxy variables are ignored, redirects are not followed, and compressed responses are refused
  (Blackwall asks for `identity`). The response is capped at 64 KiB, counted as it streams, and
  must be UTF-8. A response that is not 2xx is an error with the status and at most 200 characters
  of the body, with saved values redacted. Calls use the command time limit.

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
model; other content is noted but omitted. A server starts with an allow-listed environment (see
[Child environments](#child-environments)), not a copy of yours. If it needs a variable such as a
proxy setting or a token you already export, list its name under `env`:
`"env":["GITHUB_TOKEN"]`. Names only, never values; at most 32 distinct names, and the variable must
be set where Blackwall runs. Only you can set `env`: a server added by a project's
`.blackwall/project.json` cannot use it, and a server with a `url` takes no `env` (use `headers`).
A name that loads code into a program (`LD_*`, `DYLD_*`, `PYTHONPATH`, `NODE_OPTIONS` and similar),
`PATH`, or one of Blackwall's own secrets is refused, and the file is invalid. A server that needs an
access token can also list its environment variable name under `secrets`; the value lives only in
the platform credential store and is given to that server's process when it starts. If the file is
invalid, a secret is missing, or a server does not start, the run continues without those tools and
shows a warning.

When a server does not start, the tool card's details show the last lines of its error output and
the variable names it was not given, so a missing `env` entry is easy to spot. The text has
terminal escape sequences removed, is capped at 2 KiB, and has the server's secrets redacted. Only
you see it: it is not sent to the model, saved in the conversation or traced. `bw` prints it under
the warning as `  details: …` on standard error, also with `bw run --json`, whose JSON on
standard output never includes it.

A server offered as a web address uses MCP streamable HTTP (protocol 2025-06-18) instead of a
command. Give it a `url`, and `headers` whose values each name a saved secret:

```json
{"servers":[
  {"name":"docs","url":"https://mcp.example.com/mcp","headers":{"Authorization":"{secret:DOCS_TOKEN}"},"secrets":["DOCS_TOKEN"]}
]}
```

Each entry has exactly one of `command` and `url`. The URL must use https; plain http is allowed only
for the literal addresses `127.0.0.1` and `::1` (not `localhost`, which a hosts file could point
elsewhere), and a URL may not contain a user name or password. A header value must be exactly
`{secret:NAME}`: the header is sent with that secret's saved value, so save the whole value, such as
`Bearer <token>` for `Authorization`. Literal header values are refused, and the protocol's and
HTTP's own headers (`Accept`, `Content-Type`, `Mcp-Session-Id`, `MCP-Protocol-Version`, `Host`,
`Connection`, `Upgrade`, `Expect`, `Keep-Alive`, `Trailer`, `Proxy-Authorization`, `Cookie` and
similar) cannot be set. Secret values are removed from tool results, errors, tool descriptions and
input schemas.

A URL server's secrets are bound to the origin (`scheme://host:port`) they were saved for:
`bw mcp secret` records it under `secretOrigins`, and a header is sent only when its secret was saved
for the URL's current origin. If the URL moves to another origin, or a local server's environment
secret is reused as a header, the server is skipped with a warning until the secret is saved again,
and `bw mcp list` and the desktop show which secrets were saved for another address. Saving a
server whose origin changed through Blackwall also removes its approval rules, so it asks again;
edit `url` with care by hand, as a hand edit keeps the rules in `policy.json`.

Blackwall posts each JSON-RPC message with `Accept: application/json, text/event-stream`, reads a
JSON or event-stream reply (single messages or batches), keeps the `Mcp-Session-Id` from
initialization, and sends `DELETE` to end the session when the run ends. If the server forgets the
session (404), Blackwall initializes again once and retries. It never uses a proxy from
`HTTPS_PROXY` or similar variables and follows only 307 and 308 redirects within the server's
origin; others are errors. Replies to notifications and to the server's own requests should be
202 Accepted; any body they carry is closed unread. Limits: a reply is at most 1 MiB in total, at
most 256 messages wait at once, a server may send at most 32 requests of its own while answering
one call (more stops the server for the run), and a session id is at most 128 visible ASCII
characters. Requests the server sends on a reply stream are answered "method not found" (ping is
answered). Startup has 15 seconds and calls use the command time limit, as for local servers; a
server that times out or fails is not used again in that run. Servers that need OAuth sign-in are
not supported yet; bearer tokens and API-key headers cover most hosted servers.

Server names that start with `p-` are reserved for project servers. Blackwall will not add one, and
an existing `mcp.json` entry with such a name is skipped with a warning to rename it; the other
servers still start.

In the desktop app, **Settings → AI capabilities → MCP servers** lists the servers with enable and
disable toggles, adds a server from its command line or, with the **Command | URL** switch, from
its address with header rows that each name a secret, removes one, and saves or removes its secrets through a
password field. It uses the same validation and credential store as the CLI. **Add server** offers
suggested general-purpose servers (Chrome DevTools, Playwright, Context7, filesystem, sequential
thinking, Git, and `mcp-remote` for servers offered only as a web address). Picking one fills in the
form with a pinned package version; filesystem and remote ask for a folder or address at the end of
the command. The list ships with the app and is never downloaded, but `npx -y` downloads the pinned
package from npm the first time the server starts, without asking again. Suggestions start with
`npx`, so they are not offered on Windows.

The CLI can edit the file without starting a server. `bw mcp add <name> <command> [arguments]` adds
an enabled server, `bw mcp add <name> --url <url> [--header <Header>=<SECRET_NAME>]...` adds an HTTP
server whose headers come from those secrets (save each with `bw mcp secret`), `bw mcp list` shows each server and its command, and `bw mcp enable <name>`,
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

### Child environments

Every program Blackwall starts gets an allow-listed environment instead of Blackwall's own. This
covers shell commands, command tools, MCP servers, and the git, browser-opener and listening-port
discovery (`lsof`, `netstat`) helpers, on every system. The Linux browser opener also gets the
desktop-session variables it needs. It keeps the AppImage's runtime variables (`PYTHONHOME`,
`LD_LIBRARY_PATH` and similar) from breaking Python and other runtimes, and keeps your unrelated
credentials from reaching a program that does not need them.

- Programs you run for yourself (unsandboxed shell commands and command tools, MCP servers) get
  `HOME`, `LANG`, `LC_*`, `TERM`, `TMPDIR`, `USER`, `LOGNAME`, `SHELL`, `TZ` and `XDG_RUNTIME_DIR`,
  plus the names you list under `env` in `mcp.json` or `tools.json`.
- An unsandboxed shell command gets that list but no `env` names, since the shell has no `env`
  list: `SSH_AUTH_SOCK`, `KUBECONFIG`, `AWS_PROFILE`, `DISPLAY` and similar are not inherited. On
  Linux every shell command currently runs unsandboxed, so this applies to all of them.
- A sandboxed shell command or command tool gets only `HOME`, `LANG`, `LC_ALL`, `TERM`, `TMPDIR`
  and `PATH`, and ignores `env`.
- `PATH` is your login shell's, with relative and empty entries, the project folder and the
  AppImage's own folders removed. On Windows, the variables programs need to start are kept too.
- Blackwall's model key, relay token and signing keys are never passed, even if listed in `env`.

To see what a program gets, run `env` as an unsandboxed command.
File tools remain workspace jailed, with reviewed edits; **Web access** controls the separate web
tools and does not enable command network. Model connections remain available with command network off.

Use **Stop** or Escape to interrupt the run. Child tasks stop with their parent. The agent stops after
the configured iteration limit (20 model/tool iterations by default); each child stops after its
step limit (eight by default, see [Subagents](#subagents)). Batches of read-only children run at most
three at once; children that can edit, run commands or use the web, or that run on a command tool,
run one at a time. The selected model must support OpenAI-compatible tool calls for Agent mode to work well.
Blackwall's instructions and any project guidance reach the model as one system message at the start,
because some chat templates, such as Qwen's on vLLM, reject any other system message.

Tool entries are collapsed by default and can be expanded to inspect results. Restored transcripts
show past actions without executing them again. A fresh model turn currently receives the prior prose
and attachments; historical tool-call protocol messages are not replayed across separate turns.

## Subagents

`spawn_agent` hands a focused subtask to a child. Set its limits in **Memory → Advanced ·
Subagents** or with `bw config set`:

| Setting | `bw config set` | Default |
| --- | --- | --- |
| Step limit, 1–50 model turns | `subagent-steps <n>` | 8 |
| Tools: any of `read_file`, `list_files`, `search_files`, `write_file`, `shell`, `web_fetch`, `web_search` | `subagent-tools <a,b,…\|none>` | `read_file,list_files` |
| Runs on: this model, or one owner command tool | `subagent-runs-on <local\|TOOL>` | `local` |

A child never gets `spawn_agent`, memory or skills. Edits, commands and web requests from a child
ask for approval and obey your rules and execution profile exactly as the parent's do; web tools
also need **Web access** on. A child cannot confirm a tool call written as text, so such calls never
run.

**Running on a command tool.** With `subagent-runs-on <name>`, a child does not use this model.
Its goal and context go, as one prompt, to that `tools.json` tool, and the tool's output (without
the `Exit: 0` line) is the child's answer. The tool must:

- run a program (not `"kind":"http"`) with `"stdin":"json"`; the prompt arrives as
  `{"arguments":{"prompt":"Task: …\n\nContext:\n…"}}` and never on the command line;
- declare a string `prompt` in `parameters`, need no other argument, and use no `{placeholders}`.

`bw config set subagent-runs-on` and the desktop check this when you choose the tool, and every
run checks again. Each delegation **always asks**: an allow rule never skips the prompt (a deny
rule still denies), "allow identical action this run" is not offered, and the prompt shows the
whole text being sent, marked **Sends data off this device**. The model is not offered that tool
directly, so it can reach it only this way. The delegation is traced as
`invoke_agent subagent` with `blackwall.mode` `subagent`.

Delegate CLIs can act on their own, so give them read-only flags. A wrapper that reads the prompt
from stdin JSON, as in [Work in a project](#work-in-a-project):

```sh
#!/bin/sh
# ~/bin/review: a read-only second opinion. Swap the last line for another CLI below.
python3 -c 'import json,sys; sys.stdout.write(json.load(sys.stdin)["arguments"]["prompt"])' \
  | exec claude -p --permission-mode plan
```

```json
{"tools":[
  {"name":"review","description":"Ask another assistant for a read-only review","command":["/Users/me/bin/review"],
   "stdin":"json",
   "parameters":{"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"]}}
]}
```

| CLI | Last line of the wrapper |
| --- | --- |
| Claude Code | `\| exec claude -p --permission-mode plan` |
| Codex | `\| exec codex exec --sandbox read-only -` (`-` reads the prompt from stdin) |
| GitHub Copilot CLI | `\| xargs -0 copilot -p` with no `--allow-tool` or `--allow-all-tools`, so it cannot use tools that need permission (Copilot takes the prompt as an argument; check `copilot --help` for your version) |

Then `bw config set subagent-runs-on review`. The tool runs under your execution profile, and the
delegate CLI needs network to reach its service, so turn on command network or pick a profile that
allows it.

A trusted project can lower the step limit and narrow the tools (see
[Project settings](#project-settings)) but can never set where children run.

## Project instructions

Agent runs read `AGENTS.md` guidance (see [Project instructions](PROJECT_INSTRUCTIONS.md)). Set its
limits and extra files in **Memory → Advanced · Instructions** or with `bw config set`:

| Setting | `bw config set` | Default |
| --- | --- | --- |
| Per-file limit, 1–64 KiB | `instruction-file-kib <n>` | 8 |
| Total for a turn, 8–256 KiB | `instruction-total-kib <n>` | 32 |
| Up to 8 extra files read after `AGENTS.md` | `instruction-files <a,b,…\|none>` | none |

A file over the per-file limit is cut at the limit, on a character boundary, and the run's warnings
name it; the model sees a note after the file's guidance, not inside it. A file that would pass the
total is skipped with a warning, as before.

Extra files, such as `CLAUDE.md` or `.github/copilot-instructions.md`, are read after `AGENTS.md` (or
`AGENTS.override.md`) in your data folder and in each project folder that is searched, in the order
you list them. They follow the same rules: regular UTF-8 files only, symbolic links refused, never
outside the folder. Each must be a relative path of plain names with `/`: no `..`, `.`, `.git`,
leading `/`, drive letter, backslash or colon, and not `AGENTS.md` itself. The CLI and the desktop
check this when you save, and every run checks again.

Guidance lines that look like the prompt's own markers (`BEGIN GUIDANCE`, `END GUIDANCE`, `Source:`,
`Scope:`, in any case and spacing) reach the model prefixed with `> `, so a file cannot end its block
early or pose as another file.

## Project settings

A project can carry its own settings in `.blackwall/project.json` at the project root. Because the
file comes with the folder, anyone who can change the repository can change it, so Blackwall never
lets it add permissions on its own:

```json
{
  "policy": {"mode": "ordered", "rules": [
    {"decision": "deny", "kind": "exec", "command": "cat secrets.env"},
    {"decision": "allow", "kind": "exec", "command": "cargo test"}
  ]},
  "mcp": {"servers": [{"name": "docs", "url": "https://docs.example.com/mcp"}]},
  "limits": {"max_output_bytes": 32768, "subagents": {"max_steps": 4, "tools": ["read_file"]}},
  "instructions": {"max_file_kib": 4, "extra_files": ["CLAUDE.md"]}
}
```

Every section is optional, and unknown fields make the file invalid. `policy` uses the same rule
format as `policy.json`, except that a project rule cannot name a `project`, and a project allow
rule can neither name one of your MCP servers (only the project's own `p-` servers) nor match every
file (a `path` of `.` or anything that resolves to the project root). A project allow rule never
allows one of your MCP or command tools, even through `command`; only its own `p-` servers.

**Trust.** Until you trust the file, only its `deny` and `ask` rules apply, because they can only
tighten. Its `allow` rules and every other section wait. To trust it:

- Desktop: when the selected project has an untrusted or changed file, a banner appears under the
  project bar. **Review project settings** lists the file's rules (decision, kind, and what each
  matches), marks broad ones ("matches every file", "any host", "your MCP server"), and shows the
  file (or, when it changed, the diff from the version you trusted). Choose **Trust** or **Not
  now**; Not now hides the banner for that version of the file. If the file changes while the
  dialog is open, Trust is turned off until you reload and review it again.
- Invisible characters that can disguise text (bidi controls, zero-width characters, the byte
  order mark, and other Unicode format characters) are shown as `\u{XXXX}`, and long runs of blank
  lines are collapsed with a note, in both the desktop and the CLI. A diff over 5,000 lines is
  replaced by the whole file.
- CLI: `bw project status` shows the file's path, its trust, its rules, what applies now, and
  what waits. `bw project trust` prints the rules and the file (or the diff) and asks `y/N`. Without a terminal it refuses
  unless you pass `--yes`.

Trust is saved in `trusted-projects.json` in the data folder as the project's canonical folder path
and the SHA-256 of the file's exact bytes, plus a copy of the file to show later changes. Any edit,
even whitespace, makes the file untrusted again until you review it. Only the bytes you were shown
are trusted: if the file changes between showing and trusting, trusting is refused. Untrust a
project with `bw project untrust` in it, or from **Settings → General → Security → Trusted
projects**; its deny and ask rules keep applying.

Like `policy.json`, the trust list is an ordinary file in your data folder. Anything you let run
commands as your user, including an agent command you approve, could change it; that is an accepted
local risk, the same as for your own rules. If the list cannot be read, Blackwall treats that as a
sign of tampering: every action asks until it is fixed or removed.

**What applies.** Rules load at the start of every run, from the owner's file and then the
project's. Across files, a deny or ask from either file beats an allow from either file, whatever
the mode. Writes to the data folder and to the project's own `.blackwall` folder (in any letter
case) always ask. When an untrusted project's deny or ask rules apply, the run's warnings say how
many. The file must be a regular file of at most 64 KiB in
a real `.blackwall` folder; symbolic links are refused. It is read once, and its hash and settings
come from those same bytes. If it cannot be read or is invalid, every action asks for approval
until it is fixed, and the run's warnings and `bw project status` say why.

**Project MCP servers.** Once the file is trusted, each server in its `mcp` section starts with the
run as `p-<name>`, so a project's `docs` is `p-docs` and its tools are `p-docs__<tool>`; it can
never shadow one of your servers, and your own `mcp.json` cannot use the `p-` prefix. A project
server is reached only by `url` over streamable HTTP, never by a command, and it never gets your
secrets or headers. Its address must be on the public internet: a URL whose host is, or resolves
to, a loopback, private, link-local or shared address (`127.0.0.0/8`, `10.0.0.0/8`,
`172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`, `100.64.0.0/10`, `::1`, `fc00::/7`,
`fe80::/10` and similar) is refused with a warning, and the checked addresses are the ones used to
connect. Approvals for its tools say the server is "defined by this project", and **Always allow**
for a `p-` server or tool is always saved for this project only; an owner rule allowing a `p-` tool
without a `project` is ignored.

`limits` applies once the file is trusted, and only ever lowers yours: the run uses the smaller of
your and the project's `max_output_bytes` and `subagents.max_steps`, and only those of your
subagent tools the project also lists (`read_file` and `list_files`). A project cannot raise a
limit, add a tool, or name `runs_on`; a file that tries is invalid. The `instructions` section is
applied once the file is trusted and also only lowers yours: the run uses the smaller
`max_file_kib` and `max_total_kib`, and if it lists `extra_files`, only those of your extra files
that it also lists. A project cannot add an extra file.

### Ordered and strictest rules

Each rules file, the owner's `policy.json` and a project's `policy`, has its own `mode`:

| Mode | Within the file |
| --- | --- |
| `strictest` (default) | Deny outranks ask, ask outranks allow. Existing files keep this meaning. |
| `ordered` | The first matching rule decides, so `allow git status` before `ask git` allows `git status` and asks for other `git` commands. |

The mode only ranks rules inside one file. Blackwall then takes the strictest answer across files,
so a project's deny still beats your allow. Commands with control syntax never match an allow rule
in either mode.

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

**Memory** holds facts you saved or approved. Enable **Use saved memories in chat** to include a
bounded selection in future chat and agent requests. Memory is off by default. The advanced
context-window setting changes the displayed estimate, not the model server's configured context
length or a guaranteed token budget.

### Reviewed learning

Enable Memory in the desktop Memory dialog or with `bw config set memory on`.
In Agent mode, Blackwall can read reviewed facts with `memory_list` and suggest a
preference, convention, correction, or lesson using `memory_propose`. Proposals
stay out of persistent model context until you review them. Learning adds no
extra model calls; the agent chooses when to propose a useful durable fact during
a task or in response to a correction.

Open **Memory** to review pending suggestions. Each shows its source conversation
and user message, scope, saved text, and proposed text. Edit the proposed text and
choose **Approve**, or **Reject** it. The saved list supports **Edit** and **Forget**.
Review remains available while memory is off.

The same queue and facts are available from the CLI:

```sh
bw memory pending
bw memory approve <proposal-id>
bw memory approve <proposal-id> Revised text to remember
bw memory reject <proposal-id>
bw memory list
bw memory edit <memory-id> Updated text
bw memory forget <memory-id>
```

Use `/memory pending` and the other actions during interactive chat. `/memory on`
and `/memory off` control injection and agent access. CLI settings retain their
existing precedence over desktop settings; each client's memory switch controls
its own runs. Both clients share the same SQLite database in the configured data
folder.

User scope stores user preferences across projects. Project and environment
scopes store conventions and environment facts for the canonical selected project
folder only. Ordinary Chat receives user preferences; Agent runs also receive
facts for their project. Child agents and guest chats have no owner memory tools.
Guest chats use no owner memory injection. Memory is untrusted context and never
grants file access, shell approval, network access, or any other tool permission.

Stable topic keys identify related facts within a scope and project. The agent
should read existing facts and reuse a key or provide a `replaces` ID when refining
one. Whitespace/case-equivalent text and related pending keys are deduplicated.
This is deterministic matching, not semantic similarity: different topics or
wordings still need owner judgment. A proposal captures the saved revision;
editing, another approval, or forgetting invalidates that snapshot. Stale approval
fails without modifying the saved fact. Reject the old proposal and ask the agent
for a new one. Forgetting also removes pending replacements.

Schema version 3 adds scopes, stable keys, revisions, provenance, and the review
queue. Existing memories migrate to user scope without changing their text. Each
proposal references the actual saved user message that prompted the run. Accepted
facts retain that source. No transcripts are copied into memory metadata. The
queue is limited to 200 proposals, the fact store to 2,000 entries, individual
text to 8,000 bytes, tool-list output to 16,000 bytes, and injected context to
4,000 characters including its framing. The database upgrade is transactional;
older app versions will refuse to open the newer schema.

### Skills

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
newest 200 traces. They stay on this computer unless you turn on [OTLP export](#telemetry-export).

The same switch records usage metrics in `~/.blackwall/metrics/YYYY-MM-DD.jsonl`: token usage and
duration per model call, tool calls by outcome, and runs by outcome, kept for 400 days by default
(see [Usage](#usage)). Metrics
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
  writer queue dropped, whether recording is off now, and the periods in the range when recording
  was off (desktop and `bw` separately). Turning recording off before this version was not written
  down, so days with no data are marked "no data" in the chart, not "off".
- **Where it went:** tokens by project (the workspace folder's name, never its path; Chat has no
  project) or by model. Selecting a row shows its share of the range, its split across Agent,
  Subagent, and Chat, and its busiest sessions by their neutral IDs. Titles and prompts are never
  recorded. Under the list, the range splits into Agent, Subagent, and Chat tokens, runs, and
  calls. In the Models view each row also shows calls, tokens in and out, median output speed, p90
  call duration, and the share of failed calls.
- **Tokens per day** as stacked input and output bars, with a table view; **What the tokens
  contain**: input, cached input, output, and reasoning, where the model server reports the last
  two; and **Top tools** with error and denied rates.
- **Export:** the OTLP endpoint, when it last sent, its last error (class and HTTP status only),
  spans and metric batches sent and dropped, and **Send test span** (see
  [Telemetry export](#telemetry-export)).

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

Usage metrics are kept for 400 days by default. Choose 30 to 3,650 days or forever in **Settings →
Security → Telemetry**, or with `bw config set usage-retention-days 90` (or `forever`); older files
are deleted at the next start.

## Telemetry export

Send the same traces and usage metrics to an OpenTelemetry collector over OTLP/HTTP. Off by default.
Set it in **Settings → Security → Telemetry**, or:

```sh
bw config set otlp-endpoint https://otel.example.com     # or http://127.0.0.1:4318; off to stop
bw config set otlp-protocol protobuf                    # or json
bw config set otlp-signals traces,metrics               # or just one
bw config set otlp-include-project off                  # project names and session IDs stay local
op read "op://Private/otel/api-key" | bw telemetry header x-api-key   # value stays in the keychain
bw telemetry test
```

By default the endpoint is off, the protocol is protobuf, both signals are sent, and project names
and session IDs stay local. `https` is required except for `127.0.0.1`, `::1`, and `localhost`; proxies and redirects are not
followed and each request has a 10 s limit. Header values live only in the credential store, bound
to the endpoint origin they were entered for; set the endpoint first, and enter headers again after
moving to another origin. Prompts and outputs
leave only when content capture is on, and project names and session IDs only when you allow them.
Export never slows or fails a run: a full queue drops data and counts it, and failures appear only
in the export status. See [Export](TELEMETRY.md#export) for Collector, Grafana Alloy, Jaeger, and
Langfuse setups.

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
`bw config set text-tool-calls on|off` controls [tool calls written as text](#local-models). `bw project status|trust|untrust` reviews and trusts the current folder's
[project settings](#project-settings). Saved CLI choices take precedence over saved desktop preferences, then `BLACKWALL_MODEL`,
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
per line, ending with a `result` line that has the session ID, `status`, the answer, any error,
`reason`, `denied` (how many actions did not run because a rule, a prompt or the headless run denied
them; each also has a `tool_result` with `"denied": true`) and `usage` (`input` and `output` tokens the
server reported for every model call in the run, including calls before a failure, timeout or
interrupt). A `model_usage` event carries each call's tokens as it returns. Standard input is not
read: actions that owner rules allow run, and anything that would show an approval prompt is
denied. Add `--timeout SECS` or `--timeout=SECS` (10–86400) to stop the whole run after that many
seconds. Flags come before the prompt, each at most once; an unknown `--option` is a usage error,
and `--` ends the flags so a prompt can start with `--`.

The exit code says why the run ended, with or without `--json`:

| Exit | Meaning | `reason` |
|---|---|---|
| 0 | Completed, nothing denied | `complete` |
| 1 | Other error | `error` |
| 2 | Usage error, such as a missing prompt or a `--timeout` outside 10–86400 | `usage` |
| 3 | Completed, but at least one approval was denied | `denied` |
| 4 | Step limit (`iterations`) reached | `iterations` |
| 5 | Model or network error, including a request or response over Blackwall's size limit | `model` |
| 6 | Context budget exceeded | `budget` |
| 7 | Whole-run time limit (`--timeout`) reached | `timeout` |
| 130 | Interrupted (Ctrl+C), on every platform | `interrupted` |

`denied` counts denials whatever the `reason`. Exit 3 means the run completed after at least one
denial; when a denial is followed by an error, the error's reason and exit code win.
A usage error with `--json` still prints a `result` line, with a null session ID. Other errors
before the run starts, such as invalid settings, exit 1 and are reported only on standard error.

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
not implemented. [The release checklist](RELEASE_CHECKLIST.md) tracks these as open work.
