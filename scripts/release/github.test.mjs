import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import test from 'node:test';
import { requiredAssets } from './validate.mjs';

const script = fileURLToPath(new URL('./github.mjs', import.meta.url));
const repository = 'example/blackwall';
const sha = 'a'.repeat(40);

function fixture(t) {
  const directory = mkdtempSync(join(tmpdir(), 'blackwall-release-test-'));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  const state = { releases: [], blobs: {}, mutations: [], sha, latest: 1 };
  const release = { id: 2, tag_name: 'v0.1.4', target_commitish: sha, draft: true, prerelease: false, assets: [] };
  const compatibility = { id: 1, tag_name: 'main', draft: false, assets: [] };
  state.releases.push(compatibility, release);
  function asset(owner, name, contents) {
    const bytes = Buffer.from(contents);
    const id = Object.keys(state.blobs).length + 10;
    state.blobs[id] = bytes.toString('base64');
    owner.assets.push({ id, name, state: 'uploaded', size: bytes.length,
      digest: `sha256:${createHash('sha256').update(bytes).digest('hex')}`,
      url: `https://api.github.com/repos/${repository}/releases/assets/${id}`,
      browser_download_url: `https://github.com/${repository}/releases/download/${owner.tag_name}/${name}` });
  }
  for (const name of requiredAssets('0.1.4')) asset(release, name, name.endsWith('.sig') ? `signed-${name}\n` : `binary-${name}`);
  const platforms = {};
  for (const [platform, arch] of [['darwin-aarch64', 'aarch64'], ['darwin-x86_64', 'x64']]) {
    const name = `Blackwall_0.1.4_${arch}.app.tar.gz`;
    platforms[platform] = { signature: `signed-${name}.sig`, url: release.assets.find(asset => asset.name === name).url };
  }
  asset(release, 'latest.json', JSON.stringify({ version: '0.1.4', notes: 'Release notes', pub_date: '2026-09-29T12:00:00Z', platforms }));
  asset(compatibility, 'latest.json', JSON.stringify({ version: '0.1.3' }));
  mkdirSync(join(directory, 'bin'));
  mkdirSync(join(directory, 'docs/releases'), { recursive: true });
  writeFileSync(join(directory, 'docs/releases/0.1.4.md'), '# Release notes\n' + 'Reviewed release changes. '.repeat(5));
  const mock = join(directory, 'bin/gh');
  writeFileSync(mock, `#!${process.execPath}
import fs from 'node:fs';
import crypto from 'node:crypto';
const file = process.env.MOCK_STATE;
const state = JSON.parse(fs.readFileSync(file));
const args = process.argv.slice(2);
const save = () => fs.writeFileSync(file, JSON.stringify(state));
const out = value => { save(); process.stdout.write(JSON.stringify(value)); };
const missing = () => { process.stderr.write('gh: Not Found (HTTP 404)'); process.exit(1); };
if (args[0] === 'api') {
  const path = args[1].replace('repos/example/blackwall/', '');
  const method = args.includes('--method') ? args[args.indexOf('--method') + 1] : 'GET';
  if (method === 'PATCH') {
    const release = state.releases.find(item => item.id === Number(path.split('/').at(-1)));
    Object.assign(release, JSON.parse(fs.readFileSync(0, 'utf8')));
    state.latest = release.id;
    state.mutations.push('publish');
    out(release);
  } else if (method === 'POST') {
    const release = { id: 3, assets: [], ...JSON.parse(fs.readFileSync(0, 'utf8')) };
    state.releases.push(release);
    state.mutations.push('create-draft');
    out(release);
  } else if (path.startsWith('actions/')) {
    out({ workflow_runs: [{ head_sha: state.sha, event: 'push', head_branch: 'main', run_number: 1, run_attempt: 1, status: 'completed', conclusion: state.ciFailure ? 'failure' : 'success' }] });
  } else if (path.startsWith('releases?')) out([state.releases]);
  else if (path === 'releases/latest') out(state.releases.find(item => item.id === state.latest));
  else if (path.startsWith('releases/tags/')) {
    const release = state.releases.find(item => item.tag_name === path.split('/').at(-1));
    if (!release) missing();
    out(release);
  } else if (path.startsWith('releases/assets/')) {
    const id = path.split('/').at(-1);
    if (!state.blobs[id]) missing();
    process.stdout.write(Buffer.from(state.blobs[id], 'base64'));
  } else if (path.startsWith('git/ref/tags/')) {
    if (!state.tagSha) missing();
    out({ object: { type: 'commit', sha: state.tagSha } });
  } else throw new Error('Unexpected API call: ' + path);
} else if (args[0] === 'release' && args[1] === 'upload') {
  if (args[2] !== 'main') throw new Error('Versioned files must not be overwritten');
  const release = state.releases.find(item => item.tag_name === 'main');
  release.assets = release.assets.filter(item => item.name !== 'latest.json');
  if (state.failUpload) { state.failUpload = false; save(); process.exit(1); }
  const bytes = fs.readFileSync(args[3]);
  state.blobs[500] = bytes.toString('base64');
  release.assets.push({ id: 500, name: 'latest.json', state: 'uploaded', size: bytes.length, digest: 'sha256:' + crypto.createHash('sha256').update(bytes).digest('hex') });
  state.mutations.push('advance-feed');
  save();
} else throw new Error('Unexpected gh command: ' + args.join(' '));
`);
  chmodSync(mock, 0o755);
  const statePath = join(directory, 'state.json');
  const save = () => writeFileSync(statePath, JSON.stringify(state));
  const read = () => JSON.parse(readFileSync(statePath));
  const run = (command, overrides = {}) => spawnSync(process.execPath, [script, command], {
    cwd: directory, encoding: 'utf8',
    env: { ...process.env, PATH: `${directory}/bin:${process.env.PATH}`, MOCK_STATE: statePath,
      GITHUB_REPOSITORY: repository, GITHUB_SHA: sha, GITHUB_REF: 'refs/heads/main',
      GITHUB_OUTPUT: join(directory, 'output'), RELEASE_VERSION: '0.1.4', RELEASE_APPROVED: 'true', ...overrides },
  });
  save();
  return { state, release, compatibility, directory, save, read, run };
}

