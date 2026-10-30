import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';

const root = new URL('../../', import.meta.url);
const read = path => fs.readFileSync(new URL(path, root), 'utf8');
const linuxConfig = () => JSON.parse(read('src/app/tauri.linux.conf.json'));
const bundled = {
  '/usr/lib/blackwall/bwrap': 'resources/linux/bwrap',
  '/usr/share/doc/blackwall/bubblewrap/COPYING': 'resources/linux/COPYING',
};
const profileSource = 'linux/apparmor/blackwall-bwrap';

test('the Linux packages install bundled bubblewrap at /usr/lib/blackwall/bwrap', () => {
  const { deb, appimage } = linuxConfig().bundle.linux;
  assert.deepEqual(appimage.files, bundled);
  assert.deepEqual(deb.files, { ...bundled, '/usr/share/blackwall/apparmor/blackwall-bwrap': profileSource });
  assert.deepEqual(deb.depends, ['libcap2']);
});

test('the bubblewrap build pins the 0.13.1 release, its SHA-256 and a non-SELinux, non-setuid build', () => {
  const script = read('scripts/build-bwrap.sh');
  assert.match(script, /https:\/\/github\.com\/containers\/bubblewrap\/releases\/download\/v0\.13\.1\/bubblewrap-0\.13\.1\.tar\.xz/);
  assert.match(script, /^BWRAP_SHA256='[0-9a-f]{64}'$/m);
  assert.match(script, /sha256sum --check/);
  assert.match(script, /-Dselinux=disabled/);
  assert.match(script, /test -u/);
  assert.match(script, /-Db_pie=true/);
  assert.match(script, /-D_FORTIFY_SOURCE=3 -fstack-protector-strong -fstack-clash-protection/);
  assert.match(script, /-Wl,-z,relro,-z,now/);
});

/** The workflow's steps, each from its `- name:` or `- uses:` line to the next. */
const steps = text => text.split(/\n(?=\s*- (?:name|uses):)/);

test('each release builds bubblewrap without a token, then publishes its verified source', () => {
  for (const workflow of ['release-beta.yml', 'release-main.yml']) {
    const all = steps(read(`.github/workflows/${workflow}`));
    const build = all.findIndex(step => /^\s*- name: Build bubblewrap$/m.test(step));
    const attach = all.findIndex(step => /^\s*- name: Attach bubblewrap source$/m.test(step));
    assert.ok(build >= 0 && attach > build, workflow);
    assert.match(all[build], /run: \.\/scripts\/build-bwrap\.sh src\/app\/resources\/linux$/m);
    // The build runs downloaded source code, so it gets no token.
    const code = step => step.split('\n').filter(line => !line.trim().startsWith('#')).join('\n');
    assert.doesNotMatch(code(all[build]), /^\s*env:|GH_TOKEN|github\.token|secrets\./m);
    assert.match(all[attach], /GH_TOKEN: \$\{\{ github\.token \}\}/);
    assert.match(all[attach], /gh release upload "v\$RELEASE_VERSION" src\/app\/resources\/linux\/bubblewrap-0\.13\.1\.tar\.xz --clobber/);
    assert.doesNotMatch(all[attach], /build-bwrap/);
    for (const step of [all[build], all[attach]]) assert.match(step, /if: runner\.os == 'Linux'/);
  }
  assert.match(read('scripts/build-bwrap.sh'), /install -m 0644 "\$work\/bubblewrap\.tar\.xz" "\$out_dir\/bubblewrap-0\.13\.1\.tar\.xz"/);
  assert.match(read('NOTICE'), /source tarball \(bubblewrap-0\.13\.1\.tar\.xz, SHA-256 checked\) is published with each Blackwall release/);
});

