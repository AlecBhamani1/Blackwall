import assert from 'node:assert/strict';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { compareVersions, versionParts } from './validate.mjs';

const version = process.argv[2];
versionParts(version);
const current = JSON.parse(readFileSync('.github/release.json', 'utf8'));
assert(compareVersions(version, current.version) > 0, 'Choose a version newer than the current release plan');
const notesPath = `docs/releases/${version}.md`;
assert(!existsSync(notesPath), 'Release notes already exist; update the release plan manually to avoid overwriting them');
writeFileSync(notesPath, `# Blackwall ${version}

TODO: Describe the release and link the v${version} milestone.

## Changes

- TODO: Describe the changes in this batch and link the relevant issues or pull requests.

## Downloads

- macOS: \`aarch64.dmg\` for Apple Silicon or \`x64.dmg\` for Intel Macs.
- Windows: \`x64-setup.exe\`.
- Linux: \`amd64.AppImage\`/\`amd64.deb\` for x86-64 or \`aarch64.AppImage\`/\`arm64.deb\` for ARM64.
Existing installations continue to receive update information through the original feed.

## Acceptance status

TODO: Record automated checks, applicable native acceptance evidence, and any remaining limitations.
Updater signatures authenticate updates; they do not establish Apple Developer ID signing, notarization, or Windows code signing.
`);
writeFileSync('.github/release.json', JSON.stringify({ version }, null, 2) + '\n');
console.log(`Prepared ${notesPath}. Complete the notes, update CHANGELOG.md, and assign the promotion PR to milestone v${version}.`);
