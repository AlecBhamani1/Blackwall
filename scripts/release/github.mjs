import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { appendFileSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { betaVersion, compareReleaseVersions, requiredAssets, requireSuccessfulCI, validateManifest, versionParts } from './validate.mjs';
import { validateReleaseNotes } from './promotion.mjs';

const repository = process.env.GITHUB_REPOSITORY;
const channel = process.env.RELEASE_CHANNEL ?? 'stable';
assert(['stable', 'beta'].includes(channel), 'Unknown release channel');
const beta = channel === 'beta';
const branch = beta ? 'partial' : 'main';
const feedTag = beta ? 'beta' : 'main';
const baseVersion = JSON.parse(readFileSync('.github/release.json', 'utf8')).version;
versionParts(baseVersion);
const version = beta ? betaVersion(baseVersion, process.env.GITHUB_RUN_NUMBER) : baseVersion;
if (process.env.RELEASE_VERSION) assert.equal(process.env.RELEASE_VERSION, version, 'Build version must match the committed release plan');
const sha = process.env.RELEASE_COMMIT ?? process.env.GITHUB_SHA;
assert.match(repository ?? '', /^[\w.-]+\/[\w.-]+$/);
assert.match(sha ?? '', /^[a-f0-9]{40}$/);
const tag = `v${version}`;
const prefix = `repos/${repository}`;

function gh(args, options = {}) {
  return execFileSync('gh', args, { encoding: 'utf8', maxBuffer: 128 * 1024 * 1024, ...options });
}
function api(path, method = 'GET', body) {
  return JSON.parse(gh(['api', `${prefix}/${path}`, '--method', method,
    ...(body ? ['--input', '-'] : [])], { input: body ? JSON.stringify(body) : undefined }));
}
function optional(path) {
  try { return api(path); } catch (error) {
    if (String(error.stderr).includes('(HTTP 404)')) return null;
    throw error;
  }
}
function findRelease(tagName) {
  const pages = JSON.parse(gh(['api', `${prefix}/releases?per_page=100`, '--paginate', '--slurp']));
  return pages.flat().find(release => release.tag_name === tagName);
}
function assetBytes(asset) {
  const bytes = gh(['api', `${prefix}/releases/assets/${asset.id}`, '-H', 'Accept: application/octet-stream'], { encoding: 'buffer' });
  assert.equal(bytes.length, asset.size, `Incomplete download: ${asset.name}`);
  assert.equal(`sha256:${createHash('sha256').update(bytes).digest('hex')}`, asset.digest,
    `Checksum mismatch: ${asset.name}`);
  return bytes;
}
function feed(release) {
  const asset = release.assets.find(asset => asset.name === 'latest.json');
  assert(asset, 'Compatibility release is missing latest.json');
  return JSON.parse(assetBytes(asset).toString());
}
function assertReleaseCommit(release) {
  assert.equal(release.target_commitish, sha, 'Release belongs to a different commit');
  const ref = optional(`git/ref/tags/${tag}`);
  if (ref) {
    // This workflow creates lightweight tags. Refuse unexpected annotated/moved tags.
    assert.equal(ref.object.type, 'commit');
    assert.equal(ref.object.sha, sha, 'Release tag belongs to a different commit');
  }
}
function assertCI() {
  const result = api(`actions/workflows/ci.yml/runs?head_sha=${sha}&branch=${branch}&event=push&per_page=100`);
  requireSuccessfulCI(result.workflow_runs, sha, branch);
}

async function prepare() {
  assertReleaseTrigger();
  if (!beta) assert.equal(process.env.RELEASE_APPROVED, 'true', 'Release acceptance must be explicitly confirmed');
  assertCI();
  const notes = beta
    ? `# Blackwall ${version}\n\nPreview of partial. Features may be unfinished and contain bugs.\n\n${readFileSync('CHANGELOG.md', 'utf8').split('## [Unreleased]')[1]?.split(/\n## \[/)[0]?.trim() ?? ''}`
    : readFileSync(`docs/releases/${version}.md`, 'utf8').trim();
  if (!beta) validateReleaseNotes(notes + '\n', version);
  const existing = findRelease(tag);
  const compatibility = beta ? optional(`releases/tags/${feedTag}`) : api('releases/tags/main');
  if (compatibility?.assets.some(asset => asset.name === 'latest.json')) {
    const current = feed(compatibility);
    const comparison = compareReleaseVersions(version, current.version);
    assert(existing && !existing.draft ? comparison >= 0 : comparison > 0,
      'Version must be newer than the feed, or a retry of its published version');
  } else if (!beta) {
    assert(existing && !existing.draft && api('releases/latest').id === existing.id,
      'Only the published Latest release may repair a missing feed');
  }
  if (beta) assertLatestBeta();
  if (existing && !existing.draft && !beta) {
    assert.equal(api('releases/latest').id, existing.id, 'Only the current Latest release may be retried');
  }
  let release;
  if (existing) {
    assertReleaseCommit(existing);
    release = existing;
  } else {
    assert(!optional(`git/ref/tags/${tag}`), 'Tag already exists; choose an unused version');
    release = api('releases', 'POST', {
      tag_name: tag, target_commitish: sha, name: `Blackwall ${tag}`,
      body: `${notes}\n\nSource commit: ${sha}.`, draft: true, prerelease: beta, make_latest: 'false',
    });
  }
  appendFileSync(process.env.GITHUB_OUTPUT, `release_id=${release.id}\nversion=${version}\ncommit=${sha}\nbuild_required=${release.draft}\n`);
}

function assertReleaseTrigger() {
  if (process.env.GITHUB_EVENT_NAME === 'workflow_run') {
    const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
    assert.equal(event.workflow_run.event, 'push');
    assert.equal(event.workflow_run.head_branch, branch);
    assert.equal(event.workflow_run.head_repository.full_name, repository);
    assert.equal(event.workflow_run.conclusion, 'success');
    assert.equal(event.workflow_run.head_sha, sha);
  } else {
    assert.equal(process.env.GITHUB_EVENT_NAME, 'workflow_dispatch', 'Unsupported release event');
    assert.equal(process.env.GITHUB_REF, `refs/heads/${branch}`, `Dispatch releases from ${branch} only`);
  }
}

// An old beta job may resume after a newer job published. Never move its feed back.
function assertLatestBeta() {
  const pages = JSON.parse(gh(['api', `${prefix}/releases?per_page=100`, '--paginate', '--slurp']));
  for (const release of pages.flat()) {
    if (release.draft || !release.prerelease || !/^v\d+\.\d+\.\d+-beta\.\d+$/.test(release.tag_name)) continue;
    assert(compareReleaseVersions(version, release.tag_name.slice(1)) >= 0,
      'A newer beta version is already published');
  }
}

async function publish() {
  assertReleaseTrigger();
  if (!beta) assert.equal(process.env.RELEASE_APPROVED, 'true');
  assertCI();
  const release = findRelease(tag);
  assert(release, 'Draft release is missing');
  assertReleaseCommit(release);
  const signatures = {};
  // Verify uploaded bytes before making the release visible or changing the feed.
  for (const name of requiredAssets(version)) {
    const asset = release.assets.find(asset => asset.name === name);
    assert(asset, `Missing release file: ${name}`);
    const bytes = assetBytes(asset);
    if (name.endsWith('.sig')) signatures[name] = bytes.toString();
  }
  // Optional signed installers, such as Debian packages, may add their own feed entries.
  for (const asset of release.assets) {
    if (asset.name.endsWith('.sig') && !(asset.name in signatures)) {
      signatures[asset.name] = assetBytes(asset).toString();
    }
  }
  const manifest = validateManifest(release, feed(release), signatures, repository, version, channel);
  const compatibility = beta ? optional(`releases/tags/${feedTag}`) : api('releases/tags/main');
  const latest = beta ? null : api('releases/latest');
  if (beta) assertLatestBeta();
  if (latest && latest.tag_name !== 'main') {
    assert(compareReleaseVersions(version, latest.tag_name.replace(/^v/, '')) >= 0,
      'A newer version is already published');
  }
  if (compatibility?.assets.some(asset => asset.name === 'latest.json')) {
    const current = feed(compatibility);
    const comparison = compareReleaseVersions(version, current.version);
    assert(comparison >= 0, 'Refusing to downgrade the updater feed');
    if (comparison === 0) {
      assert.deepEqual(current, manifest, 'This version is already served with different update metadata');
    }
  } else if (!beta) {
    // gh --clobber deletes the old asset before uploading its replacement. If that
    // upload failed, only the already-published Latest release may repair the feed.
    assert(!release.draft && latest.id === release.id,
      'Missing feed can only be repaired from the published Latest release');
  }
  const directory = mkdtempSync(join(tmpdir(), 'blackwall-release-'));
  try {
    const path = join(directory, 'latest.json');
    writeFileSync(path, JSON.stringify(manifest, null, 2) + '\n');
    if (release.draft) {
      api(`releases/${release.id}`, 'PATCH', { draft: false, make_latest: beta ? 'false' : 'true' });
    }
    if (beta && !compatibility) {
      api('releases', 'POST', { tag_name: feedTag, target_commitish: sha, name: 'Blackwall beta updates',
        body: 'Signed preview builds from partial. See the versioned beta releases for downloads and changes.',
        draft: false, prerelease: true, make_latest: 'false' });
    }
    gh(['release', 'upload', feedTag, path, '--clobber', '--repo', repository]);
    assert.deepEqual(feed(api(`releases/tags/${feedTag}`)), manifest, 'Feed verification failed');
    console.log(`Published https://github.com/${repository}/releases/tag/${tag}`);
    if (process.env.GITHUB_STEP_SUMMARY) {
      appendFileSync(process.env.GITHUB_STEP_SUMMARY,
        `Published [Blackwall ${tag}](https://github.com/${repository}/releases/tag/${tag}) from source commit \`${sha}\`.\n\nEvery platform download, updater signature, and the compatibility feed were verified.\n`);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

const command = process.argv[2];
assert(['prepare', 'publish'].includes(command), 'Use prepare or publish');
await (command === 'prepare' ? prepare() : publish());