test('publication verifies draft, publishes first, then advances the compatibility feed', t => {
  const f = fixture(t);
  const result = f.run('publish');
  assert.equal(result.status, 0, result.stderr);
  assert.deepEqual(f.read().mutations, ['publish', 'advance-feed']);
  assert.equal(f.read().releases[1].draft, false);
  assert.equal(f.run('publish').status, 0, 'Already-published releases can repair/reconfirm their feed');
  assert.deepEqual(f.read().mutations, ['publish', 'advance-feed', 'advance-feed']);
});

test('a failed upload after deletion is recoverable without rebuilding or editing published files', t => {
  const f = fixture(t);
  f.state.failUpload = true;
  f.save();
  assert.notEqual(f.run('publish').status, 0);
  assert.equal(f.read().releases[1].draft, false);
  assert.equal(f.read().releases[0].assets.length, 0);
  const retry = f.run('publish');
  assert.equal(retry.status, 0, retry.stderr);
  assert.deepEqual(f.read().mutations, ['publish', 'advance-feed']);
});

for (const [name, mutate] of [
  ['incomplete build', f => { f.release.assets.pop(); }],
  ['corrupt download', f => { f.state.blobs[f.release.assets[0].id] = Buffer.from('tampered').toString('base64'); }],
  ['moved release tag', f => { f.state.tagSha = 'b'.repeat(40); }],
  ['different draft commit', f => { f.release.target_commitish = 'b'.repeat(40); }],
  ['failed CI', f => { f.state.ciFailure = true; }],
  ['newer published version', f => { f.state.releases.push({ id: 3, tag_name: 'v0.1.5' }); f.state.latest = 3; }],
]) {
  test(`${name} cannot publish or change the existing feed`, t => {
    const f = fixture(t);
    mutate(f);
    f.save();
    assert.notEqual(f.run('publish').status, 0);
    assert.deepEqual(f.read().mutations, []);
  });
}

test('prepare requires explicit approval on main and refuses published versions', t => {
  const f = fixture(t);
  assert.notEqual(f.run('prepare', { RELEASE_APPROVED: 'false' }).status, 0);
  assert.notEqual(f.run('prepare', { GITHUB_REF: 'refs/heads/feature' }).status, 0);
  f.release.draft = false;
  f.save();
  assert.notEqual(f.run('prepare').status, 0);
  assert.deepEqual(f.read().mutations, []);
});

test('prepare creates a commit-pinned draft with committed release notes, and can resume it', t => {
  const f = fixture(t);
  f.state.releases.pop();
  f.save();
  const result = f.run('prepare');
  assert.equal(result.status, 0, result.stderr);
  const draft = f.read().releases[1];
  assert.equal(draft.target_commitish, sha);
  assert.equal(draft.tag_name, 'v0.1.4');
  assert.equal(draft.draft, true);
  assert(draft.body.includes('Reviewed release changes'));
  assert.match(readFileSync(join(f.directory, 'output'), 'utf8'), /release_id=3/);
  assert.equal(f.run('prepare').status, 0);
  assert.deepEqual(f.read().mutations, ['create-draft']);
});
