# Release checklist

The checks that must pass before a public (stable) release, collected from the archived
[delivery plan](archive/DELIVERY_PLAN.md) and [native acceptance record](archive/NATIVE_ACCEPTANCE.md).
A box is checked only with evidence: a linked PR, a release note, or a dated record in
`docs/releases/`. Beta builds publish automatically and do not need this list; see
[UPDATES.md](UPDATES.md) and [REPOSITORY_WORKFLOW.md](REPOSITORY_WORKFLOW.md).

## Distribution

- [ ] Apple Developer signing and notarization, checked on a clean Mac.
- [ ] Windows code signing, checked on a clean Windows install.
- [ ] Clean installation of each desktop target (macOS Apple Silicon and Intel, Windows x64,
      Linux x64 and ARM64).
- [ ] Signed updater upgrade from the previous installed release, keeping data and settings.
- [ ] Publish only with available release credentials and the owner's authorized release scope.

## Native app on each platform

- [ ] Credential store (Keychain, Credential Manager, Secret Service): save, read, replace and
      remove, including permission denial and login-keychain changes.
- [ ] App lock with guest sharing and local agent/tool work active.
- [ ] Storage failures (read-only, full volume) during pairing approval and locking.
- [ ] Keyboard-only use of every screen, including the no-model and offline states.
- [ ] Visual review at desktop and narrow widths: setup, detection, download, success, failure,
      chat, settings, approvals and sharing.

## Remote use

- [ ] Two physical computers on separate networks: pairing, host consent, revocation, sleep/wake
      and network changes.
- [ ] Guest access over a cellular connection.
- [ ] An operated default relay, so onboarding needs no address administration (self-hosted stays
      under advanced settings).

## Agent and models

- [ ] End-to-end with a real supported model: remember a fact across relaunch, run a saved skill,
      approve one web request, run a child task and see its result in the parent.
- [ ] Model and tool-call compatibility across the supported local servers and attachment formats.

## Beta feedback

- [ ] A private beta, with material issues closed before public publication.

## Not release gates

Deferred: a Postgres memory adapter, additional search providers, and application-level end-to-end
encryption for the relay.
