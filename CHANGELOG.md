# Changelog

All notable changes to Blackwall will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- A Max skill size preference (Memory → Advanced · Instructions, or
  `bw config set max-skill-kib`) defaults to 10% of the configured context window at about four bytes
  per token, clamped to 32–256 KiB, and accepts a manual 32–256 KiB override. Skill import and
  loading use the same limit, and refusals now identify the failed rule and exact sizes.
  Before downgrading to an older `bw`, reset a manual value with
  `bw config set max-skill-kib auto`; older versions reject settings containing the manual key.
- On Linux, Read-only commands now run in a sandbox: the project is read-only with `.git`, `.ssh`
  and the other protected folders hidden (below `node_modules`, `target` and the other skipped
  folders, only their direct children), of the home folder only `~/.cargo`, `~/.rustup`,
  `~/.nvm`, `~/.pyenv`, `~/.local/bin` and `~/.local/share/uv` are visible, read-only and with
  Cargo's credentials hidden, only the system folders (without `/usr/local/etc` or Linuxbrew's
  `etc`) and a short list of `/etc` files are visible, `/tmp` is
  private, Unix-domain sockets are blocked and the network is off unless Command network is on.
  In Read-only the project is mounted from the folder that was checked, by descriptor. It uses
  the bundled bubblewrap, or a root-owned system one of version 0.12.0 or newer. Without one, the
  note asks you to install Blackwall's `.deb` or AppImage or a newer bubblewrap; when the system
  won't let bubblewrap start a sandbox (the AppImage on Ubuntu 24.04), the note suggests the
  `.deb`. Commands fail closed either way. A project that is your home folder, contains it,
  contains another mounted file system, has more than 500,000 files outside the skipped folders, or
  takes more than 10 seconds to check is refused. With command network on, sandboxed commands can
  reach localhost and the local network, and the desktop app says so.
- On Linux, Workspace-write commands now run in a sandbox. The command writes to a private staged
  copy of the project; when it has exited, Blackwall checks the changes and copies them into the
  project: new and changed files (keeping their modification times; new ones get your umask),
  deletes and new folders. It refuses protected names (`.git`, `.ssh` and the others) and
  instruction files in any letter case, links that lead outside the project (but not absolute
  links into `/usr`, `/bin`, `/lib`, `/lib64` or `/opt`, as virtualenvs make), set-user-ID,
  set-group-ID and file-capability files, devices, pipes and sockets, and the output says why next
  to each path. Overlay metadata it can't apply safely, or more than 500,000
  changed entries, refuses the whole batch. Writes into the hidden folders succeed inside the
  sandbox but are thrown away. Anything you changed in the project meanwhile is a conflict and your
  version stays; deletes are held back when anything else was refused or conflicted. Nothing is
  applied after a time limit, a cancel, the output-size stop or a launcher killed from outside, and
  a command that exits with any code, including one killed inside the sandbox, has its finished
  changes applied. Staged changes that weren't all applied are kept for at least 24 hours, in a
  folder the output names. One Workspace-write command runs per project at a time. It needs Linux
  5.11 or newer with overlay mounts; a project on an overlay file system, inside Blackwall's data
  folder, containing the bubblewrap Blackwall uses or the app's own files, or whose path (or the
  data folder's) has a comma, colon or backslash is refused, and Read-only keeps working where
  Workspace-write isn't available.
- The Linux packages bundle bubblewrap 0.13.1 for sandboxed commands at `/usr/lib/blackwall/bwrap`
  (not setuid, built in CI from the release tarball whose SHA-256 is pinned; LGPL-2.0-or-later,
  license text under `/usr/share/doc/blackwall/bubblewrap/`, see `NOTICE`). Where the system's
  AppArmor supports it (Ubuntu 24.04 and later, Debian 13), the `.deb` installs and loads
  `/etc/apparmor.d/blackwall-bwrap`, which lets only that bwrap create user namespaces and gives
  the programs it runs no capabilities. Removing the package unloads and deletes it. Older AppArmor
  releases (Ubuntu 22.04, Debian 12) are left unchanged. The AppImage ships no profile.
- When an MCP server does not start, the tool card's details now show the last lines of its error
  output and the environment variable names it was not given, and `bw` prints them under the
  warning as `  details: …`. The text is cleaned of terminal escape sequences, capped at 2 KiB and
  redacted of the server's secrets. Only you see it: it is not sent to the model, saved in the
  conversation or traced.
