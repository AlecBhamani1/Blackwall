# Persistent computer pairing — implementation contract

Updated: 2026-09-21. Status: **implemented in this checkout; release acceptance remains open**.
The versioned exchange, native Keychain/SQLite orchestration, saved-computer UI, stable reconnect,
and durable revocation are implemented. The remaining native/device tests below are release gates;
this document does not claim a published capability or an operated default relay.

September 17 credential acceptance preserved all existing host/client pairing IDs and states.
Save/removal recovery UI and focus were improved; 77 frontend tests pass. Packaged native
write/removal denial, including pairing persistence under those failures, remains unverified.
The detailed observations and distinction between native probes and component fault tests are
in `NATIVE_ACCEPTANCE.md`, “Keychain writes and removal — September 17.”

September 21 storage acceptance preserved existing paired-device IDs/states during chat recovery
from write protection, a full disposable volume, and read-only storage. This does not exercise
pairing approval/revocation writes under those conditions; that native gate remains open.
See `NATIVE_ACCEPTANCE.md`, “Storage failure recovery — September 21.”

## What a person should experience

On the computer running the model, choose **Connect another computer**, review the model being
shared, and create a pairing invitation. On the second computer, choose **Connect a computer** and
paste that invitation. The host shows the requesting computer's name and a matching confirmation
code. The person checks both screens and approves on the host. Both computers then show a
friendly saved name. Later launches reconnect without copying addresses, keys, or invitations.

The host must remain open, unlocked, and awake. Both computers show that requirement before
pairing. Pairing permits model chat only; it does not grant host filesystem, shell, memory, skills,
or conversation access. Guest links keep their existing temporary browser-chat behavior.

A full invitation is the initial implementation. It contains the relay origin, so self-hosted
setups work without a global directory. A bare short-code flow is not advertised until the operated
default relay and its abuse controls exist. A three-group hexadecimal confirmation code is a verification
aid, never the authentication secret.

## Trust and identifiers

| Item | Creation and storage | Exposure and lifetime |
| --- | --- | --- |
| Host identity | Native host generates an independent 256-bit secret; Keychain stores it | Secret never enters webview state; relay retains an ownership digest |
| Pairing invitation | Host generates an independent 256-bit secret, plus random public exchange ID | Secret appears only in creation UI and invitation fragment; expires after five minutes |
| Candidate device | Native client generates its own independent credential | Client Keychain stores raw credential; host/relay receive only the required digest |
| Paired address | Host generates a fresh random address for each approved device | Stable across relaunch; not reused for a different pairing |
| Display name | Explicit person-editable label, at most 120 UTF-8 bytes, no controls | Plain metadata, never proof of identity |
| Confirmation code | Binds the particular exchange and candidate | Both screens must show the same code before host approval |

HTTPS/WSS protects transport. The relay remains inside the trust boundary and can observe forwarded
traffic. This release does not claim application-level end-to-end encryption. A compromised relay
cannot be made trustworthy by a confirmation code; that code prevents accidental approval of
the wrong pending device within the trusted deployment.

## Protocol states and failure behavior

Use a versioned extension with an explicit capability response. An older relay must return a clear
"Pairing requires a relay update" result, without silently falling back to a reusable guest link.

1. **Invite created.** Require an unlocked host, a successfully discovered model, available paired
   device capacity, and an acknowledged relay registration before showing the invitation. Keep the
   short-lived raw invitation only in memory. Cancel and expiry erase the pending exchange.
2. **Candidate presented.** Validate the invitation locally before making any request. Send its
   secret in an authorization header, never a query string or log. Atomically bind the exchange to
   one candidate credential and name. A second candidate cannot replace the first. Bound failures
   per exchange (five attempts), global pending exchanges, request sizes, and deployment-wide request rate (1,200 per minute).
3. **Host review.** Show device name, matching code, chosen model, and the exact access granted.
   The pairing remains unusable until the host approves that exact exchange and candidate. Denial,
   cancellation and expiry reject approval. Locking cancels reachable pending exchanges and stops
   tunnels immediately; network failures retain bounded retry state without granting model access.
4. **Durable approval.** Persist host device metadata and its Keychain material before acknowledging
   approval. Use a recoverable pending/committed record so a crash between Keychain and SQLite
   writes cannot grant access without a visible, revocable device. Never return a successful pairing
   if either durable write failed.
5. **Client receipt.** Persist the device credential and host label before adding a usable saved
   connection. Permit bounded idempotent retrieval by the same candidate after a lost response;
   a different candidate or replayed invitation must not create another paired device. Clear the
   invitation from the UI as soon as it has been handed to native code.
