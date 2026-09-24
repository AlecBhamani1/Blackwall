# Blackwall delivery plan

Updated: 2026-09-21. This document tracks execution of the product completion work.
`PLAN.md` preserves the original feature specification; this file defines delivery order,
acceptance criteria, review gates, and current evidence. A checkbox means verified work,
not an intention or a mockup.

Latest acceptance: September 21 verified native chat recovery with a write-protected database,
a genuinely full disposable volume, and a read-only volume. Fixed New chat hiding unsaved
content/export and storage errors incorrectly blaming the model. Export, retry, relaunch,
record preservation, and no replay passed. All 81 frontend tests pass, Svelte diagnostics are
clean, and all unsigned bundles rebuilt. Pairing/app-lock storage failures and Keychain
mutation denial remain open. See `NATIVE_ACCEPTANCE.md`, “Storage failure recovery — September 21.”

Previous acceptance: September 17 added explicit pending credential guidance, accurate setup
save-failure recovery, and keyboard focus through removal/cancellation/results. All 77 frontend
tests pass, Svelte diagnostics are clean, and unsigned app bundles rebuilt. Native save/removal
and focus passed; a scoped private-keychain probe verified rejected-update preservation and retry.
Interactive write/removal denial in the packaged app remains an open gate; see
`NATIVE_ACCEPTANCE.md`, “Keychain writes and removal — September 17.”

## Product outcome

A person without server administration experience can install Blackwall, connect a model,
get an answer, recover from a connection failure, and understand what the app may access.
The agent can then perform useful project work with visible, interruptible actions and
explicit permission for changes. Memory, skills, and sharing are understandable and manageable.

## Accepted first-release scope — 2026-09-10

The user accepted the focused-release recommendation. Freeze unrelated additions. The first
public release must make local setup and remembered remote access understandable, preserve chats
and attachments, handle interruptions, and expose manageable access and revocation. Existing
agent/memory/skill features must pass their acceptance checks if included in the release.

Delivery sequence:

1. **Connection foundation:** preserve relay ownership across disconnect/restart, reject takeover,
   recover without silently replaying prompts, bound stalled connections, and verify expiry.
2. **Persistent pairing:** implement the contract in `PAIRING_PLAN.md`, including one-time exchange,
   host consent, Keychain device credentials, relaunch recovery, and individual revocation.
3. **Connection polish:** paired-computer names and availability, clear offline/revoked recovery,
   keyboard navigation, and reviewed desktop/narrow layouts.
4. **Release candidate:** run packaged-app persistence, credential, lock, attachment, real-model,
   relay restart, and separate-network acceptance. Fix and retest every material finding.
5. **Private beta, then public release:** operate the default relay, configure Apple signing and
   notarization, validate installation/upgrade, close beta issues, and publish only after the gates pass.

Deferred beyond this release: Postgres, automatic learning, agent-generated skills, additional
search providers, and richer web extraction. Open boxes for these in the historical milestones
are roadmap tracking, not first-release blockers.

## Baseline before this work

- Implemented: macOS Tauri shell, Svelte chat, model discovery, streaming, attachments,
  recent chats in webview localStorage, expiring relay invitations, signed updater support.
- Agent and subagent demo components are presentation examples, not functioning tools.
- Core currently exports protocol and sharing modules. Agent, memory, skills, authentication,
  and durable session implementations are absent.
- The owner currently supplies a model address and separately deployed relay configuration.
- Model installation and lifecycle are external to the current app.
- CI definitions exist; their presence does not prove the latest release is healthy.
- Apple notarization and a hosted default relay cannot be claimed from repository code alone.

## Delivery rules

1. Preserve working chat, attachment, sharing, and updater behavior.
2. Use the existing theme, typography, spacing, and component conventions.
3. Complete both UI and backend for an advertised capability; never present a demo as live work.
4. Keep endpoint addresses, deployment tokens, and transport vocabulary in advanced settings.
5. Test state transitions, persistence, cancellation, and trust boundaries with deterministic fixtures.
6. Inspect rendered desktop and narrow-width layouts, keyboard focus, empty states, and errors.
7. Never automatically install software, download model weights, or change remote computers at startup.
   These actions must originate from an explicit, descriptive user control in the app.
8. No cloud service or public release is marked delivered until deployment and real-device checks pass.

