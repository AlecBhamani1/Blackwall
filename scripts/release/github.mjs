import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { appendFileSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { compareVersions, requiredAssets, requireSuccessfulCI, validateManifest, versionParts } from './validate.mjs';
import { validateReleaseNotes } from './promotion.mjs';

const repository = process.env.GITHUB_REPOSITORY;
const version = JSON.parse(readFileSync('.github/release.json', 'utf8')).version;
if (process.env.RELEASE_VERSION) assert.equal(process.env.RELEASE_VERSION, version, 'Build version must match the committed release plan');
const sha = process.env.RELEASE_COMMIT ?? process.env.GITHUB_SHA;
assert.match(repository ?? '', /^[\w.-]+\/[\w.-]+$/);
versionParts(version);
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
  const result = api(`actions/workflows/ci.yml/runs?head_sha=${sha}&branch=main&event=push&per_page=100`);
  requireSuccessfulCI(result.workflow_runs, sha);
}

async function prepare() {
  assertProductionTrigger();
  assert.equal(process.env.RELEASE_APPROVED, 'true', 'Release acceptance must be explicitly confirmed');
  assertCI();
  const notes = readFileSync(`docs/releases/${version}.md`, 'utf8').trim();
  validateReleaseNotes(notes + '\n', version);
  const existing = findRelease(tag);
  const compatibility = api('releases/tags/main');
  if (compatibility.assets.some(asset => asset.name === 'latest.json')) {
    const current = feed(compatibility);
    const comparison = compareVersions(version, current.version);
    assert(existing && !existing.draft ? comparison >= 0 : comparison > 0,
      'Version must be newer than the feed, or a retry of its published version');
  } else {
    assert(existing && !existing.draft && api('releases/latest').id === existing.id,
      'Only the published Latest release may repair a missing feed');
  }
  if (existing && !existing.draft) {
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
      body: `${notes}\n\nSource commit: ${sha}.`, draft: true, prerelease: false, make_latest: 'false',
    });
  }
  appendFileSync(process.env.GITHUB_OUTPUT, `release_id=${release.id}\nversion=${version}\ncommit=${sha}\nbuild_required=${release.draft}\n`);
}

function assertProductionTrigger() {
  if (process.env.GITHUB_EVENT_NAME === 'workflow_run') {
    const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8'));
    assert.equal(event.workflow_run.event, 'push');
    assert.equal(event.workflow_run.head_branch, 'main');
    assert.equal(event.workflow_run.head_repository.full_name, repository);
    assert.equal(event.workflow_run.conclusion, 'success');
    assert.equal(event.workflow_run.head_sha, sha);
  } else {
    assert.equal(process.env.GITHUB_REF, 'refs/heads/main', 'Dispatch releases from main only');
  }
}

async function publish() {
  assertProductionTrigger();
  assert.equal(process.env.RELEASE_APPROVED, 'true');
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
  const manifest = validateManifest(release, feed(release), signatures, repository, version);
  const compatibility = api('releases/tags/main');
  const latest = api('releases/latest');
  if (latest.tag_name !== 'main') {
    assert(compareVersions(version, latest.tag_name.replace(/^v/, '')) >= 0,
      'A newer version is already published');
  }
  if (compatibility.assets.some(asset => asset.name === 'latest.json')) {
    const current = feed(compatibility);
    const comparison = compareVersions(version, current.version);
    assert(comparison >= 0, 'Refusing to downgrade the updater feed');
    if (comparison === 0) {
      assert.deepEqual(current, manifest, 'This version is already served with different update metadata');
    }
  } else {
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
      api(`releases/${release.id}`, 'PATCH', { draft: false, make_latest: 'true' });
    }
    gh(['release', 'upload', 'main', path, '--clobber', '--repo', repository]);
    assert.deepEqual(feed(api('releases/tags/main')), manifest, 'Feed verification failed');
    console.log(`Published https://github.com/${repository}/releases/tag/${tag}`);
    if (process.env.GITHUB_STEP_SUMMARY) {
      appendFileSync(process.env.GITHUB_STEP_SUMMARY,
        `Published [Blackwall ${tag}](https://github.com/${repository}/releases/tag/${tag}) from source commit \`${sha}\`.\n\nBoth architecture downloads, updater signatures, and the compatibility feed were verified.\n`);
    }
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

const command = process.argv[2];
assert(['prepare', 'publish'].includes(command), 'Use prepare or publish');
await (command === 'prepare' ? prepare() : publish());
