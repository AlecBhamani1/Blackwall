# Contributing to Blackwall

Thank you for helping make Blackwall a dependable local-first agent harness. This guide
describes the baseline expected of every change. The architecture and security invariants are
documented in `docs/PLAN.md` while the initial implementation is being built.

## Development setup

Blackwall is macOS-first. You will need:

- the current stable Rust toolchain with `rustfmt` and `clippy`;
- Node.js 22.12 or newer and npm;
- the [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for macOS; and
- `cargo-deny` for dependency-policy checks.

Install the repository-local JavaScript tooling and Git hooks:

```sh
npm install
npm --prefix ui install
```

The root install registers the hooks through the `prepare` script. Run `npm run hooks:install`
to refresh them manually if you installed dependencies with lifecycle scripts disabled.

Run the desktop app during development:

```sh
export BLACKWALL_MODEL_ENDPOINT=http://your-model-host:11434
npm run desktop
```

`OLLAMA_HOST` is also supported when it is already configured. Blackwall connects to the selected
endpoint; it never starts or manages Ollama. A model server is optional for build and test
commands, and automated tests must not depend on a live network service.

## Architecture boundaries

- `src/core` is a framework-free Rust library. It must not depend on Tauri or UI code.
- `src/app` and `src/bw` are thin adapters around the core library.
- `ui` is a presentation layer. Rust-to-UI communication goes through the typed serde event
  protocol rather than ad hoc payloads.
- Persistent application data stays under `~/.blackwall/`; credentials and API keys belong in
  the platform keychain, never in source, logs, fixtures, or config JSON.
- File tools remain jailed to the selected workspace. Network access and mutating tools remain
  approval-gated.

Use typed errors in the core and reserve `anyhow` for application edges. Avoid `unwrap` and
`expect` in production code. All I/O must be asynchronous where practical, timeout-capped, and
bounded in output size.

## Quality checks

Run the checks relevant to your change before opening a pull request:

```sh
cargo fmt --manifest-path src/Cargo.toml --all -- --check
cargo clippy --manifest-path src/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path src/Cargo.toml --workspace --all-features
cargo deny --manifest-path src/Cargo.toml check
npm --prefix ui run check
npm --prefix ui run test:run
npm --prefix ui run build
```

Tests should be deterministic and offline. Use scripted model backends and local test servers;
cover both the success path and security boundaries such as path traversal, denial decisions,
timeouts, interrupts, and SSRF protections.

Lefthook runs formatting and static analysis before commits, test suites before pushes, and
commitlint on commit messages. Hooks supplement CI; they do not replace it.

## Commits and pull requests

Use [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/):

```text
feat(core): stream assistant deltas
fix(tools): reject paths outside the workspace
docs: clarify local model setup
```

Keep commits focused and write subjects in the imperative mood. Use `!` and a `BREAKING CHANGE:`
footer for incompatible changes. Pull requests should explain the user impact, identify security
or data-migration consequences, list verification performed, and include screenshots for visible
UI changes. Update `CHANGELOG.md` for user-visible behavior.

Do not commit secrets, model transcripts containing private data, generated build output, or local
database files. Report suspected vulnerabilities privately to the repository maintainers instead
of opening a public issue.

## Licensing

Blackwall is licensed under the Apache License, Version 2.0. Unless you explicitly state otherwise,
any contribution intentionally submitted for inclusion in Blackwall is provided under that same
license, without additional terms or conditions. See `LICENSE` for the complete terms.