## Milestone 1 — Guided setup and first answer

### Experience

- [x] First-use setup in the conversation area, with a clear title, short explanation, and routes
  for this computer, another computer, and an invitation.
- [x] A returning user with a working connection reaches chat without repeated onboarding.
- [x] A returning user with a failed connection gets an actionable recovery entry point.
- [x] Detect local model services using a small fixed loopback candidate list, never a LAN scan.
- [x] Show detected service, installed models, and a single connect action.
- [x] Distinguish missing service, service without models, invalid address, authentication,
  unreachable computer, and timeout errors without exposing raw response bodies by default.
- [x] Guide installation through the official provider download; recheck after installation.
- [x] Offer a conservative local model choice with download progress and explicit user initiation.
- [x] Keep custom OpenAI-compatible connection input under advanced/manual setup.
- [x] Validate invitation scheme/shape, handle expiry/revocation, and never persist invite secrets.
- [x] Save a replacement connection only after a successful model discovery.

### Engineering and acceptance

- [x] Native discovery is bounded, asynchronous, and does not send configured remote credentials
  to unrelated local services.
- [x] Downloads use a fixed local provider endpoint, bounded streamed progress, cancellation,
  explicit errors, and no arbitrary command execution.
- [x] Failed/stale probes cannot replace a later successful choice.
- [x] A fresh profile can connect an existing local service without typing a URL.
- [ ] No-model and offline states remain usable with keyboard-only navigation.
- [x] Desktop and narrow-width visual review, frontend tests, Rust tests, build, and lint pass.

## Milestone 2 — Remembered computers, pairing, and sharing

- [x] Named direct connections saved after validation, with origin-scoped model credentials.
- [x] Persistent paired-computer identity and per-device credentials.
- [x] Remember and reconnect to the last working connection; retry must not resend a prompt.
- [x] Pairing protocol: independently random secret, short expiry, bounded attempts, host consent,
  revocation, and no exposed upstream URL or model credential.
- [x] Distinguish persistent owner pairing from temporary guest invitations.
- [x] Friendly paired-computer label with model availability and reconnect state.
- [ ] Default relay configuration when an operated service is available; self-hosted override
  under advanced settings.
- [x] One primary Share action, expiry, copy/QR, revoke, and clear host-must-stay-awake guidance.
- [x] Multiple invites/guests with individually revocable access and bounded request capacity.
- [x] Loopback checks for revoked links, overload, host disconnect/reconnect, rejected takeover,
  and cleanup when guests stop reading.
- [x] Actual relay-process termination/restart preserves ownership and the same active invitation;
  offline takeover is rejected and completed prompts are not replayed.
- [x] Real-time expiry ends an unfinished request and rejects reuse of the expired registration.
- [ ] Sleeping host and real-device network changes.
- [x] Keep HTTPS transport trust explicit; application-level E2EE requires a separate reviewed
  cryptographic protocol and must not be implied by a padlock or “private” label.

### External delivery dependencies

- A default relay needs an operator, domain, hosting credentials, and a usage/abuse budget.
- A computer-to-computer acceptance test needs a second machine and a separate network.
- Until the default relay is deployed, do not imply arbitrary short codes work globally.

## Milestone 3 — Durable local data and credentials

- [x] Versioned native configuration under `~/.blackwall`, bounded input, atomic writes,
  restrictive permissions, and corruption recovery that does not silently destroy data.
- [x] Durable session store with creation/update/list/load/delete and attachment persistence.
- [x] One-time migration of existing recent chats; retain the old copy until the native write succeeds.
- [x] Saving/deleting/switching conversations cannot resurrect deleted data or mix streams.
- [x] Clear persistence errors with a retry or export path.
- [x] Keychain-backed model credentials scoped to the selected origin and used by guest sharing.
- [x] Keychain-backed relay credentials, saved after successful registration.
- [x] Credential-management controls for current model/relay origins: presence, verified replacement,
  explicit removal, preserved previous key on failure, and no returned secret values.
- [ ] Search-provider credentials and provider selection.
- [x] Keep stored model keys out of returned UI state, SQLite preferences, guest URLs, and prompts.
  User-entered key drafts and temporary invitation URLs necessarily exist in their input/creation UI.
- [x] Implement app lock with passphrase hashing, unlock/lock, failed-attempt handling, and
  documented limits (a UI lock is not encrypted storage); hash and UI tests pass.
