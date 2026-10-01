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

// Beta versions use the next patch above the committed production plan. The
// publication workflow run number orders snapshots and remains fixed on retries.
export function betaVersion(base, runNumber) {
  const [major, minor, patch] = versionParts(base);
  assert.match(String(runNumber ?? ''), /^[1-9]\d*$/, 'Missing beta workflow run number');
  return `${major}.${minor}.${patch + 1n}-beta.${runNumber}`;
}

export function releaseVersionParts(version) {
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-beta\.([1-9]\d*))?$/.exec(version ?? '');
  assert(match, 'Use a stable version or a numbered beta version');
  return [...match.slice(1, 4).map(BigInt), match[4] ? BigInt(match[4]) : null];
}

export function compareReleaseVersions(left, right) {
  const a = releaseVersionParts(left);
  const b = releaseVersionParts(right);
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] > b[i] ? 1 : -1;
  }
  if (a[3] === b[3]) return 0;
  if (a[3] === null) return 1;
  if (b[3] === null) return -1;
  return a[3] > b[3] ? 1 : -1;
}

export function requireSuccessfulCI(runs, sha, branch = 'main') {
  const latest = runs
    .filter(run => run.head_sha === sha && run.event === 'push' && run.head_branch === branch)
    .sort((a, b) => b.run_number - a.run_number || b.run_attempt - a.run_attempt)[0];
  assert(latest?.status === 'completed' && latest.conclusion === 'success',
    `The latest ${branch}-branch CI run for this exact commit must finish successfully first`);
}

// Each updater platform, the signed bundle it installs, and installers offered as direct downloads.
// Names follow the Tauri CLI output that tauri-action uploads; macOS archives gain version and arch.
export function desktopPlatforms(version) {
  const file = suffix => `Blackwall_${version}_${suffix}`;
  return [
    { platform: 'darwin-aarch64', bundle: 'app', updater: file('aarch64.app.tar.gz'), installers: [file('aarch64.dmg')] },
    { platform: 'darwin-x86_64', bundle: 'app', updater: file('x64.app.tar.gz'), installers: [file('x64.dmg')] },
    { platform: 'linux-x86_64', bundle: 'appimage', updater: file('amd64.AppImage'), installers: [file('amd64.deb')] },
    { platform: 'linux-aarch64', bundle: 'appimage', updater: file('aarch64.AppImage'), installers: [file('arm64.deb')] },
    { platform: 'windows-x86_64', bundle: 'nsis', updater: file('x64-setup.exe'), installers: [] },
  ];
}

export function requiredAssets(version) {
  releaseVersionParts(version);
  return desktopPlatforms(version).flatMap(({ installers, updater }) => [
    ...installers,
    updater,
    `${updater}.sig`,
  ]);
}

// Convert Tauri's API asset URLs into stable, version-specific public downloads.
// Every platform entry and its signature must match this release's files.
export function validateManifest(release, manifest, signatures, repository, version, channel = 'stable') {
  assert(['stable', 'beta'].includes(channel));
  if (channel === 'stable') versionParts(version);
  else {
    assert(releaseVersionParts(version)[3] !== null, 'Beta releases require a beta version');
  }
  assert.equal(release.tag_name, `v${version}`);
  assert.equal(release.prerelease, channel === 'beta');
  assert.equal(manifest.version, version);
  assert(Number.isFinite(Date.parse(manifest.pub_date)), 'Missing updater publication date');
  assert.equal(typeof manifest.notes, 'string');
  let downloadTag = `v${version}`;
  if (release.draft && release.html_url) {
    const prefix = `https://github.com/${repository}/releases/tag/`;
    assert(release.html_url.startsWith(prefix), 'Unexpected draft release URL');
    const draftTag = release.html_url.slice(prefix.length);
    assert(draftTag === downloadTag || /^untagged-[a-f0-9]+$/.test(draftTag), 'Unexpected draft tag');
    downloadTag = draftTag;
  }
  const assets = new Map();
  for (const asset of release.assets) {
    assert(!assets.has(asset.name), `Duplicate asset: ${asset.name}`);
    assets.set(asset.name, asset);
  }
  for (const name of requiredAssets(version)) {
    const asset = assets.get(name);
    assert(asset?.state === 'uploaded' && asset.size > 0, `Missing or incomplete asset: ${name}`);
    assert.equal(asset.browser_download_url,
      `https://github.com/${repository}/releases/download/${downloadTag}/${name}`);
  }
  const platforms = {};
  const signed = (key, name) => {
    const asset = assets.get(name);
    assert(asset?.state === 'uploaded' && asset.size > 0, `Missing or incomplete asset: ${name}`);
    const signature = signatures[name + '.sig']?.trim();
    assert(signature && signature.length > 0, `Missing signature: ${name}`);
    const candidate = manifest.platforms[key];
    const publicUrl = `https://github.com/${repository}/releases/download/v${version}/${name}`;
    assert.equal(candidate.signature?.trim(), signature, `Signature mismatch: ${key}`);
    assert([asset.url, asset.browser_download_url, publicUrl].includes(candidate.url), `Wrong release asset: ${key}`);
    platforms[key] = { signature, url: publicUrl };
  };
  for (const { platform, bundle, updater, installers } of desktopPlatforms(version)) {
    assert(manifest.platforms?.[platform], `Missing platform: ${platform}`);
    // The primary entry and its installer-specific alias both install the updater bundle.
    signed(platform, updater);
    if (manifest.platforms[`${platform}-${bundle}`]) signed(`${platform}-${bundle}`, updater);
    // Newer Tauri builds also sign Debian packages so .deb installations update in place.
    const deb = installers.find(name => name.endsWith('.deb'));
    if (deb && manifest.platforms[`${platform}-deb`]) signed(`${platform}-deb`, deb);
  }
  assert(Object.keys(manifest.platforms).every(key => key in platforms), 'Unexpected updater platform');
  return { ...manifest, platforms };
}
