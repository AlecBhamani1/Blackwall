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

Use the \`aarch64.dmg\` installer for Apple Silicon or the \`x64.dmg\` installer for Intel Macs.
Existing installations continue to receive update information through the original feed.

## Acceptance status

TODO: Record automated checks, applicable native acceptance evidence, and any remaining limitations.
Updater signatures authenticate updates; they do not establish Apple Developer ID signing or notarization.
`);
writeFileSync('.github/release.json', JSON.stringify({ version }, null, 2) + '\n');
console.log(`Prepared ${notesPath}. Complete the notes, update CHANGELOG.md, and assign the promotion PR to milestone v${version}.`);