- [ ] Complete native Keychain and app-lock interaction acceptance.
- [x] Settings for export/delete, context budget, and workspace selection.
- [ ] Reopen after restart preserves messages and supported attachment contents.

## Milestone 4 — Agent execution, tools, and approvals

- [x] Framework-independent OpenAI-compatible model transport that handles tool calls and usage.
- [x] Bounded agent loop: model -> tool proposals -> approvals -> tool results -> next model turn.
- [x] Per-run cancellation, maximum iterations, timeout/output limits, and deterministic errors.
- [x] Workspace selection using a native folder picker; canonical filesystem boundary.
- [x] Read/list/search files, proposed edits with unified diff, and explicit write approval.
- [x] Shell execution with command/cwd shown, approval, timeout, bounded stdout/stderr,
  process-tree cancellation, and honest disclosure of sandbox limitations.
- [x] Approvals tied to the exact action and active run; deny/allow-once/remembered narrow policy.
- [x] No tool access for browser guests; host sharing remains model chat only.
- [x] Real tool transcript entries and inline approval cards with keyboard accessibility.
- [x] Restored sessions preserve visible tool history without re-executing actions.
- [x] CLI run/resume shares the same core runtime and permission policy.

### Acceptance tests

- Scripted model requests a file read, proposes an edit, receives denial, then completes.
- Allowed edit changes only the reviewed file; path traversal and symlink escape are rejected.
- Cancellation while awaiting approval causes no tool action.
- Cancellation during execution stops work and prevents late events entering another session.
- Repeated/malformed tool calls, output overflow, errors, and iteration limit terminate predictably.

## Milestone 5 — Memory, skills, web, and child agents

- [x] Local SQLite memory with migrations, search, bounded prompt injection, edit, and delete.
- [x] Distinguish user-approved facts from agent-generated suggestions; expose what will be remembered.
- [ ] Optional Postgres adapter behind explicit configuration; never required by onboarding.
- [x] User-managed skills: discover, validate frontmatter, view, enable/disable, create/update/delete.
- [ ] Agent-proposed skill creation and learning with explicit approval.
- [x] Bundled repository-orientation and bug-triage skills; invalid files reported without crashes.
- [x] Opt-in web fetch/search with host/purpose approvals, DNS/IP restrictions, redirect validation,
  time/body caps, and untrusted-content treatment.
- [x] Child-agent runtime with bounded concurrency, restricted inherited tools, parent cancellation,
  structured result/failure, visible nested transcript, and no recursive approval bypass.
- [ ] Acceptance: remember a user fact across relaunch, invoke a saved skill, approve one web request,
  run a child task, and verify parent receives its result.

## Milestone 6 — Review, hardening, and release

- [x] Review changed code for lifecycle races, stale responses, secret leakage, destructive defaults,
  accessibility, unnecessary abstraction, and consistency with documented behavior.
- [x] Fix findings, add regression coverage for behavior that could recur, and rerun affected checks.
- [x] `npm run verify` (type checking, tests, production build).
- [x] `cargo fmt --manifest-path src/Cargo.toml --all -- --check`.
- [x] `cargo clippy --manifest-path src/Cargo.toml --workspace --all-targets --all-features -- -D warnings`.
- [x] `cargo test --manifest-path src/Cargo.toml --workspace --all-features`.
- [x] `cargo deny --manifest-path src/Cargo.toml check`.
- [ ] Visual evidence for setup selection, detection, download, success, failure, chat, settings,
  approvals, and sharing; desktop and narrow window, focus and scrolling checked.
- [x] Update README, architecture, changelog, sharing documentation, and this checklist to match evidence.
- [ ] Apple Developer signing/notarization configuration and clean-machine installation check.
- [ ] Signed updater upgrade test against a previous installation.
- [ ] End-to-end checks against a real supported model, another computer, and cellular guest access.
- [ ] Publish only with available release credentials and the user's authorized release scope.

## Review record

Initial source review found:
- `configureEndpoint` saves an unverified address, overwriting a working connection.
- Removing the active conversation calls `newChat`, which can save the deleted conversation again.
- Finishing an old request can reset the run state while a newer conversation is streaming.
- The initial offline state gives provider jargon without a setup action.
- Model server authentication is process-global rather than bound to a connection origin.

