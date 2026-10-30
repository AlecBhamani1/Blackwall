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
});

test('the bubblewrap build pins the 0.13.1 release, its SHA-256 and a non-SELinux, non-setuid build', () => {
  const script = read('scripts/build-bwrap.sh');
  assert.match(script, /https:\/\/github\.com\/containers\/bubblewrap\/releases\/download\/v0\.13\.1\/bubblewrap-0\.13\.1\.tar\.xz/);
  assert.match(script, /^BWRAP_SHA256='[0-9a-f]{64}'$/m);
  assert.match(script, /sha256sum --check/);
  assert.match(script, /-Dselinux=disabled/);
  assert.match(script, /test -u/);
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
  const child = rules.slice(rules.indexOf('profile blackwall-bwrap-child'));
  assert.match(child, /^\s*deny capability,$/m);
  assert.doesNotMatch(child, /userns|mount|pivot_root/);
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
