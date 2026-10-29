# Packaged native acceptance

Updated: 2026-09-21. Public release status: **acceptance still in progress**.

## Scope and isolation

These checks use two unsigned, clearly named macOS app bundles on one development Mac:
`Blackwall Acceptance Host.app` and `Blackwall Acceptance Client.app`. Both run the real
Tauri UI, native commands, SQLite persistence, and macOS Keychain integration. The relay is
the actual local relay executable. Early checks and repeatable failure cases use a deterministic
loopback model fixture. September 15 also exercised real Qwen and Gemma models through an
isolated Ollama server; the results below distinguish those runs from fixture responses.

Bundle identifiers isolate the two roles from each other and from the installed app:

| Storage | Production | Acceptance |
| --- | --- | --- |
| SQLite and skills | Existing `~/.blackwall` paths | `~/Library/Application Support/com.blackwall.acceptance.host` or `.client` |
| Keychain services | Existing `com.blackwall.*` names | `com.blackwall.acceptance.host.*` or `.client.*` |
| WebKit storage | Existing default data store | Separate fixed persistent data stores for host/client |
| Updater | Existing release endpoint | No endpoints or updater artifacts |

Explicit persistent WebKit stores require macOS 14 or newer. The build script checks this.
The runtime config applies those stores only to the two acceptance identifiers. An upstream
Tauri config-code-generation issue required setting the store identifier through the Rust API.
Production retains its identifier, storage paths, and credential service names.

The acceptance updater reports that no endpoints are configured; this is expected for these
isolated bundles. Never distribute them as production releases.

## Observed results — September 10–11

| Check | Result and evidence |
| --- | --- |
| Native prompt input and streaming | Passed. Verified text input and the fixture reply in the packaged host UI. |
| Independent histories | Passed. Client began empty while the host retained its prior chat after relaunch. |
| Invitation creation | Passed. Correct five-minute display; name and Copy invitation focus; relay details remain open during editing. |
| Approval | Passed. Both native screens showed identical confirmation codes; host approval created a saved client connection. |
| Paired chat | Passed. Client discovered the advertised model and received a streamed fixture reply through its saved pairing. |
| Relaunch | Passed. Both app processes exited and relaunched. SQLite retained the same device ID, role, name, and active state. Client sent another chat without a new invitation. |
| No automatic prompt replay | Passed for this run. A request made while the host was closed failed; the fixture count stayed unchanged until a new explicit request after relaunch. |
| Host removal | Passed. Explicit removal persisted the host record as revoked and remained revoked after relaunch. Client reconnect failed. |
| Fresh pairing after removal | Passed. Local client removal cleared the old card; a fresh exchange saved a working connection. |
| Focus after approval | Passed in rebuilt native UI: client review panel → saved connection; host approval → Done. Removal returns focus to the panel heading. |
| Text attachment | Passed native picker selection and send. SQLite retained the generated file name and exact text contents. |

The original temporary fixture directory was cleared between sessions. Its relay-persistence
result is not inferred from the later replacement fixture. Earlier independent relay process
and container restart checks are recorded in `DELIVERY_PLAN.md` and `PAIRING_PLAN.md`.

## September 14 follow-up and verification limits (initially blocked)

The acceptance client SQLite file retained the attachment's exact text across the interrupted
session. Host/client metadata still showed the replacement pairing active and the original host
record revoked. Both isolated bundles and the ordinary unsigned `Blackwall.app` rebuilt from the final source.
Their bundle identifiers were verified. Fixture resume reused the saved loopback ports, duplicate
startup was rejected, and the test app/model/relay processes were stopped afterward.

The final visual recheck could not run: Orca 1.4.201 returned `permission_denied` with
“computer-use agent peer is not authorized” for both app inspection and capability discovery,
while its permission status reported Accessibility and screenshots granted. No workaround was
used to bypass this restriction. The latest HTTP-404/offline recovery copy and attachment display
after relaunch were initially left open; the resumed verification below closes those two checks. The earlier native results above
remain valid for their recorded stage; they are not claimed as a final complete acceptance pass.

Automated verification: 65 frontend tests pass with zero Svelte errors/warnings; 72 standard Rust
tests passed across the workspace and the final app follow-up. The opt-in Keychain test is excluded
from ordinary runs. Clippy passes for all targets/features with warnings denied. Tests specifically
cover serialized IPC rejection before its error event, safe recovery copy, offline state without
prompt replay, approval focus, and selecting the newly saved computer when another already exists.

## Resumed packaged verification — September 14

After desktop access was reconnected, capability discovery and native UI inspection succeeded.
The final rebuilt acceptance client opened its saved pairing without another invitation.

- Opening the saved conversation displayed `acceptance-note.txt`, its 48-byte size, the original
  message, and the model reply. The attachment card was also checked visually in a screenshot.
- Closing the host and explicitly sending a request changed the client to **Model offline**,
  exposed **Reconnect**, and displayed the new guidance about an open, unlocked, awake host
  and pairing again if access was removed. The failed turn remained in the conversation.