### Implementation and review evidence — 2026-09-09

| Area | Implemented and checked | Acceptance still open |
| --- | --- | --- |
| Connection setup | Fixed local probes; install/recheck; starter downloads; named direct connections; invitation validation; preserve working configuration on failure | Real installer/download acceptance and persistent remote pairing |
| Data | SQLite transactions; session/attachment lifecycle; one-time migration; lazy loading; save retry ordering; JSON export | Packaged-app relaunch and disk-failure exercises |
| Credentials/lock | Origin-scoped Keychain model keys; verified replacement keys; Argon2 verifier; command gates; cancellation; UI clear after save flush | Native Keychain prompts, wrong-passphrase rate-limit exercise, forgotten-passphrase recovery |
| Agent | Bounded tool-call transport; read/list/search; reviewed writes; approved shell; cancellation and stale-edit protection | Real model tool compatibility across supported providers |
| Extensions | Editable memory/skills; opt-in web; up to three restricted children; real CLI | Postgres, automatic learning, agent-managed skills, richer web extraction |
| Sharing | Existing authenticated relay streaming; model pinning; Keychain model/relay credentials; four named links; individual/all-link revocation; revoke on lock | Persistent pairing, default relay, two-device/cellular acceptance |

Verification already completed during development:

- `npm run verify`: 54 frontend tests passed, zero type/accessibility warnings, production build passed.
- Rust checks: 48 tests passed (app 4, core 30, relay 10 unit + 4 process integration), including actual loopback relay streams,
  independent invite revocation, overload, host loss/reconnect, and abandoned response cleanup.
- `cargo fmt --check` and Clippy with warnings denied passed on the current implementation.
- Optimized native application and CLI builds passed earlier in development. The latest unsigned
  application bundle also built successfully after sharing and credential changes; unsigned native
  smoke-check results are recorded below.
- `cargo deny`: advisories, licenses, bans, and sources passed. Existing transitive duplicate-version
  warnings remain allowed by repository policy; no advisory exceptions were added.
- Live browser-development chat reached an available `gemma2:2b` service, streamed the requested
  “Blackwall connection test passed.” response, and returned to Ready.
- Orca rendered screenshots reviewed at desktop and 390px widths: setup, memory, skills, approvals,
  unlock, sharing, and access-key management. Download progress was reviewed with a controlled fixture; no model weights were
  downloaded for visual testing. Fixtures were removed from the source tree after review.
- Screenshots are retained locally in `/tmp/blackwall-review/`; these are review evidence, not release assets.

Concrete findings fixed during review:

1. Concurrent connection attempts could discard a previously working model catalog.
2. Deleted active conversations could be saved again by navigation or an old stream.
3. Cancelled streams could reset a newer run or append late events.
4. Save failures needed ordered replay so a later deletion would not be undone by retry.
5. Locking needed to preserve unsaved content if flushing failed.
6. Sharing needed the selected model's saved Keychain credential, with redirects disabled.
7. Native dialogs needed to recheck lock state after user selection.
8. Shell overflow could block on pipes; capped concurrent reads and process-group cleanup prevent that.
9. Nonzero shell exits now produce failed tool results while retaining captured output.
10. Child runs needed bounded concurrency, preserved result order, and restricted tools.
11. Settings patches needed a transaction to avoid losing concurrent panel changes.
12. Narrow approval buttons needed a deliberate stacked layout; download progress needed theme styling.
13. Failed agent requests now finish running tool/child indicators; restored interrupted sessions do likewise.
14. Unsent drafts survive failed lock flushes; controls are inert while lock preparation is in progress.
15. Named connections now restore their own selected model, and automatically named invites avoid duplicates.
16. Keychain controls ignore stale status results after switching services and preserve rejected replacement drafts.
17. Host loss before response headers now returns complete JSON instead of a broken HTTP connection.
18. Guest response cancellation now drops pending relay state and capacity even when the response stream
    never finishes normally; disconnect error delivery cannot wait indefinitely on a slow guest.
19. Connection and sharing controls precede advanced key/lock settings and app updates.

### Remaining work, in delivery order

1. Finish the remaining native interaction checks in `NATIVE_ACCEPTANCE.md`. Packaged pairing,
   chat, relaunch, revocation, app lock, real-model streaming, PNG/text attachments, and native
   JSON export passed on one Mac. Broader providers/formats and active guest/agent lock checks remain.
