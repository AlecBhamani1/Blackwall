import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { compareVersions, versionParts } from './validate.mjs';

export const releaseChecklist = [
  'Release notes describe this batch and link its milestone.',
  'CI and applicable native acceptance checks are complete; evidence and remaining limitations are recorded in the release notes.',
  'I approve publishing this version and advancing the signed updater feed.',
];

export function validateReleaseNotes(notes, version) {
  versionParts(version);
  assert(notes.trim().length >= 80, 'Commit meaningful release notes before publishing');
  assert(notes.startsWith(`# Blackwall ${version}\n`), 'Release notes must start with the version heading');
  for (const heading of ['Changes', 'Downloads', 'Acceptance status']) {
    assert(notes.includes(`\n## ${heading}\n`), `Release notes need a ${heading} section`);
  }
  assert(!/\b(?:TODO|TBD|PLACEHOLDER)\b/i.test(notes), 'Complete the release-note placeholders before promotion');
}

export function validatePromotion(event, version, previousVersion, notes) {
  const pr = event.pull_request;
  assert(pr, 'Production changes must use a pull request');
  assert.equal(pr.base.ref, 'main');
  assert.equal(pr.head.ref, 'partial', 'Only the long-lived partial branch can promote to main');
  assert.equal(pr.head.repo?.full_name, pr.base.repo.full_name, 'Promotion must come from this repository');
  assert(compareVersions(version, previousVersion) > 0, 'Every production promotion needs a newer stable version');
  validateReleaseNotes(notes, version);
  assert.equal(pr.milestone?.title, `v${version}`, 'Assign the promotion PR to its v<version> milestone');
  assert(notes.includes(pr.milestone.html_url), 'Link the release milestone in the release notes');
  // Ignore examples and hidden comments: approval must be a visible checked item.
  const body = (pr.body ?? '').replace(/<!--[\s\S]*?-->/g, '').replace(/```[\s\S]*?```/g, '');
  const checked = body.split('\n').filter(line => /^\s*- \[[xX]\] /.test(line)).map(line => line.trim().slice(6));
  for (const item of releaseChecklist) {
    assert(checked.includes(item), `Complete the production checklist: ${item}`);
  }
}

export function previousReleaseVersion(ref) {
  const git = args => execFileSync('git', args, { encoding: 'utf8' }).trim();
  const files = git(['ls-tree', '-r', '--name-only', ref]).split('\n');
  if (files.includes('.github/release.json')) {
    return JSON.parse(git(['show', `${ref}:.github/release.json`])).version;
  }
  // One-time migration from the existing manually versioned releases.
  const versions = files.map(file => /^docs\/releases\/(\d+\.\d+\.\d+)\.md$/.exec(file)?.[1]).filter(Boolean);
  assert(versions.length > 0, 'The production branch needs a release-version baseline');
  return versions.sort(compareVersions).at(-1);
}

function main() {
  if (process.env.GITHUB_EVENT_NAME !== 'pull_request') {
    console.log('Promotion checks apply to pull requests; production publication separately requires passing main CI.');
    return;
  }
  const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
  const pr = event.pull_request;
  assert(['partial', 'main'].includes(pr.base.ref), 'Target partial for development or main for a release');
  if (pr.base.ref === 'partial') {
    console.log('Development pull request targets partial.');
    return;
  }
  const version = JSON.parse(readFileSync('.github/release.json', 'utf8')).version;
  versionParts(version);
  const notes = readFileSync(`docs/releases/${version}.md`, 'utf8');
  validatePromotion(event, version, previousReleaseVersion(pr.base.sha), notes);
  execFileSync('git', ['merge-base', '--is-ancestor', pr.base.sha, pr.head.sha]);
  const previousVersion = previousReleaseVersion(pr.base.sha);
  const repository = pr.base.repo.full_name;
  const latest = JSON.parse(execFileSync('gh', ['api', `repos/${repository}/releases/latest`], { encoding: 'utf8' }));
  assert.equal(latest.tag_name, `v${previousVersion}`, 'Wait for the previous production release to finish before promoting another batch');
  const feed = JSON.parse(execFileSync('gh', ['release', 'download', 'main', '--pattern', 'latest.json', '--output', '-', '--repo', repository], { encoding: 'utf8' }));
  assert.equal(feed.version, previousVersion, 'Wait for the previous release to advance the compatibility feed');
  console.log(`Approved promotion contract for v${version}; main is an ancestor of partial.`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