- A configurable command output cap (`maxOutputBytes`, 4 KiB to 1 MiB, default 64 KiB; Memory →
  Advanced, or `bw config set output-bytes`). Output over it keeps its first 40% and last 60%
  around a `[… N bytes omitted …]` line, cut on character boundaries after redaction, and the tool
  result is marked `truncated`; it used to fail at 64 KiB. File reads keep their 64 KiB limit.
- Subagent settings (Memory → Advanced · Subagents, or `bw config set subagent-steps`,
  `subagent-tools`, `subagent-runs-on`): a step limit (1–50, default 8), any of the built-in file,
  search, edit, shell and web tools (default `read_file` and `list_files`; edits, commands and web
  still ask and follow your rules), and where children run. `runs_on` a command tool sends the
  goal and context as `{"arguments":{"prompt":…}}` on stdin to a `"stdin":"json"` tool with a
  `prompt` parameter; each delegation always asks, cannot be remembered, shows the whole prompt
  and is marked "Sends data off this device". The docs give read-only flags for Claude Code, Codex
  and Copilot. The model is not offered that tool directly.
- A trusted project's `limits` now apply, and only lower yours: a smaller output cap and step
  limit and fewer subagent tools. A project can never choose where subagents run.
- Setup, **Settings → Model connection** and `bw setup` find every model server on this computer:
  the common model server ports plus every port listening on loopback (Linux `/proc/net/tcp`, macOS
  `lsof`, Windows `netstat`), at most 64, asked at once for 1.5 s each with credential-free GETs.
  Each is listed with its kind (Ollama, llama.cpp, vLLM, LM Studio or OpenAI-compatible), address,
  models, loaded model and context window. Only loopback is asked. `bw setup --list` prints the list.
- Instruction limits and extra instruction files (Memory → Advanced · Instructions, or
  `bw config set instruction-file-kib`, `instruction-total-kib`, `instruction-files`): a per-file
  limit (1–64 KiB, default 8), a total for a turn (8–256 KiB, default 32), and up to 8 extra files
  such as `CLAUDE.md` or `.github/copilot-instructions.md`, read after `AGENTS.md` in each folder
  with the same symlink and UTF-8 rules. Extra names must be relative paths with no `..`, `.git`,
  root or drive, checked when saved and at every run. A trusted project's `instructions` section
  now applies and only lowers the limits or keeps fewer of your extra files; it cannot add one.
  A `project.json` whose `instructions.extra_files` names `AGENTS.md`, `.`, `.git` or a `:` is now
  invalid; fix it and trust the project again. A file larger than the total limit is skipped, so
  keep the total at least as large as the per-file limit.
- Settings saved by this version carry `subagents`, `instructions` and
  `contextBudget.maxOutputBytes`, which an earlier `bw` rejects; to go back, remove those keys from
  the CLI settings file.

- Command tools in `tools.json` can take JSON on stdin (`"stdin":"json"` writes
  `{"arguments":{…}}`), declare a typed argument schema (`parameters`: string, number, integer or
  boolean properties with `enum`, `description` and `required`), or send an HTTP request
  (`"kind":"http"` with `url`, `method`, `headers` and `"body":"json"`). HTTP tools ask for Network
  approval, matched on the URL's host; values fill only the path or query, percent-encoded; header
  values come only from the credential store as `{secret:NAME}`, saved with
  `bw tools secret <tool> <NAME>` for the URL's origin; responses are capped at 64 KiB with no
  redirects, proxies or compression. A tool that sends headers or a JSON body is allowed only by
  a Network rule naming it (`"tool":"<name>"`), not by a host allow. Existing `tools.json` files
  load unchanged.
- Project settings in `.blackwall/project.json`. A project's deny and ask rules apply right away;
  its allow rules (and its `mcp`, `limits` and `instructions` sections, validated now; `mcp` is
  used from this release) wait until the owner trusts the file. Trust is saved in `trusted-projects.json`
  as the canonical folder and the SHA-256 of the exact bytes shown, and any edit untrusts it.
  Review and trust with `bw project status|trust|untrust` or the desktop banner and dialog, which
  shows a diff when the file changed; untrust from **Settings → Security → Trusted projects**.
  The file must be a regular file of at most 64 KiB, never a symbolic link; an unusable file
  makes every action ask. The review lists each rule with badges for broad ones and shows
  invisible Unicode format characters as `\u{XXXX}`; project allow rules for the owner's MCP
  servers or for every file are refused, and edits under `.blackwall/` always ask.