2. Complete native acceptance of the implemented persistent pairing flow: Keychain failure recovery,
   signed-upgrade credentials, two physical Macs relaunching, and full keyboard navigation.
   Same-Mac lock during approval and both isolated app processes relaunching have passed.
3. Complete sleeping-machine and network-change acceptance. Actual relay-process restart, retained
   ownership, offline takeover rejection, active-request expiry, loopback reconnect, overload,
   independent revocation, and abandoned response cleanup are covered.
4. Choose and operate a default relay. Operator/domain/hosting details were requested but have not been
   supplied; self-hosted deployment remains available. Do not invent a public service or publish an endpoint.
5. Keep search-provider expansion, richer extraction, Postgres, and automatic learning deferred.
6. Perform native packaged-app acceptance and separate-network tests; then sign, notarize, test upgrades,
   and publish under the chosen release scope. No release or deployment has been performed.

The full roadmap is **not complete**. Checked items describe implemented behavior with the evidence
above; open native/device acceptance gates must be completed before calling this a polished public release.

### Sharing follow-up

The desktop now manages four independent named invitations, each with its own relay connection,
credential, expiry, and two-request allowance (eight concurrent guest requests maximum per desktop).
The manager serializes creation and revocation. List/status calls omit raw invite URLs and QR data;
the UI keeps creation-time links only in memory and associates them with their own invite identifier.
A regression test verifies that revoking one link leaves another usable and that a fifth link is rejected.
Relay tokens are saved in Keychain only after the relay accepts registration; a storage failure revokes
the newly created invite. Existing model keys are still scoped to the upstream model origin.

### Packaged macOS smoke check

The unsigned application bundle at `src/target/release/bundle/macos/Blackwall.app` built successfully.
It was launched as a separate process alongside the installed application. Native initialization
returned an unlocked app, discovered the configured model, showed migrated conversation summaries,
and rendered the reviewed Settings ordering. The native folder picker opened and cancellation
returned to the unchanged workspace state. The test window was then closed.

Orca's desktop automation reported `window_not_focused` for coordinate-dependent controls and could
not verify text input, including after its supported restore attempt. Semantic button actions and
native picker cancellation were observable. Native prompt submission, attachment relaunch,
Keychain replacement/removal, and enabling/changing/unlocking the passphrase were **not** verified.
This is a limited packaged smoke check, not clean-machine or release acceptance. No installed app
was replaced, no update was installed, and no public release or relay deployment was performed.

### Connection foundation review — 2026-09-10

The accepted first-release scope is now recorded above. Optional feature expansion is deferred.
`PAIRING_PLAN.md` specifies the exchange, native persistence, UI states, and acceptance matrix;
its unchecked work remains required before advertising persistent paired computers.

Review found an offline ownership gap: removing a disconnected relay session also forgot its
host credential, allowing a different host to register the same known public address. The relay
now reserves addresses in a private SQLite registry using host-credential digests. Reservations
survive disconnection, expiry, and relay process termination; disk failure rejects registration
instead of silently discarding ownership. Concurrent database connections cannot both claim the
same address. The registry is capped at 100,000 permanent reservations and the deployment guide
explains backup, capacity, migration, and the single-process routing limitation.

Additional lifecycle fixes:

- Heartbeats and peer-idle deadlines detect silently stalled host connections.
- Socket writes and queued output have deadlines; the host loop no longer waits for itself to
  drain a full queue while replying to a ping.
- Active requests terminate at invitation expiry with an explanatory response.
- Explicit shutdown interrupts a pending reconnect handshake.
- Expired-invite cleanup preserves unrelated entries if its creating operation is cancelled.

Evidence: the actual relay executable is started, killed, restarted with the same private data,
then tested for offline takeover rejection and automatic desktop recovery of the original URL.
The upstream request count proves completed prompts are not replayed. Other process tests check
active-request expiry and a genuinely silent TCP host; the latter is disconnected at the heartbeat
deadline and the legitimate owner can reconnect. Ownership tests cover competing claims, corrupt
and future schema data, private permissions, symlink rejection, and absence of raw keys on disk.

The production Dockerfile now uses the locally verified Rust 1.98 series and provisions a private,
non-root-owned data directory. Compose retains it in `relay_data`. Linux core/relay tests and the
container build are added to CI; adding a job is not evidence that hosted CI has run.