test('CI runs the real-bubblewrap tests on x86-64 and arm64, and the quality gate needs both', () => {
  const ci = read('.github/workflows/ci.yml');
  const job = ci.slice(ci.indexOf('\n  linux-sandbox:'), ci.indexOf('\n  relay-linux:'));
  assert.match(job, /runs-on: \$\{\{ matrix\.os \}\}/);
  assert.match(job, /os: \[ubuntu-24\.04, ubuntu-24\.04-arm\]/);
  assert.match(job, /fail-fast: false/);
  // `needs.linux-sandbox.result` is success only when every matrix leg succeeded.
  const gate = ci.slice(ci.indexOf('\n  quality-gate:'));
  assert.match(gate, /needs: \[[^\]]*\blinux-sandbox\b[^\]]*\]/);
  assert.match(gate, /const required = \[[^\]]*'linux-sandbox'[^\]]*\]/);
});

test('NOTICE credits bubblewrap and links its source', () => {
  const notice = read('NOTICE');
  assert.match(notice, /bubblewrap 0\.13\.1 \(LGPL-2\.0-or-later\)/);
  assert.match(notice, /https:\/\/github\.com\/containers\/bubblewrap\/tree\/v0\.13\.1/);
});

test('the AppArmor profile grants user namespaces only to the bundled bwrap and is never unconfined', () => {
  const profile = read(`src/app/${profileSource}`);
  const rules = profile.split('\n').filter(line => !line.trim().startsWith('#')).join('\n');
  const attached = [...rules.matchAll(/^profile\s+(\S+)\s+(\/\S*)/gm)].map(m => m[2]);
  assert.deepEqual(attached, ['/usr/lib/blackwall/bwrap']);
  assert.deepEqual([...rules.matchAll(/\/usr\/[^\s,]*bwrap\b/g)].map(m => m[0]), ['/usr/lib/blackwall/bwrap']);
  assert.match(rules, /^\s*userns,$/m);
  assert.doesNotMatch(profile, /unconfined/);
  // Sandboxed programs run without capabilities and cannot create further user namespaces.
  const start = rules.indexOf('profile blackwall-bwrap-child');
  const bwrap = rules.slice(0, start);
  const child = rules.slice(start);
  assert.match(child, /^\s*deny capability,$/m);
  assert.doesNotMatch(child, /userns|mount|pivot_root/);
  // Programs bwrap runs must enter the capability-free stack; a missing child profile fails the exec.
  const execRules = text => [...text.matchAll(/^\s*(\S+)\s+\S+\s+->\s+(\S+),$/gm)].map(m => [m[1], m[2]]);
  // px to the named stack: AppArmor 4.0 resolves a leading-'&' target only through an ix fallback.
  assert.deepEqual(execRules(bwrap), [['px', 'blackwall-bwrap//&blackwall-bwrap-child']]);
  assert.deepEqual(execRules(child), [['px', 'blackwall-bwrap//&blackwall-bwrap-child']]);
  // Apart from those two rules, no exec permission in any form or case: ix, ux, pix, cux, PUx,
  // CUx and the like, a mode string that includes one (rix, mrPx), or a profile change.
  const pinned = /^\s*px \/\*\* -> blackwall-bwrap\/\/&blackwall-bwrap-child,$/gm;
  for (const profile of [bwrap, child]) {
    const rest = profile.replace(pinned, '');
    assert.doesNotMatch(rest, /\b[pc]?[iu]x\b/i);
    assert.doesNotMatch(rest, /\bix\b|\b[PC]Ux\b/);
    assert.doesNotMatch(rest, /(?:^|[\s(])[rwalkmpciux]*x[rwalkmpciux]*(?=[\s,)]|$)/im);
    assert.doesNotMatch(rest, /change_profile/);
  }
});

test('the .deb installs and removes the AppArmor profile with its maintainer scripts', () => {
  const { deb } = linuxConfig().bundle.linux;
  assert.equal(deb.postInstallScript, 'linux/deb/postinst.sh');
  assert.equal(deb.preRemoveScript, 'linux/deb/prerm.sh');
  const postinst = read(`src/app/${deb.postInstallScript}`);
  const prerm = read(`src/app/${deb.preRemoveScript}`);
  assert.match(postinst, /apparmor_parser -r/);
  assert.match(postinst, /\/usr\/share\/blackwall\/apparmor\/blackwall-bwrap/);
  assert.match(prerm, /apparmor_parser -R/);
  assert.match(prerm, /rm -f \/etc\/apparmor\.d\/blackwall-bwrap/);
});