- Relaunching the host and choosing the named saved connection restored **Model connected**.
  Returning to the conversation preserved the attachment and failed turn. The fixture had
  received zero requests, confirming reconnect did not replay the failed prompt.
- A new explicit message received a streamed reply through the saved pairing. The fixture
  recorded exactly one request, matching that explicit action.

The desktop authorization blocker is resolved. These checks close the final attachment-display
and offline/reconnect-copy recheck from the previous stage. They still use two app instances on
one Mac and a deterministic model. Next is native app-lock/passphrase acceptance, followed by
real-model and two-physical-Mac/network acceptance. Local screenshots and request-count evidence
are retained under `src/target/native-acceptance-20260914/` (ignored build/test artifacts).

## Findings fixed during review

- The relay settings closed on the first character because their open state depended on the
  edited URL. Their open state is now independent and retains keyboard focus.
- Invitation creation could briefly display six minutes because its clock tick preceded the
  new expiry. The displayed countdown is bounded by the actual five-minute lifetime.
- Removed form controls dropped focus. Creation, review, approval, and saved-connection
  transitions now move focus to their next useful control without taking focus from another panel.
  Saved connection buttons include the computer name in their accessible label.
- Host approval displayed its success message twice. It now has one result message.
- Native IPC failures arrived as plain objects and lost their recovery message. Errors now
  normalize whether the native rejection or event arrives first. Public copy excludes raw
  endpoint URLs and upstream bodies.
- Lost, timed-out, refused, and unavailable model connections no longer remain labeled ready
  after a failed turn. Recovery preserves the failed turn and never resends it automatically.
  Busy/model-level failures do not automatically classify a reachable service as offline.

## Reproduce locally

Run from the repository root, with the UI dependencies and Rust toolchain installed:

```sh
cargo build --manifest-path src/Cargo.toml -p blackwall-relay
python3 scripts/build-acceptance.py
python3 scripts/serve-native-acceptance.py --directory src/target/native-acceptance
```

Keep the fixture terminal open. In another terminal, run:

```sh
python3 scripts/launch-acceptance.py host --directory src/target/native-acceptance
python3 scripts/launch-acceptance.py client --directory src/target/native-acceptance
```

Use the relay address recorded in the fixture’s `state.json` in the host’s advanced pairing
settings. The client launcher deliberately supplies an unavailable direct model address, so
successful chat must use the saved pairing. The launcher clears inherited model/relay keys.
Both acceptance apps retain their state across launches and rebuilds; use their own removal
controls to clear a test device. A new fixture folder alone does not reset app storage.

The fixture records only model name, message count, and streaming mode. Do not save invitation
secrets or Keychain values in reports. Close the acceptance windows and use Ctrl+C in the fixture
terminal to stop its own model server and relay. To reuse the same ports and relay data after
stopping the fixture, restart it with the same directory and `--resume`. An existing listener
causes startup to fail; the script does not kill unrelated processes. Optional `--model-port`
and `--relay-port` select fixed loopback ports for a new fixture.

Rebuild the ordinary unsigned app after acceptance builds if reviewing the production bundle:

```sh
cd src/app
PATH="$HOME/.cargo/bin:$PATH" ../../ui/node_modules/.bin/tauri build --bundles app --no-sign --ci \
  --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

## Remaining release gates

- Two physical Macs on separate networks, sleep/wake, and network changes.
- Broader model/provider and attachment-format coverage. Same-Mac Gemma streaming, PNG/text
  attachments, persistence, and native JSON export passed on September 15 (details below).
- Full keyboard-only native flow across the remaining application screens.
- Native lock acceptance with guest-browser sharing and local agent/tool work active.
- Keychain write/removal permission failures, login-keychain transitions, and signed upgrades.
  Native read timeout/denial recovery passed September 16. Basic chat recovery from full/read-only
  storage passed September 21; native storage failures during pairing approval and locking remain open.
- Operated default relay and onboarding that requires no address administration.
- Apple signing/notarization, clean installation, upgrade, and updater acceptance.
- Private beta feedback and closure of material issues before public publication.

Same-Mac acceptance does not close these remaining gates. Nothing has been installed over the
production app, deployed, or published by this work.


## Native app-lock test preparation — September 14

The isolated host/client apps and retained relay fixture were started for the next gate.
A new `--stream-delay` fixture option sends the first delta immediately and delays completion
(up to 60 seconds), allowing an actual in-progress response to be interrupted by locking.
A direct fixture check verified the early delta and delayed `[DONE]` independently.

Execution sequence:

1. Enable a disposable passphrase on the acceptance host; check mismatch validation and secure-field clearing.
2. Start paired streaming, lock the host, and verify the stream stops and subsequent access fails.
3. Reject an incorrect passphrase; unlock correctly and reconnect without replay.
4. Relaunch the host; verify it starts locked and does not open paired tunnels before unlocking.
5. Start an unfinished pairing, lock the host, and verify that exchange cannot gain access.
6. Change and remove the test passphrase through the native UI; verify normal relaunch and clean up.

At preparation time, Orca could inspect the native Settings tree but could not open the App lock
disclosure: `window_not_focused`, with “no focused window.” The supported restore attempt and
LaunchServices activation did not fix it; OS permissions remained granted. Manual focus was
requested. No passphrase was set, and none of the native lock assertions is marked passed yet.

## Native app-lock results — September 14

The preparation blocker above was resolved with native keyboard navigation (Tab, Shift+Tab,
Return). Tests used the isolated acceptance host/client, their own SQLite and Keychain namespaces,
and the retained loopback relay with `--stream-delay 30`. The production app and its data were
not used. Secure text was supplied through stdin; disposable passphrases were never printed.

| Check | Observed result |
| --- | --- |
| Enable lock | Mismatched confirmation was rejected. Matching values enabled the lock and displayed the saved result. |
| Interrupt paired streaming | The client displayed an initial response and Stop response. Lock now on the host stopped the stream before the delayed completion, retained partial text, and displayed Model offline with unlock/reconnect guidance. |
| Locked access | Saved-connection attempts failed while the host was locked. No additional model requests were recorded. |
| Wrong and correct unlock | Incorrect passphrases were rejected. The correct passphrase restored the host and its existing pairing. |
| Reconnect without replay | Reconnect retained the interrupted turn without sending it again. Only a new explicit request advanced the fixture count, from two to three, and completed successfully. The first recorded request predates this lock test. |
| Locked startup | After closing and relaunching the rebuilt host, it displayed the passphrase screen; the client could not connect until unlock. |
| Pending approval cancellation | Both windows showed the same confirmation code. Without approval, locking the host caused the client to report the request declined. Read-only SQLite inspection showed the same two host records (one revoked, one active) and one active client record, with no new device. Unlocking did not revive the pending approval. |
| Change passphrase | The old passphrase stopped unlocking the host. The replacement worked immediately and after another complete process restart. |
| Remove lock | The old passphrase could not remove the lock. The replacement removed it through Settings. Another process restart opened the normal chat screen without a passphrase, and the existing client reconnected. |
| Keyboard recovery | Rebuilt native UI focused Passphrase on locked startup and after a rejected unlock. Return submitted successfully without a mouse click. |

Review found that a cached model catalog was incorrectly treated as a live connection after a
failed reconnect. The controller now retains ready status only when an already working,
different connection remains available during an unsuccessful attempt to switch servers. A
failed check of the current server stays offline. Regression tests cover both cases, and a
native retry against the locked host verified the corrected sidebar with a cached catalog.

The unlock screen now restores focus on arrival and after failed submissions. App lock copy also
explicitly explains that paired computers are disconnected. The screenshots were inspected for
the lock screen and Settings layouts. A final review also fixed focus dropping outside Settings
after app-lock submissions. A rebuilt native host confirmed focus moves to Current passphrase
after enabling, returns there after rejected removal, and moves to Passphrase after removal.

The first rebuilt host launch briefly remained on Checking your app lock while a macOS
SecurityAgent process was running. Orca could not inspect that system process, and startup later
continued to the locked screen without an automated system-dialog action. The exact prompt and
its resolution were not observed, so Keychain permission/denial acceptance remains open.

Verification: all 68 frontend tests passed; Svelte reported zero errors and warnings. Both
isolated native bundles and the ordinary unsigned `Blackwall.app` rebuilt successfully. Diff
whitespace checks passed, and the repository knowledge graph was refreshed. Rust source did not
change in this step; earlier Rust/Clippy results remain recorded above.

This closes the planned same-Mac passphrase and persistent-pairing lock sequence. It does not
establish physical-device, separate-network, real-model, guest-browser, agent/tool, or signed
distribution acceptance. Next: real-model streaming, image/attachment and export checks, then
two-Mac sleep/wake and network transitions.

Disposable test passphrases and the cancelled invitation file were deleted after native removal.
Screenshots and a sanitized evidence manifest remain in the ignored acceptance fixture folder.
The acceptance apps and the fixture processes were closed after testing.


## Real-model, attachments, and export results — September 15

Tests used the separate unsigned acceptance apps on the same 16 GB Apple Silicon Mac,
the retained loopback relay, and Ollama 0.33.2 with an isolated model directory. The
installed production app, production Keychain entries, and existing user model store
were not used. Ollama ran one request at a time with a 4,096-token context.

The test models were [Qwen 3.5 0.8B](https://ollama.com/library/qwen3.5/tags) and
[Gemma 3 4B](https://ollama.com/library/gemma3:4b). Both were discovered through the
native local-model setup. Gemma was selected in the native model menu before creating
its pairing; an existing pairing continues to share the model selected when it was created.

| Check | Observed result |
| --- | --- |
| Native local discovery | Use this computer found Ollama and its models without entering an address. Use these models connected successfully. |
| Direct real-model chat | Qwen completed a short three-sentence request through the native host. |
| Real-model pairing | Separate Qwen and Gemma invitations were joined and approved through the native windows after comparing matching codes. Saved Gemma access connected successfully. |
| Native attachments | The client selected a 131-byte text file and a 7,344-byte PNG through macOS Open dialogs. The image thumbnail rendered. Both payloads matched the original files in SQLite. |
| Real-model streaming and attachment comprehension | After reopening the client, Gemma received the attachments from the saved conversation through the pairing. A partial answer appeared while Stop response remained available. The completed answer correctly identified project ORBIT-731, Cedar room, the red square on the left, and the blue circle on the right. |
| Native export cancellation | Cancelling the Save dialog returned to the conversation with focus on Export chat. |
| Native export save | Save wrote JSON through the native dialog. All four messages, including the final Gemma answer, exactly matched SQLite. Decoded image bytes and text bytes exactly matched the original files. The export captures a fresh session timestamp, so `updatedAt` was not asserted equal. |
| Restart and reconnect | The client process exited and relaunched. Its saved Gemma connection reconnected after a fixture relay restart; loading history restored both attachments and the completed answer. Ollama recorded three total completion requests before and after these operations, with no automatic replay. |
| Empty-answer recovery | A rebuilt native host received an intentionally empty SSE completion from the deterministic fixture. It displayed recovery guidance, stored the assistant turn with `status: error`, and kept Model connected. Restoring normal fixture output did not replay the failed turn; a new explicit request succeeded. The fixture count advanced from three to four for the empty request, then to five for the explicit retry. |

The first combined attachment request with Qwen exhausted the configured context without
answer text (698 prompt tokens plus 3,398 generated tokens; runner reported truncation).
That attempt is **not** counted as successful image acceptance. It exposed an app defect:
a completed stream with no text was being stored as a successful placeholder answer. The
controller now treats empty or whitespace-only completion as an error and displays:
“The model returned no answer. Try a shorter prompt or choose another model.” The original
historical placeholder remains in its saved conversation; existing history is not rewritten.
Gemma subsequently answered the same saved attachments correctly.

Regression tests cover empty and whitespace-only completions, retained connection state,
no automatic replay, and a successful explicit next request. The native fixture now accepts
`--empty-response` for repeatable error-path acceptance; omit it when resuming to restore
normal answers. All 70 frontend tests passed, Svelte reported zero errors and warnings,
and both acceptance bundles plus the ordinary unsigned macOS bundle rebuilt successfully.
The fixture passed Python compilation and native execution. Diff whitespace checks passed,
and the knowledge graph was refreshed. No Rust source changed during this step.

An additional attempt to reconnect the older deterministic pairing remained on Checking
connection while a macOS SecurityAgent process was running. The system prompt and its
resolution were not observed. The client was closed, and empty-answer acceptance was
completed through the direct native host instead. This does not establish Keychain
permission acceptance or explain the stall conclusively. Test an older saved key across
signed app updates, cancellation/denial of its system prompt, and bounded recovery before
release. Newly created Gemma pairing access passed the restart checks described above.

Evidence, synthetic files, native exports, model metadata, and inspected screenshots are
retained in the ignored `src/target/native-acceptance-20260915/` directory. The model
folders are retained for reproducibility; the consumed invitation file was removed.
The acceptance apps, fixture relay/model, and isolated Ollama server were stopped after testing.

### Next acceptance sequence

1. Exercise Keychain writes/removals and credential access across a signed build/update.
   Native read timeout and denial recovery are completed below; login-keychain transitions
   and signed upgrade identity remain open.
2. Run the existing pairing matrix on two physical Macs and separate networks: host sleep
   and wake, network changes, offline reconnect, locked startup, revocation, and no replay.
3. Complete active guest-browser and agent/tool lock checks, keyboard flows outside the
   completed lock screens, and actual storage-failure acceptance.
4. Validate the operated default relay, signed/notarized installation, updater signature
   checks and upgrade persistence, then the planned beta/release checklist.

These results close the same-Mac real-model streaming, image/text attachment, persistence,
export, and native empty-answer checks. They do not approve a public release.


## Keychain read recovery — September 15–17

Investigation found that credential lookup happened before the network request's timeout.
The Security framework call ran on a blocking worker, but its caller could wait indefinitely
for permission. Apple's [SecItemCopyMatching documentation](https://developer.apple.com/documentation/security/secitemcopymatching(_:_:))
confirms that the underlying operation blocks its calling thread.

A shared native read wrapper now limits each caller to 30 seconds, including time waiting for
another Keychain read. Only one Keychain read runs at a time per app process. Its permit stays
inside the blocking worker when a caller times out or is cancelled, so retries cannot accumulate
concurrent read prompts. A late result is discarded by the abandoned caller. This wrapper covers
model keys, relay keys, pairing keys, credential-presence checks, and app-lock initialization.
A failed app-lock read still leaves the app locked and uninitialized; it is never treated as
absence of a passphrase. Keychain writes retain their existing behavior.

Credential failures now have specific, allowlisted recovery guidance in both connection setup
and chat. The current connection becomes offline if its credential cannot be read. A failed
attempt to switch to a different service preserves the previously working connection. Saved
pairings and failed chat turns are retained; reconnect does not automatically replay a request.
Review also found focus falling back to the page after a failed setup attempt. Setup now focuses
the recovery message after controls become available again. A further startup check found
that the initial connection failure was hidden when setup first opened; setup now carries
that recovery message into its initial view, directly below the introduction so saved
computer lists cannot push it below the visible area.

### Native observations

- The actual SecurityAgent prompt was inspected by PID. It identified the acceptance host's
  pairing-key service and requested permission to read it. No login password was entered or read
  by automation. Later explicit client reconnect to the older deterministic pairing succeeded;
  the earlier prompt's resolution was not observed and is not counted as a verified Allow action.
- A disposable model-key entry was created under `com.blackwall.acceptance.client.model` for
  the loopback fixture origin, after verifying no entry already existed. It trusted the system
  `security` utility, requiring the acceptance app to ask for read permission. This did not
  alter production keys or any existing pairing key.
- With that prompt unanswered, the native connection form returned its controls and displayed
  Keychain recovery guidance. At the 48.3-second observation after Connect, the same macOS prompt
  was still present. This establishes recovery despite an outstanding OS read; the configured
  30-second deadline is covered by the implementation and automated timeout tests.
- A fresh request displayed a new prompt. Clicking its observed Deny button was followed by
  prompt closure and immediate recovery guidance before the 30-second deadline. Orca reported
  `window_not_found` during its post-action capture because the prompt had closed; the native
  app's changed state provided the postcondition. No successful connection was reported for
  the rejected candidate.
- Read-only SQLite comparison showed identical host/client pairing IDs and states. The fixture
  completion count stayed at five throughout these checks, proving no chat replay.

Automated verification: 73 frontend tests; zero Svelte diagnostics; 21 app Rust tests passed
with one opt-in Keychain test ignored; Clippy passed for the app and all its targets. Rust tests
exercise timeout, caller cancellation, retaining read exclusivity until OS completion, rejected
reads, and explicit retry. Frontend tests cover safe credential copy, retained pairing/failed
turns, offline state, no replay, and keyboard focus after failure.

The timeout does not dismiss the macOS prompt or cancel the OS read. Until that prompt resolves,
subsequent reads in that app can also time out. Respond to the prompt or quit the app before
retrying; the app does not bypass Keychain permissions. Native write/removal prompt failures,
login-keychain lock/unlock, and credential access across a signed production upgrade remain
separate release gates. This is read-recovery acceptance using isolated unsigned bundles.

The rebuilt native client also refused the existing pairing after Deny and displayed Model
offline. A subsequent manual retry focused the recovery message (visible focus outline);
the form remained usable. The disposable model-key entry was removed after testing.

The September 16 startup check confirmed the rebuilt screen retains Keychain guidance after
Deny. Final visual verification on September 17 confirmed the revised banner placement above
all three saved computers, without scrolling. The relay was stopped for this final launch;
the visible banner reported an unreachable model, so this observation verifies layout and
offline state rather than another Keychain-specific error mapping. Both isolated acceptance
bundles and the ordinary unsigned macOS app were rebuilt with the final recovery changes.
Screenshots and sanitized evidence remain in the ignored fixture folder. All acceptance
windows and fixture processes were closed after testing.

## Keychain writes and removal — September 17

Status: **recovery UI improved; native interactive mutation-denial acceptance remains open**.
This pass used only disposable acceptance credentials and a separate disposable keychain.
It did not lock the login keychain, alter production credentials, or enter a login password.

### Findings and changes

- Setup previously mapped a Keychain save error to generic unreachable-model guidance.
  It now identifies a failed Keychain save, retains the replacement draft, focuses recovery
  guidance, and does not proceed to model discovery until saving succeeds.
- Save/removal operations show pending guidance while awaiting completion. Setup distinguishes
  verifying/saving a key from testing the connection. Buttons remain disabled during mutation;
  no caller timeout claims that an OS mutation was cancelled or that a late write cannot happen.
- Opening removal previously discarded keyboard focus when its button disappeared. The
  confirmation now focuses **Keep key**; cancelling returns focus to **Remove key…**. Success
  and failure messages receive focus after the operation settles.
- Added component tests cover pending replacement/removal, no premature success, denied removal
  preserving saved state, explicit retry, retained replacement drafts, and ignoring results
  from a service that is no longer selected. These use controlled client results; they are not
  native permission-denial observations.

### Native observations and limits

1. The acceptance client's advanced setup saved a disposable replacement at the loopback model
   origin. The subsequently observed permission prompt concerned reading the key. Denying that
   read returned connection recovery guidance. This is not evidence of a denied write.
2. A disposable item with modified access controls still did not produce a mutation-denial
   prompt in the tested app flow. Its removal completed successfully. No denial pass is claimed
   from this attempt, and the item was removed.
3. A small standalone Security-framework probe used `SecItemUpdate` and `SecItemDelete` against
   a private keychain with all queries explicitly scoped to that keychain. With interaction
   disabled in the probe process and that private keychain locked, update returned `-25293`.
   Unlocking the private keychain showed the original bytes unchanged; an explicit update retry
   succeeded and the replacement bytes matched. Deletion while locked returned success (`0`),
   followed by item-not-found. Locking a keychain therefore did not reproduce removal denial in
   this environment. This probe validates OS behavior, not the packaged app's complete error path.
4. The final rebuilt client saved a fresh disposable key through Settings. Native accessibility
   verified focus on **Keep key** and, after cancellation, **Remove key…**. Confirmed removal
   displayed **No saved key** and a visibly focused success message. An independent metadata
   lookup returned item-not-found. No chat or guest-link request was made.

Verification: **77 frontend tests passed; zero Svelte errors/warnings**. Both acceptance bundles
and the ordinary unsigned macOS bundle rebuilt successfully. Rust was unchanged in this pass.
Host/client paired-device IDs and states still match the earlier baseline; the fixture model
completion count remains five. The private keychain and disposable model-key entry were removed;
the acceptance app and fixture processes were stopped. Screenshots, probe source, and sanitized
results are retained under ignored `src/target/native-acceptance-20260917/`.

Remaining gate: reproduce actual pending/denied write and removal operations in the packaged app
under controlled native permission conditions, including app-lock and pairing persistence.
The private-keychain probe and component fault tests do not replace that check. Login-keychain
transitions, signed upgrades, and separate-Mac network/sleep acceptance also remain open.

## Storage failure recovery — September 21

Status: **native chat recovery from write-protected, full, and read-only storage passed**.
This does not close credential mutation-denial or pairing/app-lock failures during storage writes.

### Findings and changes

- A native write failure exposed a recovery bug: **New chat** cleared the visible unsaved
  conversation and disabled **Export chat**, although the failed writes remained queued in memory.
  New chat now waits for persistence and leaves the current conversation and export available
  if saving fails. A delayed result cannot clear newer work or supersede a later navigation.
- Storage errors from native chat were mapped to generic model-service failure guidance.
  The allowlisted `storage_error` message now identifies local data, disk space, and permissions,
  asks the person to keep Blackwall open, and does not incorrectly mark the model offline.
- Save-failure guidance now covers folder permissions as well as disk space. Mobile navigation
  closes the sidebar only after New chat succeeds.

### Native observations

1. With a backed-up acceptance-client database protected using macOS's user-immutable flag,
   a new revision failed to save. The rebuilt app retained the conversation after **New chat**.
   Export through the native save dialog contained all four messages, including the unsaved
   revision, while an independent read confirmed SQLite still held the original two messages
   with an unchanged body hash. Retry while protected continued to report failure.
2. Removing that flag and selecting **Retry saving** persisted messages exactly matching the
   exported messages. New chat then succeeded. Relaunch preserved the recovered conversation.
   Starting with the database protected also showed the storage error; removing protection
   and retrying restored the history in the same app process.
3. A disposable 32 MB HFS+ disk image was mounted at the isolated client's data path with a
   copy of its database. The original acceptance directory was moved aside intact. Filling
   only that volume reached **zero free bytes** and OS **ENOSPC (errno 28)**. The packaged app
   displayed a save failure, retained the turn after **⌘N**, and preserved existing session,
   pairing, and settings records. Removing the filler and retrying saved the retained turn.
4. The image was remounted read-only; an independent write returned **EROFS (errno 30)**.
   App startup and retry showed the storage recovery error without replacing the existing
   database. Remounting writable and retrying restored history in the same app process.
5. The final rebuilt app repeated the full-volume check and showed the corrected local-storage
   guidance. After freeing space, retry persisted the turn. The model completion count remained
   **five** throughout storage recovery. One explicit new message then produced a fixture answer
   and increased the count to **six**. Relaunch displayed both the retained failed turn and the
   successful answer without another request.

### Verification and limits

All **81 frontend tests pass**, with **zero Svelte errors/warnings** and clean diff whitespace.
Four new regression cases cover export after blocked navigation, a pending write that later
fails, newer work superseding a pending New chat, and safe native storage-error normalization.
Both isolated acceptance bundles and the ordinary unsigned macOS app rebuilt. The repository
graph was refreshed. Rust was unchanged in this pass.

Existing pre-test conversation bodies and paired-device IDs/states match their baselines in
both original acceptance profiles; SQLite integrity checks passed. Test-only chat records from
the first write-protection exercise remain in the acceptance client. The image's separate test
records were captured in an ignored database snapshot before cleanup. The original acceptance
directory, permissions, and flags were restored; the volume was detached, its image removed,
and acceptance/fixture processes stopped. No production data, credentials, or installation changed.

Sanitized results, database snapshots, native screenshots, and the disposable fixture source are
under ignored `src/target/native-acceptance-20260921/`. The fixture used a deterministic model,
not a real-model rerun. This pass covers chat save/export/startup recovery; storage failures during
pairing activation/revocation, migration, and app-lock transitions still need native acceptance.
Next: those lifecycle boundaries and active guest/agent lock checks. Native Keychain mutation
denial, signed upgrades, physical two-Mac networks/sleep, and distribution/beta gates remain open.

## Pairing storage boundaries and active-work locking — September 22–23

Status: **native pairing recovery, unsaved-chat lock recovery, guest-stream interruption,
and pending/running agent cancellation passed in the isolated acceptance apps**.
The relay and model were loopback fixtures; these observations do not represent signed
upgrades, physical two-Mac networking, or macOS Keychain mutation-denial acceptance.

### Findings and implementation

- Client removal previously deleted its Keychain key before deleting the database row.
  A forced SQLite deletion failure left an **active** saved connection with no key.
  Removal now durably records `revoking` first. If key or row deletion fails, the client
  shows **Removal incomplete · retry removal**, offers no Connect action, and permits an
  explicit retry. Failure to save the initial intent leaves the active row and key intact.
- A relay revocation followed by a failed database commit previously left stale active
  status in the UI. Reconciliation now retains `revoking` and returns the storage warning
  with the snapshot. The pending message covers automatic retry without assuming the
  failure is a network outage.
- Background client receipt failures were invisible. Polling now preserves pending identity
  and saved devices while exposing the error and **Check again**. A later successful receipt
  clears the warning. Failed requests completing after lock cannot restore old state.
- Removing a just-approved host device left the old **Finish pairing** offer available.
  Committing removal intent now invalidates the matching in-memory offer; unrelated offers
  remain intact. A Rust regression check and a fresh native pairing/removal both confirmed
  that the removed device's approval prompt does not return.
- Refusing to lock an unsaved conversation put its explanation behind Settings. The error
  now appears beside **Lock now** with keyboard focus, and a failed attempt can be retried.
- Creating/stopping guest access removes the initiating button. Focus now moves to the
  generated link or the Create button, or to the error on failure, so keyboard navigation
  stays in Settings. The existing guest lifecycle test now checks both focus transitions;
  the final packaged app also passed creation, stopping, and subsequent Tab navigation.

### Native observations

1. Host approval was exercised with scoped SQLite insertion failures before pending save
   and during activation. Failed initial persistence created no paired records; the partial
   activation retained a pending record and recovered through **Finish pairing** with the
   same device ID. A client receipt failure created no active client row. Removing the fault
   allowed its existing request to finish without a new identity.
2. The rebuilt client displayed the receipt warning and **Check again**. Background polling
   completed the same pairing immediately after writes recovered. With removal-intent writes
   blocked, its row stayed active and Keychain metadata confirmed the key remained present.
   With final row deletion blocked, its row became `revoking`, the key was absent, Connect
   disappeared, and explicit retry removed the remaining row. No secret was read to check
   Keychain presence.
3. Blocking the host's final `revoked` save left a visible pending-removal row and storage
   warning while the relay already returned **404**. Removing the fault let reconciliation
   finish; native local removal then cleaned up its record and key.
4. Blocking a new chat save kept the conversation in memory and refused app locking. The
   final rebuilt app displayed the explanation inside Settings; screenshot inspection confirmed
   the visible focus outline. After restoring writes, **Retry saving**, **Lock now**, and unlock
   succeeded. No automatic model request was caused by save retry or unlock.
5. A guest HTTP stream through a link created in native Settings delivered its first event.
   Native **Lock now** ended the stream after about **10.6 seconds**, before the fixture's
   30-second completion and without `[DONE]`. The old link returned **404** both while locked
   and after unlock. Explicitly creating a fresh guest link restored model discovery (**200**).
   This is guest transport acceptance, not an external-browser UI test.
6. A deterministic tool proposal appeared in the native approval card. Locking while approval
   was pending canceled it without writing a file. A second explicit request was approved
   using **Allow once** in a dedicated disposable project. Its shell and `sleep` processes
   were independently observed running. Locking terminated both; the delayed marker remained
   absent beyond 30 seconds and after unlock. Neither interrupted agent run resumed automatically.

SQLite triggers were restricted to named disposable pairings or chat titles. They exercise
specific commit boundaries; unlike the September 21 storage pass, they do not simulate an
actual full disk. The interrupted overnight receipt left an orphan disposable client key;
that exact account and the expired test pair were removed before the final run.

### Verification and release limits

**86 frontend tests pass**, with **zero Svelte errors/warnings**. **24 app Rust tests pass**;
one opt-in native Keychain test remains ignored. App Clippy passed for all targets/features
with warnings denied. Isolated acceptance apps and the ordinary unsigned macOS app rebuilt.
Automated tests cover real SQLite reopening after partial removal, canceled removal, stale
poll responses, visible retry state, lock-error focus, and invalidation of a removed offer.

All original session, pairing, and settings rows in both profiles match the pre-test baseline;
both databases pass integrity checks. Synthetic lifecycle chats remain only in the acceptance
host for evidence. Temporary faults, disposable pairings/keys, guest links, and test app lock
are removed. Sanitized results and private native snapshots are under ignored
`src/target/native-lifecycle-20260921/`.
Acceptance apps and fixture processes are stopped. The final model count is **11**: five
explicit requests beyond the baseline of six (two storage-lock probes, one guest stream,
and two agent probes). Pairing, recovery, relaunch, and unlock did not replay a completion.

Rebuilding an unsigned acceptance bundle triggered macOS Keychain prompts for existing entries.
The bounded read recovery UI remained available. No login password or Keychain Allow action was
used; the newly created disposable lock entry was reset for the rebuilt-app check, then removed
through Settings. Existing credential entries were preserved. This does **not** close the
signed-upgrade or native credential write/removal denial gates.

Next: native Keychain mutation-denial/login-keychain transitions, migration recovery, broader
keyboard acceptance, physical two-Mac sleep/wake and network changes, operated default relay,
signed/notarized install/update testing, and private beta. Public release remains gated.

## Native Keychain mutation recovery — September 23–24

Status: **canceled native write, OS-rejected replacement/removal, and explicit recovery passed
for model credentials in the packaged acceptance client**. Removal failure was an actual
Security-framework error caused by write protection, not a clicked Deny/Cancel prompt.

### Isolation and method

A disposable private keychain held one synthetic credential for the loopback model origin.
An ignored diagnostic dylib scoped only that exact acceptance-client service/account to the
private keychain by adding `kSecUseKeychain`/`kSecMatchSearchList` to its `SecItem` queries.
The adapter checked the acceptance executable path, forwarded calls to the real macOS Security
framework, and recorded only operation names and returned statuses. It did not manufacture
failures, change Blackwall's error handling, or ship in any app bundle. Other credentials were
not redirected. The keychain search list was restored immediately after private-store creation
and matched its baseline at cleanup. The login keychain was never locked or reconfigured.

This resolves the earlier obstacle where locking a store alone did not prevent deletion.
The tests distinguish operation types rather than treating a failed read as failed mutation;
Apple documents those operations separately in its
[ACL authorization reference](https://developer.apple.com/documentation/security/acl-authorization-keys).
The observed statuses below are evidence from this Mac, not a claim that every macOS version
will expose the same permission prompts or error codes.

### Observed results

| Native action / condition | OS result | Independently verified postcondition |
| --- | --- | --- |
| Replace key while the private store is locked; Cancel its native unlock prompt | `SecItemAdd`: `-128` | Original bytes unchanged; Settings restored retry controls and retained the draft |
| Replace key while only the private keychain file has `uchg` | Duplicate add, then `SecItemUpdate`: `100001` | Old bytes unchanged; Saved in Keychain remained visible; no success reported |
| Remove key with the same file protection | `SecItemDelete`: `100001` | Item and bytes preserved; removal confirmation/retry remained available |
| Restore write access and retry replacement without re-entering the draft | `SecItemUpdate`: `0` | Stored bytes exactly matched the retained test draft |
| Restore write access and retry removal | `SecItemDelete`: `0` | Independent read returned item-not-found (`-25300`); UI showed No saved key |

The actual private-keychain prompt named **Blackwall Acceptance Client**. While it remained
unanswered, Settings displayed its pending guidance and disabled mutation controls. Canceling
returned `-128`; this was a mutation call, not model discovery or credential presence checking.
No login password or Allow action was used.

Initial synthetic text delivery did not produce the intended short test candidate, so those
runs were not used to claim an exact replacement-byte match. The final run used semantic field
entry plus a native keyboard input event. An independent Security-framework reader verified
both the old and replacement bytes before/after failure and retry, without logging key values.

### Finding, review, and verification

Failed removal incorrectly said that the key could not be **saved**. The native credential
error now names **remove** or **save** correctly and asks the person to check Keychain access
and retry. Rebuilt-app screenshots confirmed the removal wording, retained saved state, and
visible error focus. Both failure paths retained their recovery controls.

All **86 frontend tests** and **24 app Rust tests** passed; the opt-in native round-trip test
remained ignored. Svelte reported zero errors/warnings and app Clippy passed with warnings
denied for all targets/features. Both isolated acceptance bundles and the ordinary unsigned
macOS app rebuilt. Diff whitespace checks passed and the code graph was refreshed.

Original sessions, paired devices, and settings rows in both acceptance profiles matched the
baseline; SQLite integrity checks passed. Model completion count stayed **11** throughout:
these tests made only model-discovery requests. The disposable credential/private keychain
and write-protection flag were removed, the keychain search list was unchanged, and acceptance
and fixture processes were stopped. No production data or credential was modified.
Evidence, diagnostic source, operation statuses, and native screenshots are under ignored
`src/target/native-keychain-20260923/`.

Limits: this validates the packaged Settings flow using a private-store query adapter, not an
unmodified signed-distribution environment. A removal-specific permission prompt was not
reproduced. Native app-lock/pairing credential mutation boundaries, login-keychain transitions,
signed upgrades, physical two-Mac networking/sleep, and distribution/beta gates remain open.