Final checks for this stage: 54 frontend tests passed with zero Svelte errors/warnings; 48 Rust
tests passed across the full-workspace run and targeted follow-up tests; Clippy with warnings denied,
formatting, diff whitespace checks, and dependency policy passed. The latest unsigned macOS app
bundle rebuilt successfully. The production relay image built on Linux ARM64 using Docker, and
`deploy/relay/smoke.mjs` passed against that image, including replacement with its data volume retained.
Compose configuration and CI YAML parsed successfully. Linux x86_64 hosted CI has not run locally.
The smoke containers and their temporary volumes were removed. No public relay or release was published.


### Persistent pairing implementation and review — 2026-09-10

The focused release now includes the persistent pairing implementation. Host Settings creates a
five-minute invitation for a verified model. The client generates an independent device credential,
then both screens show a matching confirmation code. The host must explicitly confirm that code and
approve the exact candidate. Saved-computer cards expose connect/retry and individual removal.
Direct URL setup stays under advanced controls; temporary guest links keep their separate lifecycle.

Typed SQLite schema 2 records hold metadata and credential digests. Raw keys stay in separate native
Keychain services scoped to the full paired address. Host approval writes a visible pending record
before relay approval, then commits it active. Pending, revoking, revoked, and client records cannot
start host tunnels. Completed host records reconnect with their original address and a renewable
one-day lease. Locking signals tunnels synchronously before waiting on pending pairing network work.

Host removal records intent locally, stops the tunnel, and commits a durable relay tombstone before
showing access removed. Offline revocation remains visible and retries. Tests verify that removing one
device stops its active request, leaves another usable, and cannot be undone by stale credentials.
A single eight-request semaphore covers temporary links and paired devices together.

Review fixes included:

- Preserve the exact pending client credential after an uncertain response, but clear a definitively
  rejected new request so the person can try a different invitation.
- Let expired or unbound requests cancel locally; retain uncertain cancellation for recovery.
- Clear denied client drafts and show newly saved computers in the approval response immediately.
- Prevent a stale approval from reviving a locally removed device.
- Resolve model credentials through regular chat's origin-scoped Keychain/environment rules.
- Save device capacity checks and updates in one transaction; verify the schema 1 upgrade preserves
  existing conversations and rejects future data.
- Prevent abandoned manual access-key drafts from being applied to saved/paired computers and give
  saved connection buttons explicit accessible names.
- Keep creation secrets out of restored UI state and explain how to create a replacement invitation.

Protocol and UI verification evidence is recorded below. Remaining release gates are
native Keychain failure injection, lock/approval races in a packaged app, two real computers on
separate networks (including sleep/wake), an operated default relay, and signing/notarization/update
acceptance. No public relay or release has been published.


Pairing verification: 59 frontend tests passed, with zero Svelte errors/warnings and a successful
production UI build. Rust checks passed 59 standard tests across the workspace and targeted
follow-ups, including four real relay-process restart/deadline tests. An additional opt-in native
macOS Keychain test passed: two disposable devices on one relay retained independent client keys,
the host key used a separate service, invalid replacement preserved the existing key, and removing
one client preserved the other device and host credentials. Disposable entries were removed.
This checks real native storage; it does not substitute for packaged-app permission prompts or
fault injection at every approval write.

The production Linux ARM64 relay image built successfully. Container acceptance passed capability
detection, no model access before host approval, client receipt, authenticated model discovery,
relay replacement with retained ownership, takeover rejection, and revocation surviving another
replacement. Raw device/host/invitation/deployment secrets were absent from the ownership file.
Smoke containers and volumes were removed. Hosted Linux x86_64 CI remains unverified locally.

The UI was inspected at desktop and 390px widths, including host confirmation and saved-computer
removal; no horizontal overflow was present. Component tests also cover keyboard activation of the
confirmation checkbox, candidate-change reset, clearing invitation drafts, explicit removal, and
retry after an offline connection. Temporary visual fixtures were removed from the checkout.

Final artifact checks: the latest unsigned macOS app bundle rebuilt successfully after the review
fixes. Clippy passed with all targets/features and warnings denied; Rust formatting and diff whitespace
checks passed. The rebuilt app was not installed or published. Packaged native interaction gates
remain open as listed above.


