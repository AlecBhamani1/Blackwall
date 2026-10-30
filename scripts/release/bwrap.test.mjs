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

test('the Linux packages install bundled bubblewrap at /usr/lib/blackwall/bwrap', () => {
  const { deb, appimage } = linuxConfig().bundle.linux;
  assert.deepEqual(appimage.files, bundled);
  assert.deepEqual(deb.files, bundled);
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