6. **Connected.** Start the outbound host tunnel with the stable per-device address. Use a renewable
   bounded lease, not a decades-long expiration. Renewal requires the retained host credential.
   Advertise only the selected model and retain existing request/body/concurrency limits.
7. **Offline.** Keep the device metadata and key. Retry connection with bounded backoff and jitter;
   show **Computer offline** / **Reconnecting**. Never replay an interrupted or completed prompt.
   Failed turns remain visible and require an explicit retry from the person.
8. **Revoked.** Persist revocation before disconnecting and acknowledging it. Stop in-flight work
   for that device. Revocation must survive host/relay restart, delayed renewal, and stale reconnect
   attempts. Require a fresh exchange and fresh credentials to pair again.

Keep pending exchanges ephemeral: a relay restart cancels unfinished pairing and asks the person
to create a new invitation. Completed pairings survive restart. This avoids reconstructing partly
approved exchanges from ambiguous state.

## Native data and lifecycle

Use typed records outside arbitrary preferences. Store schema version, device ID, display name,
relay origin, allowed model, credential digests, and status in SQLite schema version 2. Exchange
expiry is ephemeral; stable records do not contain raw credentials. Store raw host/client secrets in
separate Keychain services and use **origin plus paired address** for account scope. Existing
origin-only model-key accounts cannot safely distinguish two paired computers on one relay.

- Native commands are all app-lock gated and recheck the gate after user interaction and network
  waits. Serialize pairing/revocation against lock and application shutdown.
- Initialize saved host tunnels only after unlocking; stop them on locking. Relaunch must not
  create a new device identity or require a new invitation.
- Do not silently delete a saved device because its host is temporarily unreachable.
- Removing a client-side connection removes its local credential and explains that host-side
  revocation is separate. Host-side **Remove device** revokes its access independently.
- Update a device's allowed model only through a reviewed host action; do not substitute a model
  silently when the previous one disappears.
- Use one combined host request budget for persistent devices and guest links. Separate managers
  must not accidentally multiply GPU capacity beyond the documented limit.
- Never load raw Keychain values into Svelte stores, SQLite, analytics, logs, exported chats, or
  status/list responses. User-entered invitation drafts necessarily exist briefly in the input UI.

## UI acceptance

- Local setup stays the simplest default. **Connect a computer** provides pairing first, with direct
  URL entry under advanced setup.
- Host pairing and temporary guest sharing are separate labeled actions with distinct explanations.
- Show a saved-computer card containing name, selected model, availability, and a primary connect
  or retry action. Expose removal without making the person find an access-key account.
- Keep a working connection intact until the paired replacement has passed model discovery.
- Provide specific expired, denied, already-used, revoked, offline, outdated-relay, and secure-save
  failures. Do not display raw upstream bodies or sensitive URLs in errors.
- Verify keyboard focus throughout creation, paste, host review, denial, success, and removal.
  Disable duplicate submissions, announce pending/result states, and restore focus after dialogs.
- Inspect 390px and desktop layouts with long device/model names, errors, loading, and empty states.

## Required tests before marking pairing complete

| Boundary | Required evidence |
| --- | --- |
| Exchange | Five-minute expiry; bounded guesses; malformed/oversized input; atomic first candidate; duplicate requests; rejection and cancellation |
| Consent | No model access before approval; exact candidate binding; lock during approval; stale approval cannot revive a cancelled exchange |
| Persistence | Host and client relaunch; failures at each Keychain/SQLite step; crash recovery; no raw secrets in metadata or export |
| Credentials | Two paired computers on one relay cannot use each other's credentials; upstream model keys never leave host |
| Recovery | Host restart, relay process restart, sleep/wake, separate network change; same saved name/address; no automatic prompt replay |
| Revocation | Active stream stops; other devices remain usable; restart and stale renewals do not restore access; new pairing uses new credentials |
| Compatibility | Old relay rejected clearly; ordinary direct connections and temporary guest links remain functional |
| Capacity | Combined guest/device limits; pending pairing limits; rate limiting before expensive work |
| Product | Fresh two-computer setup without URL administration; keyboard-only and narrow-layout review; genuine remote-network acceptance |

## Execution checkpoints