### Native pairing failure and lock review — 2026-09-10

The next release gate now has automated coverage through the production approval/receipt lifecycle.
The review fixed unstable retry addresses after an initial save failure, premature unlocking during
connection cleanup, and authorization being released while an abandoned database worker was still
committing. Prepared host identities are now distinguished from relay-approved exchanges, preserving
cancellation of incomplete pairing. Paired and guest cleanup run concurrently, and cancellation of
the lock caller does not cancel cleanup.

The acceptance matrix in `PAIRING_PLAN.md` now separates checked automated failure cases from actual
OS/device acceptance. Fault tests inject key-save, pending-save, relay-before/after-acknowledgement,
and active-save errors. They reopen real SQLite records, verify a single retry identity, and reject
host tunnels for pending records. Additional tests cover client failure recovery, locking at each
approval boundary, abandoned approvals, a real database-open failure, an abandoned blocking write,
and stopping a tunnel while another operation holds the pairing mutex.

Native Keychain permission prompts, actual full/read-only storage, packaged two-Mac relaunch,
keyboard-only pairing, sleep/network transitions, default relay operation, and distribution checks
still need acceptance. The automated test fixtures use disposable folders and never use the normal
Blackwall data directory.


A final persistence review added a transactional state check: stale writers cannot undo local removal
or downgrade a committed activation. The regression uses two separate database connections to model
another app process holding old metadata. This check complements the relay's permanent revocation
ledger and the native per-process pairing mutex.

Verification for this gate: 70 standard Rust tests passed across the full-workspace run and
targeted follow-up after the transactional state check; the opt-in Keychain smoke remains excluded
from ordinary runs and its earlier result is recorded above. All 59 frontend tests passed, with
zero Svelte errors/warnings and a successful UI build. Clippy passed for all targets/features with
warnings denied; formatting and diff whitespace checks passed. The repository knowledge graph
was refreshed using its required local AST update.

The final unsigned macOS app bundle rebuilt successfully with these fixes. No installed app was
replaced, no real user storage was used by the fault tests, and no release was published.


### Packaged pairing acceptance — September 10–14

See [NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md) for the reproducible isolated host/client
builds, fixture setup, observed native results, fixes, and remaining gates. Native approval,
paired chat, both app processes relaunching with the same saved identity, host revocation,
fresh pairing, focus transitions, and text-attachment selection/send were exercised on one Mac.
Exact attachment contents survived in SQLite. The model was a deterministic local fixture.

Review fixed collapsing relay settings, countdown rounding, dropped focus, duplicate success
copy, and lost native error categories. Connection failures now provide safe recovery copy,
mark lost connections offline, and preserve failed turns without automatic replay.

Verification reached 65 frontend tests, zero Svelte diagnostics, 72 standard Rust tests across
the workspace and app follow-up, and clean Clippy. The final visual recheck is still open because
Orca's desktop service rejected the agent peer despite granted OS permissions. This is not
two-physical-Mac, separate-network, real-model, signing, updater, or public-release acceptance.


### Desktop access restored — September 14

The final packaged attachment-display and offline-recovery recheck passed after desktop access
was restored. The saved attachment rendered after relaunch; host loss produced Model offline
and actionable guidance; reconnect restored the saved computer without replay. Only a new
explicit message reached the fixture model. See `NATIVE_ACCEPTANCE.md` for the observed sequence.
Next: native app-lock/passphrase acceptance, then real-model and physical-device/network checks.


### Native app-lock acceptance — September 14

