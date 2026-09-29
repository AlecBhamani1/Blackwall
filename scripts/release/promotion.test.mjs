import assert from 'node:assert/strict';
import test from 'node:test';
import { releaseChecklist, validatePromotion, validateReleaseNotes } from './promotion.mjs';

function fixture() {
  const repository = { full_name: 'example/blackwall' };
  const milestone = { title: 'v0.1.4', html_url: 'https://github.com/example/blackwall/milestone/1' };
  return {
    event: { pull_request: { base: { ref: 'main', repo: repository }, head: { ref: 'partial', repo: repository },
      milestone, body: releaseChecklist.map(item => `- [x] ${item}`).join('\n') } },
    notes: `# Blackwall 0.1.4\n\nRelease batch: ${milestone.html_url}\n\n## Changes\n\n- Improve releases.\n\n## Downloads\n\nUse the macOS installers.\n\n## Acceptance status\n\nAutomated checks passed; physical-device acceptance remains open.\n`,
  };
}
const validate = f => validatePromotion(f.event, '0.1.4', '0.1.3', f.notes);

test('a same-repository partial promotion needs a new version, milestone, complete notes, and explicit approval', () => {
  assert.doesNotThrow(() => validate(fixture()));
  for (const mutate of [
    f => { f.event.pull_request.head.ref = 'feature/bypass'; },
    f => { f.event.pull_request.head.repo = { full_name: 'fork/blackwall' }; },
    f => { f.event.pull_request.body = f.event.pull_request.body.replace('[x]', '[ ]'); },
    f => { f.event.pull_request.body = `<!-- ${f.event.pull_request.body} -->`; },
    f => { f.event.pull_request.body = '```\n' + f.event.pull_request.body + '\n```'; },
    f => { f.event.pull_request.milestone = null; },
    f => { f.event.pull_request.milestone.title = 'v0.1.5'; },
    f => { f.notes = f.notes.replace('milestone/1', 'milestone/2'); },
    f => { f.notes += '\nTODO: acceptance\n'; },
    f => { f.notes = f.notes.replace('## Acceptance status', '## Other'); },
  ]) {
    const f = fixture();
    mutate(f);
    assert.throws(() => validate(f));
  }
  const f = fixture();
  for (const previous of ['0.1.4', '0.1.5', 'v0.1.3']) {
    assert.throws(() => validatePromotion(f.event, '0.1.4', previous, f.notes));
  }
});

test('release notes retain the established format and reject version mismatches', () => {
  assert.doesNotThrow(() => validateReleaseNotes(fixture().notes, '0.1.4'));
  assert.throws(() => validateReleaseNotes(fixture().notes, '0.1.5'));
});