- `"mode": "ordered"` for rules files: the first matching rule in the file decides. The default
  `strictest` keeps today's meaning, and across files a deny or ask always beats an allow.
- Policy audit records gain `rule`: the deciding rule's source, index and mode, or `null`.

- Connect to MCP servers over streamable HTTP (protocol 2025-06-18). A server in `mcp.json` takes a
  `url` instead of a `command`, plus `headers` whose values name saved secrets as `{secret:NAME}`;
  literal values are refused. Add one with `bw mcp add NAME --url URL [--header NAME=SECRET]` or the
  new **Command | URL** switch in the desktop MCP screen. Header secrets are bound to the origin they
  were saved for and never sent elsewhere; a server whose origin changes loses its approval rules.
  URLs must be https (http only on 127.0.0.1 or ::1), only same-origin 307/308 redirects are
  followed, proxies are never used, replies are capped at 1 MiB, and server-sent requests and
  queued messages are bounded. OAuth sign-in is not supported yet. Server names starting with `p-`
  are now reserved for project servers: an existing one is skipped with a warning to rename it.
- A trusted project's `mcp` servers start with each run as `p-<name>`: URL only, no secrets or
  headers, and only on public addresses (private, loopback and link-local hosts are refused, and
  the checked address is pinned). Their approvals say "defined by this project", and Always allow
  saves a project-only rule.
- Distinct exit codes for `bw run` and `bw resume` with a prompt: 0 complete, 1 error, 2 usage,
  3 completed with a denied approval, 4 step limit, 5 model or network error, 6 context budget,
  7 time limit, 130 interrupted. The `--json` result line adds `reason`, `denied` and
  `usage {input, output}`, and `--timeout SECS` (10–86400) bounds the whole run. A run that
  completes after denying an approval now exits 3 instead of 0. Unknown or repeated flags are
  usage errors, `--timeout=SECS` also works, and a new `model_usage` event reports each model
  call's tokens so failed and stopped runs still count them.
- Detect the context window of any local server, not only Ollama on port 11434: Ollama on any port,
  llama.cpp (`/props`), vLLM (`max_model_len`), and LM Studio (`loaded_context_length`). The
  detected server shows in **Settings → Model connection**, `bw status`, and `/context`.
- Keep model reasoning out of answers: `<think>` blocks (even split across stream chunks) and the
  `reasoning` / `reasoning_content` fields go to a separate bounded buffer, are never sent back to
  the model, and show collapsed as "Thought for Ns". Traces keep reasoning only with content capture.