The isolated native host/client passed passphrase setup and mismatch rejection, interruption
of a paired stream, refusal of locked access, correct/incorrect unlock, restart while locked,
cancellation of pending approval, passphrase replacement across restart, and lock removal.
The saved pairing survived and reconnect did not replay interrupted work. Detailed observations
and limits are in [NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md#native-app-lock-results--september-14).

Review fixed false connected status after a failed saved reconnect and restored keyboard focus
on the unlock screen and after app-lock Settings submissions. All 68 frontend tests pass, Svelte has zero diagnostics, and the acceptance
bundles plus ordinary unsigned macOS app rebuilt. Native rechecks confirmed both fixes.

Next: real-model streaming, images/attachments and export, followed by physical two-Mac/network
acceptance. Full keyboard flow across the remaining screens, active guest/agent lock tests,
actual Keychain denial/storage failures, default relay operation, and signing/updater/beta gates
remain open. This is not a public-release approval.


### Real-model native acceptance — September 15

The isolated native apps passed local model discovery, Gemma pairing, visible real-model
streaming, and image/text comprehension through the paired connection. Attachments restored
from disk remained usable by the model. Native JSON export preserved all messages and the
exact attachment bytes. Process restart and relay restart retained the saved connection and
conversation, with no additional model completion request.

A Qwen request exhausted its configured context without an answer, exposing a false-success
placeholder. Empty and whitespace-only completions now become recoverable errors while keeping
the connection available. All 70 frontend tests pass, Svelte has no diagnostics, and both
acceptance apps plus the ordinary unsigned app rebuilt. A deterministic empty-response mode
verified the corrected native error display and a successful explicit retry.

Detailed evidence and limits are in
[NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md#real-model-attachments-and-export-results--september-15).
An older pairing stalled while macOS SecurityAgent was running; the system prompt was not
observed. Keychain permission/denial across signed upgrades remains a release gate. Next:
that recovery check, physical two-Mac sleep/wake/network acceptance, active guest/agent lock
checks, and signing/updater/default-relay/beta validation. Public release remains gated.


### Keychain read recovery — September 15–16

Keychain reads now have a 30-second caller deadline and retain an exclusive read permit until
the OS operation actually finishes, including after timeout/cancellation. This prevents stuck
connection checks and duplicate simultaneous read prompts. Failed credential access produces
specific recovery guidance, preserves saved pairings, and never replays chat automatically.
App-lock initialization uses the same bounded read and remains locked on failure. Setup focuses
the recovery message after a failed connection and shows the error when automatic startup
connection fails. Startup guidance appears above the saved computers.

Native tests inspected the actual system prompt, observed the form recover while the prompt
remained unanswered, and exercised Deny on a fresh prompt. Existing pairing IDs/states and
model request counts were unchanged. The older pairing also reconnected on explicit retry.
See [NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md#keychain-read-recovery--september-1516).
Verification: 73 frontend tests, zero Svelte diagnostics, 21 app tests passed (one opt-in test
ignored), and clean app Clippy. Write/removal permission failures, login-keychain transitions,
signed upgrades, physical two-Mac networks/sleep, guest/agent lock, and distribution gates remain.

### Lifecycle recovery and active-work lock — September 22–23

Completed the next native acceptance slice: pairing approval/receipt/removal storage failures,
lock refusal while a conversation is unsaved, guest-stream interruption, and cancellation of
pending and running agent tools. Fixed durable client removal intent, stale host revocation
status, hidden receipt warnings, stale approval offers after removal, and Settings error/focus
recovery. Guest-link creation/removal now retains keyboard focus in the dialog.

Verification: 86 frontend tests, zero Svelte diagnostics, 24 app tests passed (one opt-in test
ignored), clean app Clippy, and rebuilt unsigned bundles. See
[NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md#pairing-storage-boundaries-and-active-work-locking--september-2223)
for native evidence, exact fault boundaries, cleanup, and limitations.

Next release gates: native Keychain mutation-denial/login-keychain transitions and signed
upgrades; migration recovery; physical two-Mac networking/sleep; remaining keyboard and advertised
feature acceptance; default relay operations; signing/notarization/updater installation; private
beta. The lifecycle pass does not approve public release.

### Native credential mutation recovery — September 23–24

The packaged acceptance client passed a canceled native Keychain write prompt, OS-rejected
replacement/removal against a write-protected private keychain, preservation of old bytes,
and exact replacement/removal on explicit retry. Fixed removal errors that incorrectly said
“save.” The test-only query adapter forwarded real Security-framework calls and was not bundled
with the app. No login-keychain permissions or production credentials changed.

Verification: 86 frontend tests, 24 app tests (one opt-in ignored), clean Svelte/Clippy checks,
rebuilt unsigned apps, and baseline/cleanup checks. See
[NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md#native-keychain-mutation-recovery--september-2324).

Model-credential Settings mutation recovery is now covered. A removal-specific permission
prompt, native app-lock/pairing credential mutation boundaries, login-keychain transitions,
and signed-upgrade behavior remain distinct checks. Continue those alongside the previously
listed physical-device, operated-relay, distribution, accessibility, and beta gates.
