# Repository Guidelines

## Project Structure & Module Organization

Blackwall is a macOS-first, local-first agent harness. `src/Cargo.toml` defines four Rust crates: `core` (framework-free runtime), `app` (Tauri adapter), `bw` (CLI), and `relay` (sharing service). Keep adapters thin and core independent of Tauri. `ui/src/` contains Svelte/TypeScript code; `ui/src/lib/components/` holds components. Guest assets live in `src/core/src/share/assets/`; icons in `src/app/icons/`. Rust tests live beside modules and in crate `tests/` directories; UI tests use adjacent `*.test.ts` files. `docs/` covers architecture/workflows, `scripts/` contains release/repository tooling, and `deploy/relay/` contains deployment configuration.

## Build, Test, and Development Commands

Use Rust 1.90+, Node.js 22.12+, npm, Tauri prerequisites, and `cargo-deny`. Run commands from the root:

- `npm install` and `npm --prefix ui install`: install tooling, Git hooks, and UI dependencies.
- `npm run desktop`: launch Tauri development; `npm run dev`: launch browser UI at `http://127.0.0.1:1420`.
- `npm run desktop:build`: build desktop bundles; `npm run build`: build UI assets.
- `npm run verify`: run UI type checks, tests, and build.
- `npm run test:release`: test release and repository scripts.

Before submitting Rust changes:

```sh
cargo fmt --manifest-path src/Cargo.toml --all -- --check
cargo clippy --manifest-path src/Cargo.toml --workspace --all-targets --all-features -- -D warnings
cargo test --manifest-path src/Cargo.toml --workspace --all-features
cargo deny --manifest-path src/Cargo.toml check
```

## Coding Style & Naming Conventions

Use four-space Rust indentation, rustfmt's 100-column limit, `snake_case` functions/modules, and `PascalCase` types. Use two-space Svelte/TypeScript indentation, `PascalCase.svelte` components, and `camelCase` helpers. UI Prettier configuration uses single quotes and 100 columns. Use typed core errors; avoid production `unwrap`/`expect` and unsafe code. Lefthook enforces formatting/static checks and commitlint.

## Testing Guidelines

Use Rust unit/integration tests and Vitest with jsdom and Svelte Testing Library. Keep tests deterministic and offline with scripted backends/local servers. Cover failures and security boundaries, including traversal, denials, timeouts, and SSRF. `npm test` runs UI tests; `npm --prefix ui run test:coverage` collects coverage. No numeric coverage threshold is configured.

## Commit & Pull Request Guidelines

Follow history's Conventional Commits: `feat(core): ...`, `fix(tools): ...`, `docs: ...`; limit subjects to 100 characters. Branch from `origin/partial` and target `partial`. Link issues, assign a version milestone, describe user impact and verification, and include screenshots for UI changes. Explain security/migration consequences and update `CHANGELOG.md` for user-visible behavior. Follow `CONTRIBUTING.md` and `docs/REPOSITORY_WORKFLOW.md` for production promotions.

## Security & Configuration

Store application data under `~/.blackwall/` and credentials in the platform keychain. Keep file tools workspace-jailed and network/mutating tools approval-gated. Never commit secrets, private transcripts, databases, or build output.