- Opt-in reading of `<tool_call>` blocks a model writes as text, per connection ("This model writes
  tool calls as text", or `bw config set text-tool-calls on`). Off by default; blocks inside code
  fences and calls for tools that were not offered stay text, and every call read from text asks
  for approval even when a saved rule allows it.

- Export traces and usage metrics over OTLP/HTTP (JSON or protobuf) from **Settings → Security →
  Telemetry** or `bw config set otlp-endpoint|otlp-protocol|otlp-signals|otlp-include-project`.
  Off by default; https is required except for loopback; redirects are not followed; header values
  are kept only in the credential store, bound to the endpoint origin they were entered for
(`bw telemetry header <name>` reads one from stdin). Export
  sends exactly what the local files hold, after the whole-run scrub, leaves project names and
  session IDs out of metrics unless allowed, and never blocks a run. The Usage page gains an
  **Export** status card, and **Send test span** (`bw telemetry test`) reports the HTTP status.
  See [Export](docs/TELEMETRY.md#export).

- Set how long usage metrics are kept: 30 to 3,650 days or forever (default 400), in Settings or
  with `bw config set usage-retention-days`.

- List the periods when recording was off in the Usage page's gaps line and in `bw usage`.

- Record usage metrics with OpenTelemetry: duration for every model call and token usage when the
  server reports it (compaction summaries included), tool calls by outcome, and runs by outcome, written as OTLP JSON lines to `metrics/YYYY-MM-DD.jsonl`
  in the data folder and kept for 400 days. Metrics never include prompts, answers, tool arguments,
  or paths. See [Telemetry](docs/TELEMETRY.md).

- Add a Usage page under **Settings → Workflows** and `bw usage [--days N] [--utc] [--json]`, by
  local day:
  tokens by project, model, and day, cached and reasoning tokens where the server reports them,
  run success, median output speed, tokens saved by compaction, top tools, and what the range
  cannot show. Session drill-downs use neutral IDs only. **Export…** copies the range's metrics
  and traces as OTLP JSON. Metrics now record the workspace folder's name (never its path) and the
  conversation ID.

- Trace desktop Chat runs and subagent runs. A `spawn_agent` child appears in **Runs** and
  `bw traces show` nested under the tool call that started it.

- Enable Web access in desktop Chat to search the public web and read pages without selecting a
  project. Chat uses the same request approvals and network protections as Agent, and retains web
  results in conversation context.

### Fixed

- First-run setup now keeps connection failures neutral until a server is chosen, excludes
  embedding models, and asks which chat model to use before starting a conversation.

- Radio buttons sit inline with their labels again, and checkbox rows line up (Memory → Advanced,
  Runs, Recall). Dropdowns use the app's own style with a chevron instead of the grey Linux system button. The lock
  screen's Try again button has space above it. Linux menus and switchers show "Local AI
  assistant" as Blackwall's description.

- Before setup, `bw status` shows the model and endpoint as not set instead of printing defaults
  such as `llama3.2` as if they were configured.

- Unsandboxed shell commands keep variables such as `GTK_THEME` when you have `APPDIR` set
  yourself. Only a running AppImage, which sets both `APPDIR` and `APPIMAGE`, has its variables
  removed, including values that are not valid text.

- Python and other runtimes now start from the Linux AppImage. Programs it launches no longer get
  the AppImage's own `PYTHONHOME` and library paths. A Python MCP server that fails to start now
  shows why in the tool card's details.

- A run now reads `.blackwall/project.json` once, so its rules and its limits always come from the same version of the file, even if the file changes while the run starts.
- A subagent that runs on a command tool (`runs_on`) now gets an `execute_tool` span in its trace, like any other tool call, with the same redaction rules. Its approval events are recorded on that span.
- Usage no longer reads an oversized line in a local OTLP metrics file into memory. A line over
  4 MiB is counted as unreadable and skipped, and the next line is still read.
- Keep credentials out of the approval audit log when a command passes them as headers such as
  `X-Api-Key: …`, `Authorization:…`, cookies, or custom token headers.

- Stop simultaneous saves from silently overwriting each other. Reviewed file edits, MCP server
  changes, and skill proposal approvals now hold a lock shared by the desktop app and `bw`, so a
  conflicting change is reported instead of lost.

- Keep an MCP server's saved credentials when removing it fails; they are deleted only after the
  server is removed from the configuration.

- Let readers scroll up during a streamed response without being pulled back down. Resume
  automatic scrolling when they return to the bottom of the conversation.

- Keep advanced model connections when adding another endpoint. Switch among saved services in
  Settings, restore each service's selected model, and retain browser connections after restarting.
  Manage a saved service's access key without switching away from the current model.

- Show all slash commands and enabled skills in a scrollable menu above the chat input. Filter
  suggestions as you type, keep keyboard selections visible, and clear the menu when switching chats.

- Open Blackwall on Linux without a system keyring. A data folder with no app lock opens, and a
  model access key works for the session only, marked "Not saved: no system keyring". Saving a
  lock, telemetry header, MCP secret or HTTP tool secret says to start GNOME Keyring or KWallet and
  restart Blackwall instead; `bw` shows the same note for telemetry headers, MCP secrets and tool
  secrets. Pairing and relay keys are refused until a keyring is running.

- Keep an app lock enforced when the Linux keyring is unavailable. Blackwall records the lock state
  in the data folder with empty, non-secret marker files: `app-lock-enabled` while a lock is on and
  `app-lock-disabled` when there is none. Without a keyring, a folder with a lock, or older data
  whose lock state Blackwall can't confirm, stays locked and asks you to start the keyring and try
  again. The desktop app cannot set a lock without a keyring. A dismissed or locked keyring prompt
  is an error with "Try again", not "no keyring".

### Changed

- Rule paths with `*` or `?` are now refused with an error naming the rule, instead of being
  accepted and never matching. Paths are plain prefixes: use `src`, not `src/**`.
- Chats are now stored with each message saved once. `bw` appends only what is new on each save;
  the desktop app appends when you send a message and rewrites the chat when a streaming answer
  finishes. The first open after updating backs up the database to `backups/blackwall-backup-1.sqlite3` (then `-2`,
  `-3`) and upgrades it once; with three backups present, Blackwall asks you to remove one first.
  Older Blackwall versions cannot open the upgraded data.
- Start child programs from a cleaned environment instead of Blackwall's own, which changes
  what they see. This covers shell commands, command tools, MCP servers, the git and browser-opener
  helpers (the Linux opener also gets the desktop-session variables it needs), and listening-port
  discovery (`lsof`, `netstat`), on every system. MCP servers, command tools and the other helpers
  keep `HOME`, `LANG`, `LC_*`, `TERM`, `TMPDIR`, `USER`, `LOGNAME`, `SHELL`, `TZ`,
  `XDG_RUNTIME_DIR` and a cleaned `PATH`; sandboxed commands get less. Unsandboxed shell commands
  keep your whole environment (`SSH_AUTH_SOCK`, `KUBECONFIG`, `AWS_PROFILE` and so on) minus
  Blackwall's own keys, loader and code-injection variables, and the AppImage runtime's. An MCP server or unsandboxed command tool that relied on an inherited
  variable (a token, a proxy setting, a custom config path) stops seeing it. To migrate, add its
  name to the new `env` list in `mcp.json` or `tools.json`, such as `"env":["GITHUB_TOKEN"]`. `env`
  is owner-only (not allowed in project servers), is refused on URL servers and HTTP tools, is
  ignored for sandboxed commands, and cannot name `PATH`, loader-injection variables or
  Blackwall's own secrets.
  A bare program name in `mcp.json` or `tools.json` is now found only in that cleaned `PATH`, so a
  project folder on your `PATH` (direnv, `node_modules/.bin`) cannot supply the program. `BROWSER`
  is no longer passed to the browser opener and cannot be listed in `env`.

- A first launch with no saved connection lists the servers found on this computer and connects
  only to the one chosen, instead of connecting to the first that answered.

- **Settings → Model connection** starts with **Set up a connection** and the servers found on this
  computer, then a visible **Server address** field with **Connect** (no longer under Advanced),
  then saved connections and their options.

- When a model server rejects a request with an explanation, such as Ollama running out of memory
  to load a model, the chat error shows it, clipped to 300 characters with control characters
  removed and credentials redacted. Error kinds in traces are unchanged.

- Write traces through the OpenTelemetry SDK. A run's spans are held until it ends, then filtered
  or scrubbed as a whole before they are written, so a secret seen late in a run is removed from
  earlier spans too. With content capture off, failures show only their error class, and only
  metadata attributes are kept. Existing trace files still list and open.

- Show tool calls where they occur in the response, grouped into collapsible sections with
  individual arguments and results available on expansion.

- Move MCP servers and Runs into Settings. Group settings into General, AI capabilities, and
  Workflows, with search and optional alphabetical sorting.

- Remove the bottom status bar and the sidebar connection indicator to reduce duplicate model,
  context, and conversation mode information.

- Move Memory and Skills from the sidebar into Settings. Closing either panel returns to Settings.

- Use the Chat and Agent selector beneath Blackwall in the sidebar; remove the duplicate selector
  above the conversation.

- An instruction file over the per-file limit is now cut at the limit on a character boundary,
  with a warning naming it and a note after its guidance, instead of being skipped. Guidance lines
  that look like the prompt's `BEGIN GUIDANCE`, `END GUIDANCE`, `Source:` or `Scope:` markers are
  prefixed with `> `.

- Run command tools from `tools.json`, including a subagent's runs-on tool, outside the sandbox by
  default, on every system including Windows, in the project folder with the allow-listed
  environment plus the tool's `env` names, whatever the Commands profile is. They always ask: an
  owner allow rule or "allow identical action this run" no longer skips the prompt, and a deny rule
  still denies. The prompt says the program can run code from this project and use your files and
  network, and shows the resolved program and the names (never values) of the `env` variables it
  gets. The audit log records the program as `program`, with `tool`, `isolation`, `args`, `env`
  names and `stdinSha256`, and marks an allow rule that did not skip the prompt
  `"overridden": true`. An absolute program inside the project is refused, for command tools and
  MCP servers, and so is a program that changed after you approved it. To keep the old behavior,
  choose
  **Command tools → Inside the Commands profile** (Memory → Advanced · Subagents) or
  `bw config set command-tools-isolation sandboxed`; a project file cannot set it. `bw run --json`
  denies every prompt, so it cannot run an outside tool even with an allow rule; use `sandboxed`
  there.

### Security

- A deny `file` rule now blocks reading, listing, and searching its path, not just editing it, so
  `deny file .env` keeps the agent out of `.env`. Listings, searches, and `/review` leave denied
  files out, and the audit log names the rule as it does for a denied edit.
- `bw run --json` now counts actions an owner or project deny rule blocks, and calls to tools the
  model was not offered, in `denied`, so those runs end with reason `denied` and exit 3 instead of
  reporting success. Each such `tool_result` event has `"denied": true`.
- Update `tauri-plugin-updater` to 2.14.1, which moves `zip` from 4.6.1 to 9.1.3 to fix
  [GHSA-m665-v7pm-7grj](https://github.com/advisories/GHSA-m665-v7pm-7grj) (inconsistent zip
  headers could overwrite files). The updater only uses `zip` on Windows.
- `bw` no longer prints terminal escape codes from model or tool text. Approval prompts, streamed answers, tool errors, instruction warnings and subagent summaries drop control characters and show bidi characters as `\u{XXXX}`, so a delegate request can't repaint the approval prompt.
- A model server's error message can no longer show your API key: the key sent with the request
  is removed from the error before it is shown, saved or exported, and quoted keys are caught too.
- An MCP tool whose name contains one of its server's secrets (for example a saved header value)
  is now skipped with a warning that does not repeat the name, so the secret never reaches the
  model, traces or metrics.
- A trusted project's `command` allow rule (for example `"command": "github__delete_repository"`)
  could allow one of your own MCP or command tools. A project's allow rules now only ever allow
  its own `p-` servers.
- Update `ctutils` to 0.4.4 to fix non-constant-time operations reported in
  [RUSTSEC-2026-0385](https://rustsec.org/advisories/RUSTSEC-2026-0385.html).

## [0.1.5]

Milestone: [v0.1.5](https://github.com/AlecBhamani1/Blackwall/milestone/2).
Release preparation and acceptance limits: [0.1.5 notes](docs/releases/0.1.5.md).
Publication is pending the checked production promotion.

### Added

- Import skills written for Claude Code, Codex, and other agents in the `SKILL.md` format, with their
  scripts and reference files, from the desktop Skills panel or `bw skills`. Skills in their usual
  folders are listed, and nine suggested skills from the official Anthropic and OpenAI repositories
  and Matt Pocock's skills download at reviewed versions. Every import is scanned locally for hidden
  instructions and data leaks, and a skill with findings needs Allow anyway.

- Choose read-only (default), workspace-write, or explicitly unsandboxed command execution in
  desktop Agent mode and CLI settings. macOS Seatbelt enforces filesystem restrictions and
  default-off network access for shells, command tools, and descendants; protects application data,
  credentials and project metadata; and fails closed if unavailable. Restricted execution on other
  platforms fails closed until a backend is supported. Existing CLI settings migrate to read-only.

- Inspect Git changes with `bw review` and CLI/desktop Agent `/review`, using exclusive working-tree, commit,
  or base-branch targets. Return prioritized findings separately from unverified hypotheses,
  with bounded untracked text and JSON output. Review runs without model tools or write access.

- Review agent runs in the desktop **Runs** panel: recent runs with their model and tool calls,
  tokens, and failures, each run as a tree of steps with any captured content, and settings to turn
  traces and content capture on or off.

- Record a local OpenTelemetry trace of each CLI and desktop Agent run, with model and tool spans,
  token counts, errors, and approval requests. `bw traces` lists recent runs and `bw traces show`
  prints one as a tree. Traces are on by default and keep the newest 200; prompt, answer, and tool
  text is recorded only with `bw config set trace-content on`, redacted and size-limited.

- Search saved desktop and CLI message text locally with `bw sessions search`, `/recall`, and desktop
  Search history. Filter by project and updated date, open referenced messages without changing
  their saved mode, and optionally enable a bounded, project-scoped agent recall tool. Existing
  histories migrate in small batches; edits and deletions remove their old search excerpts.

- Load scoped `AGENTS.md` and `AGENTS.override.md` guidance in CLI and desktop Agent tasks,
  with bounded discovery, visible source lists and warnings, and a reviewed `/init` proposal
  that preserves existing project instructions.

- Review agent-proposed preferences, project conventions, and environment lessons in desktop Memory
  or `bw memory`. Proposals show their source and replacement text; approval, editing, rejection,
  and forgetting persist across clients. Scoped injection stays bounded, disabled memory blocks
  learning, and guest chats never receive owner memories.

- Budget model requests for tool definitions, generated output, and a safety reserve. Add `/context`
  and `/compact` to the CLI and chat composer, with optional automatic compaction, retained original
  transcripts, and separate resumable checkpoints that never restore historical approvals.

- Add a one-line installer for the latest development Blackwall CLI, available from any directory.

- Configure the Agent iteration limit and shell command time limit next to the context controls in
  desktop Memory and `bw config`. Defaults remain 20 iterations and two minutes.
  
- Decide agent actions with owner-defined allow, ask, and deny rules in `~/.blackwall/policy.json`.
  Projects cannot add rules, chained shell commands never match an allow rule, invalid files fall
  back to asking, and every decision is recorded in a redacted local audit log.

- Run owner-defined command tools from `~/.blackwall/tools.json` in Agent mode, such as another
  assistant's command line, without a shell and behind Exec approval and owner rules.

- Connect local MCP servers listed in `~/.blackwall/mcp.json` to Agent runs. Their tools are named
  by server, can be limited per server, and every call needs Exec approval or an owner rule.

- Load enabled skills on demand in Agent runs. The prompt lists each skill's name and description
  within the shared budget, and the model reads a skill's instructions with `read_skill`, so far
  larger libraries fit.

- Propose skills from successful work with `/learn`. Proposals keep their source, never load until
  approved with `bw skills` or in the desktop, and cannot overwrite a skill the owner changed after
  the proposal.

- Review proposed skills in the desktop **Skills** dialog: see the source and the saved skill, edit
  the proposed description and instructions, then approve or reject.

- Run CLI tasks headlessly with `bw run --json`, printing agent events as JSON lines and denying
  any action that owner rules leave to an approval prompt.

- Add, list, enable, disable, and remove MCP servers with `bw mcp`, without editing `mcp.json` by
  hand.

- Keep MCP server access tokens in the platform credential store. `bw mcp secret` saves one from
  standard input, `mcp.json` holds only its name, and the server receives it when it starts.

- Manage MCP servers and their secrets from **MCP servers** in the desktop sidebar, sharing the
  CLI's validation and credential store.

- Always allow an MCP tool, or every tool from its server, from its approval card, in one project
  or every project. The choice is saved as an owner rule with new `tool` and `project` fields, and
  **MCP servers** lists and removes these rules.

- Suggest common MCP servers in the desktop **Add server** form. Picking one fills in its name and
  a pinned `npx` command; the list ships with the app and is not offered on Windows.

### Fixed

- Update DOMPurify to 3.4.17 to include upstream sanitization security fixes.

- Send Blackwall's instructions as one system message, first. Chat templates such as Qwen's reject
  any other system message, so Agent runs failed with HTTP 400 on vLLM servers.

- Flush each policy audit record before the decision returns. Tokio finishes file writes in the
  background, so a record could be missing when the log was read right after a decision.

- Update the TLS dependency to include upstream security fixes.

- Find programs installed with Homebrew or npm for command tools, MCP servers, and shell commands
  when the macOS app is opened from the Dock, using the login shell's `PATH`.

- Save MCP server names such as `Context7` or `My Jira` in lowercase with hyphens instead of
  rejecting them, say which part of an MCP server is invalid, and show startup warnings, such as a
  server that did not start, without expanding the project instructions entry.

- Use portable paths in instruction source lists and directory scopes on Windows.

- Size requests to the context window Ollama loaded the selected model with, reserving a quarter
  of it (up to 32,768 tokens) for output, so long replies are no longer cut off at 4,096 tokens.

- List each discovered model once in the desktop picker, first-run setup, and browser client, so
  Ollama models with several runner variants no longer make the picker unavailable.

## [0.1.4] - 2026-10-01

Milestone: [v0.1.4](https://github.com/AlecBhamani1/Blackwall/milestone/1).
Release details and acceptance limits: [0.1.4 notes](docs/releases/0.1.4.md).

### Added

- Interactive `bw` chat and `bw setup`, with persisted CLI settings and `/commands`, `/model`,
  `/endpoint`, `/settings`, `/web`, and `/memory` controls. Resume restores the saved Agent project;
  graceful cancellation retains partial answers, and environment access keys stay scoped to their origin.

- Run Blackwall on Windows 10/11 (x64) and Linux (x86-64 and ARM64, including NVIDIA DGX Spark),
  with installers and signed in-app updates. Access keys, pairing credentials, and the app lock use
  Windows Credential Manager or the Linux Secret Service, and Agent-mode shell commands run in
  Windows PowerShell on Windows. Pairing and guest links work between any mix of macOS, Windows,
  and Linux computers.

- Select Stable or Beta in App updates to test signed builds from `partial` before production,
  with a saved channel preference and an explicit return to stable.

- Browse project files in a right sidebar in Agent mode, with expandable folders, name and
  content search, refresh controls, and bounded text previews inside the selected workspace.
- Render inline and display LaTeX equations in assistant messages with bundled offline fonts.
- Drop screenshots or files anywhere in the conversation to attach them, and use `@` to
  select and reference files from the current chat, including names with spaces.

- A protected `partial` integration branch and checked production promotions, tracked by version
  milestones and release notes, with automatic versioned publication after production CI.
- Both-architecture desktop build checks before promotion, dependency vulnerability review,
  weekly dependency maintenance, issue/PR templates, and reproducible GitHub repository policy.

- Copy buttons on individual Markdown code blocks, with success and failure feedback.

### Fixed

- Keep Blackwall running on macOS when the main window is closed, preserving active work and
  restoring the window from the Dock. Quit and Cmd+Q still exit the app.

- Stop Windows shell descendants on cancellation, timeout, and command completion even when
  their parent PowerShell process has already exited.

- Separate Chat and Agent histories, lock conversation mode and project after the first message,
  restore each Agent conversation’s project, and clear the project when starting a new chat.

### Changed

- Reviewed development pull requests enter a checked merge queue before reaching `partial`.
- Update Rust dependencies while preserving existing stored hashes; upgrade Vitest and coverage
  together to version 5, and retain TypeScript 6 until the Svelte checker supports its successor.

## [0.1.3] - 2026-09-24

- Add signed in-app updates backed by continuous macOS builds from GitHub `main`.
- Explicitly bundle the Blackwall logo for the app, Dock, and installer icons.
- Replace direct private-network guest sharing with a configurable, self-hosted HTTPS relay and
  outbound-only desktop connection.

### Added

- Persistent computer pairing with five-minute invitations, matching-code host consent, per-device
  Keychain credentials, saved computer cards, automatic tunnel recovery, and durable individual revocation.
- Relay pairing capability detection, bounded exchanges, and a shared eight-request guest/device budget.

- Guided local/remote/invitation setup, local service detection, and cancellable Ollama starter downloads.
- Named saved connections and origin-scoped model credentials in macOS Keychain.
- Native SQLite sessions and attachment retention, legacy migration, full-text memory, and JSON export.
- Optional Argon2 passphrase lock with Keychain verification data and stopped work on lock.
- A bounded agent runtime, project file tools, reviewed edits, approved shell/web actions, and restricted child tasks.
- Persistent Markdown skills and memory management screens; real CLI run/resume execution.
- Access-key status, verified model-key replacement, and explicit model/relay-key removal in Settings.
- Up to four named guest invitations with independent revocation and Keychain-backed relay tokens.

### Fixed

- Keep a stable pairing address through failed saves; serialize activation with locking, retain
  authorization until database workers finish, and complete lock cleanup after caller cancellation.

- Persist relay address ownership across host disconnect and process restart, rejecting offline takeover.
- Restore the same active invitation after relay restart without replaying completed prompts.
- Enforce expiry on unfinished relay requests; add heartbeat and write deadlines to stalled tunnels.
- Preserve active invitations if a cancelled new-share operation interrupts expired-link cleanup.
- Add a persistent relay deployment volume and update its Rust builder to the locally verified compiler series.

- Failed connection replacements no longer discard a working connection or its model catalog.
- Deleting active conversations and stopping old streams no longer resurrect chats or contaminate newer turns.
- Failed native writes retain their order for retry, including subsequent deletions.
- Sharing resolves saved model keys; environment credentials are restricted to their configured origin.
- Approval cancellation invalidates pending actions; stale file edits are rejected.

- Interrupted agent runs now finalize visible tool and child activity.
- Failed save flushes during locking preserve composer drafts; named connections retain their own model choice.
- Host loss before a guest response now returns a readable error; abandoned responses release relay state and capacity.

### Existing foundation

- A native Tauri desktop chat connected to a user-configured OpenAI-compatible model endpoint.
- Streaming responses, model discovery and selection, stop controls, and locally saved recent chats.
- Image, text, and file attachments through the picker, drag-and-drop, and pasted images, with
  bounded size and count limits.
- A focused Svelte interface with responsive navigation, Markdown rendering, and a tokenized dark
  visual system.
- A typed Rust protocol and a single typed desktop event channel for UI/core communication.
- Hosted guest sharing with expiring QR/browser-link invites, a pinned model, authenticated
  streaming, automatic host reconnects, and an immediate host-side stop control.
- Repository quality gates for formatting, linting, testing, dependency policy, and commit
  conventions.
