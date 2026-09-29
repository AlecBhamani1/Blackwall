# Security policy

Report suspected vulnerabilities using [GitHub private vulnerability reporting](https://github.com/AlecBhamani1/Blackwall/security/advisories/new).
Include the affected version, reproduction steps, impact, and a minimal sanitized example.
Do not include credentials, private model transcripts, Keychain values, or personal files.

The latest published version receives security fixes. Older versions may require an upgrade.
Blackwall remains a development build; distribution and native acceptance limitations are documented
in each release's acceptance status and in [the native acceptance plan](docs/NATIVE_ACCEPTANCE.md).

Security fixes follow the same checked `partial` → `main` promotion process. If a fix is urgent,
pause unrelated integration work and release a focused patch batch. Never force-push a published
tag or change the files of a published version. Rotate exposed credentials and use a new version
for a corrected build.
