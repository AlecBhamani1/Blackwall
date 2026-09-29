import assert from 'node:assert/strict';
import test from 'node:test';
import { compareVersions, requiredAssets, requireSuccessfulCI, validateManifest, versionParts } from './validate.mjs';

const repository = 'example/blackwall';
const version = '0.1.4';
function fixture() {
  const assets = requiredAssets(version).map((name, id) => ({
    id, name, state: 'uploaded', size: 100,
    url: `https://api.github.com/repos/${repository}/releases/assets/${id}`,
    browser_download_url: `https://github.com/${repository}/releases/download/v${version}/${name}`,
  }));
  const signatures = {};
  const platforms = {};
  for (const [platform, arch] of [['darwin-aarch64', 'aarch64'], ['darwin-x86_64', 'x64']]) {
    const name = `Blackwall_${version}_${arch}.app.tar.gz`;
    signatures[name + '.sig'] = `signature-${arch}\n`;
    const entry = { signature: `signature-${arch}`, url: assets.find(asset => asset.name === name).url };
    platforms[platform] = { ...entry };
    platforms[platform + '-app'] = { ...entry };
  }
  return {
    release: { tag_name: `v${version}`, prerelease: false, assets },
    manifest: { version, notes: 'Release notes', pub_date: '2026-09-29T12:00:00Z', platforms },
    signatures,
  };
}
function validate(f) { return validateManifest(f.release, f.manifest, f.signatures, repository, version); }

test('version comparison is numeric and rejects unsafe or ambiguous inputs', () => {
  assert.equal(compareVersions('0.1.10', '0.1.9'), 1);
  assert.equal(compareVersions('1.0.0', '0.99.99'), 1);
  assert.equal(compareVersions('0.1.3', '0.1.4'), -1);
  assert.equal(compareVersions('0.1.4', '0.1.4'), 0);
  for (const value of ['v0.1.4', '0.01.4', '0.1.4-beta', '../0.1.4', '0.1.4\n', '0.1.4; echo unsafe', '', undefined]) {
    assert.throws(() => versionParts(value));
  }
});

test('CI must be the latest main push for the exact release commit', () => {
  const success = { head_sha: 'expected', head_branch: 'main', event: 'push', status: 'completed', conclusion: 'success', run_number: 3, run_attempt: 1 };
  assert.doesNotThrow(() => requireSuccessfulCI([success], 'expected'));
  for (const change of [{ head_sha: 'other' }, { event: 'pull_request' }, { head_branch: 'feature' }, { status: 'in_progress' }, { conclusion: 'failure' }]) {
    assert.throws(() => requireSuccessfulCI([{ ...success, ...change }], 'expected'));
  }
  assert.throws(() => requireSuccessfulCI([success, { ...success, run_number: 4, conclusion: 'failure' }], 'expected'));
  assert.throws(() => requireSuccessfulCI([success, { ...success, run_attempt: 2, status: 'in_progress' }], 'expected'));
});

test('complete releases retain signatures and use version-specific public download URLs', () => {
  const f = fixture();
  const result = validate(f);
  for (const [platform, entry] of Object.entries(result.platforms)) {
    assert.equal(entry.signature, f.manifest.platforms[platform].signature);
    assert(entry.url.startsWith(`https://github.com/${repository}/releases/download/v${version}/`));
  }
  assert.deepEqual(validateManifest(f.release, result, f.signatures, repository, version), result);
});

test('every architecture needs its installer, updater archive, and signature', () => {
  for (const name of requiredAssets(version)) {
    const f = fixture();
    f.release.assets = f.release.assets.filter(asset => asset.name !== name);
    assert.throws(() => validate(f), /Missing or incomplete/);
  }
  const f = fixture();
  delete f.manifest.platforms['darwin-x86_64'];
  assert.throws(() => validate(f), /Missing platform/);
});

test('rejects stale, substituted, partial, and mismatched release metadata', () => {
  const mutations = [
    f => { f.manifest.version = '0.1.3'; },
    f => { f.release.tag_name = 'main'; },
    f => { f.release.prerelease = true; },
    f => { f.manifest.pub_date = 'invalid'; },
    f => { f.release.assets[0].state = 'starter'; },
    f => { f.release.assets[0].size = 0; },
    f => { f.release.assets.push(f.release.assets[0]); },
    f => { f.manifest.platforms['darwin-aarch64'].signature = 'wrong'; },
    f => { f.manifest.platforms['darwin-aarch64-app'].url = 'https://untrusted.example/build'; },
    f => { f.manifest.platforms.windows = {}; },
    f => { f.release.assets[0].browser_download_url = 'https://untrusted.example/build'; },
  ];
  for (const mutate of mutations) {
    const f = fixture();
    mutate(f);
    assert.throws(() => validate(f));
  }
});
