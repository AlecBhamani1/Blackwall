import assert from 'node:assert/strict';

export function versionParts(version) {
  assert.match(version, /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/, 'Use a stable version such as 0.1.4, without v');
  return version.split('.').map(BigInt);
}

export function compareVersions(left, right) {
  const a = versionParts(left);
  const b = versionParts(right);
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] > b[i] ? 1 : -1;
  }
  return 0;
}

export function requireSuccessfulCI(runs, sha) {
  const latest = runs
    .filter(run => run.head_sha === sha && run.event === 'push' && run.head_branch === 'main')
    .sort((a, b) => b.run_number - a.run_number || b.run_attempt - a.run_attempt)[0];
  assert(latest?.status === 'completed' && latest.conclusion === 'success',
    'The latest main-branch CI run for this exact commit must finish successfully first');
}

export function requiredAssets(version) {
  versionParts(version);
  return ['aarch64', 'x64'].flatMap(arch => [
    `Blackwall_${version}_${arch}.dmg`,
    `Blackwall_${version}_${arch}.app.tar.gz`,
    `Blackwall_${version}_${arch}.app.tar.gz.sig`,
  ]);
}

// Convert Tauri's API asset URLs into stable, version-specific public downloads.
// Both architecture entries and their signatures must match this release's files.
export function validateManifest(release, manifest, signatures, repository, version) {
  versionParts(version);
  assert.equal(release.tag_name, `v${version}`);
  assert.equal(release.prerelease, false);
  assert.equal(manifest.version, version);
  assert(Number.isFinite(Date.parse(manifest.pub_date)), 'Missing updater publication date');
  assert.equal(typeof manifest.notes, 'string');
  const assets = new Map();
  for (const asset of release.assets) {
    assert(!assets.has(asset.name), `Duplicate asset: ${asset.name}`);
    assets.set(asset.name, asset);
  }
  for (const name of requiredAssets(version)) {
    const asset = assets.get(name);
    assert(asset?.state === 'uploaded' && asset.size > 0, `Missing or incomplete asset: ${name}`);
    assert.equal(asset.browser_download_url,
      `https://github.com/${repository}/releases/download/v${version}/${name}`);
  }
  const platforms = {};
  for (const [platform, arch] of [['darwin-aarch64', 'aarch64'], ['darwin-x86_64', 'x64']]) {
    const name = `Blackwall_${version}_${arch}.app.tar.gz`;
    const asset = assets.get(name);
    const entry = manifest.platforms?.[platform];
    assert(entry, `Missing platform: ${platform}`);
    const signature = signatures[name + '.sig']?.trim();
    assert(signature && signature.length > 0, `Missing signature: ${name}`);
    for (const key of [platform, `${platform}-app`]) {
      const candidate = manifest.platforms[key];
      if (!candidate && key.endsWith('-app')) continue;
      assert.equal(candidate.signature?.trim(), signature, `Signature mismatch: ${key}`);
      assert([asset.url, asset.browser_download_url].includes(candidate.url), `Wrong release asset: ${key}`);
      platforms[key] = { signature, url: asset.browser_download_url };
    }
  }
  assert(Object.keys(manifest.platforms).every(key => key in platforms), 'Unexpected updater platform');
  return { ...manifest, platforms };
}