- [x] Preserve address ownership across host disconnect and actual relay process restart.
- [x] Reconnect the existing tunnel without replaying completed prompts.
- [x] Enforce invitation expiry on already-running requests, not only on new API calls.
- [x] Implement and test the versioned exchange and durable revocation contract.
- [x] Implement native Keychain/SQLite lifecycle, incomplete-approval recovery, and lock cancellation.
- [x] Test SQLite device restoration, distinct credential scopes, rejection recovery policy, and blocked pending/revoked tunnels.
- [x] Exercise actual macOS Keychain round trips, role/device isolation, invalid replacement, and independent removal with disposable identities.
- [x] Inject key-save, pending-save, relay acknowledgement, and active-save failures through the production lifecycle; verify retries keep one identity.
- [x] Test locking during approval, abandoned callers, real SQLite open failures, and database-worker authorization during cancellation.
- [x] Reject stale writes that would restore a removed device or downgrade a committed activation, including writes from another database connection.
- [ ] Exercise actual Keychain permission prompts, disk-full/read-only conditions, and both packaged Macs relaunching.
- [x] Build and review pairing and saved-computer UI at desktop and 390px widths.
- [x] Test confirmation reset, keyboard checkbox activation, explicit removal, and failed connection retry.
- [ ] Complete the full keyboard-only native creation/approval/removal path.
- [ ] Complete two-device acceptance with an operated relay.

Do not check the persistent-pairing milestone on the strength of the foundation tests alone.


For an opt-in local Keychain smoke check (creates and removes disposable device accounts):

```sh
cargo test --manifest-path src/Cargo.toml -p blackwall-app \
  native_pair_credentials_round_trip_and_removal_are_isolated -- --ignored
```

This test passed on the development Mac. It is ignored by default so ordinary test runs do not
interact with a developer's login Keychain. Production container acceptance is available through
`node deploy/relay/smoke.mjs <image>` and checks durable pairing revocation across container replacement.


## Native lifecycle failure review — 2026-09-10

The native commands now use the same approval/receipt lifecycle exercised by deterministic failure
tests. Keychain and relay failures are injected at the adapter boundary; successful metadata writes
use real SQLite files reopened between assertions. Tests cover failed host and client saves, lost
relay acknowledgements, retrying a single address, and abandoned pre-commit operations. Pending
records remain ineligible to start host tunnels.

Device identity is allocated before the first durable attempt and retained for that pending exchange.
A prepared identity is not evidence that the relay approved it: lock/replacement attempts cancellation
regardless, and the host's Done action checks actual relay status. This preserves cancellation after
an incomplete save.

Activation and locking are serialized. If locking begins first, activation fails and the host record
stays pending. If activation has already acquired authorization, its SQLite worker finishes before
locking begins. The worker owns the authorization guard, so cancellation of its IPC caller cannot
release the guard while the database write continues. A test abandons that caller during a real
blocking SQLite operation and verifies both the committed record and subsequent locked state.

Unlocking and a duplicate lock request are rejected while connection cleanup is unfinished. Cleanup
runs independently of its caller and stops paired and guest sharing concurrently. A separate test
holds the pairing mutex and proves an existing tunnel still receives its stop signal promptly.

These are automated native-code checks, not two-device packaged-app acceptance. Real Keychain
permission prompts, physically full/read-only storage, sleep/wake, and separate-network operation
remain open release gates.

The database also rechecks device state inside the write transaction. Stale writes cannot change
`revoking`/`revoked` back to usable access or downgrade `active` to `pending`. This protects removal
when another app process has already read an older record; a two-connection regression test verifies
that the committed state wins.


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

### Pairing lifecycle fault acceptance — September 22–23

Native host approval and client receipt recover from scoped database failures using the same
pairing identity. Client removal now saves its removal intent before deleting the key; partial
cleanup stays retryable and cannot be selected as an active connection. Host reconciliation
reports pending revocation even when the relay has revoked access but the final database save
fails. Receipt polling surfaces errors without losing the pending request, and removal clears
its matching approval offer.

Native app-lock checks also covered unsaved chat, an active guest stream, and pending/running
agent tools. All 86 frontend and 24 app tests pass (one opt-in native test ignored), with clean
Svelte/Clippy checks. Detailed evidence and limitations are in
[NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md#pairing-storage-boundaries-and-active-work-locking--september-2223).
Native Keychain mutation-denial, signed upgrades, two physical Macs across network/sleep changes,
default relay operations, distribution, and beta acceptance remain release gates.

### Model-credential native mutation recovery — September 23–24

Native Settings now has evidence for a canceled write prompt and real OS replacement/removal
failures, preserved old credentials, and successful explicit retries. Removal error wording
was corrected. This used a disposable private-keychain adapter and did not change production
credential access. See
[NATIVE_ACCEPTANCE.md](NATIVE_ACCEPTANCE.md#native-keychain-mutation-recovery--september-2324).
Pairing/app-lock credential mutation boundaries and signed/login-keychain transitions remain
separate acceptance checks; these model-credential results do not close them.
