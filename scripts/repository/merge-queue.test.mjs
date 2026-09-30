import assert from 'node:assert/strict';
import test from 'node:test';
import { ApiError, GitHub, approvalId, authorize, checkState, findApproval, processQueue } from './merge-queue.mjs';

const repository = 'owner/repo';
const head = 'a'.repeat(40);
const base = 'b'.repeat(40);
const updated = 'c'.repeat(40);
const paths = ['.github/workflows/ci.yml', '.github/workflows/branch-policy.yml'];
function fixture() {
  const pr = { number: 12, state: 'open', draft: false, title: 'fix: a reviewed change', mergeable: true,
    labels: [{ name: 'ready-to-merge' }], base: { ref: 'partial', repo: { full_name: repository } },
    head: { sha: head, ref: 'feat/change', repo: { full_name: repository } } };
  const label = { id: 100, actor: { login: 'maintainer' }, created_at: '2026-09-29T12:00:00Z' };
  const marker = { id: 50, name: 'Merge queue', head_sha: head, app: { id: 15368 },
    external_id: approvalId(pr.number, label, head), conclusion: 'neutral' };
  const runs = paths.map((path, i) => ({ id: i + 1, path, event: 'pull_request', head_sha: head,
    head_branch: pr.head.ref, status: 'completed', conclusion: 'success' }));
  const checks = ['Quality gate', 'Production promotion'].map((name, i) => ({ __typename: 'CheckRun', name,
    status: 'COMPLETED', conclusion: 'SUCCESS', startedAt: '2026-09-29T13:00:00Z',
    checkSuite: { app: { databaseId: 15368 }, workflowRun: { databaseId: i + 1 } } }));
  const effects = [];
  const api = {
    repository, canMutate: true,
    pr: async () => structuredClone(pr),
    openPulls: async () => [pr], labelEvent: async () => label,
    permission: async () => 'write', queueChecks: async () => [marker],
    checks: async () => checks, runs: async () => runs,
    partialSha: async () => base, compare: async () => ({ status: 'ahead' }),
    commit: async () => ({ parents: [{ sha: head }, { sha: base }] }),
    updateBranch: async (...args) => effects.push(['update', ...args]),
    approve: async (...args) => effects.push(['approve', ...args]),
    updateCheck: async (...args) => effects.push(['report', ...args]),
    merge: async (...args) => { effects.push(['merge', ...args]); return { merged: true }; },
    pause: async () => {},
  };
  const event = { action: 'labeled', pull_request: structuredClone(pr), sender: { login: 'maintainer' } };
  return { pr, label, marker, checks, runs, effects, api, event };
}
function mutations(f) { return f.effects.filter(effect => ['update', 'merge', 'approve'].includes(effect[0])); }

test('only maintainer opt-in authorizes the current reviewed head', async () => {
  const f = fixture();
  f.api.queueChecks = async () => [];
  assert.equal((await authorize(f.api, f.event)).state, 'queued');
  assert.equal(f.effects[0][0], 'approve');
  assert.equal(f.effects[0][1].head.sha, head);
  f.effects.length = 0;
  f.api.permission = async () => 'triage';
  assert.equal((await authorize(f.api, f.event)).state, 'blocked');
  assert.deepEqual(mutations(f), []);
});

test('a push after the label event cannot receive approval', async () => {
  const f = fixture();
  f.pr.head.sha = updated;
  assert.equal((await authorize(f.api, f.event)).state, 'blocked');
  assert.deepEqual(mutations(f), []);
});

test('superseded label events cannot authorize a newer request', async () => {
  const f = fixture();
  f.label.actor.login = 'someone-else';
  assert.equal((await authorize(f.api, f.event)).state, 'blocked');
  assert.deepEqual(mutations(f), []);
});

test('withdrawal cancels approval, but cannot cancel a reapplied label', async () => {
  const f = fixture();
  f.event.action = 'unlabeled';
  assert.equal((await authorize(f.api, f.event)).state, 'ignored');
  f.pr.labels = [];
  assert.equal((await authorize(f.api, f.event)).state, 'removed');
  assert.equal(f.effects[0][2], 'Removed from the queue.');
  assert.equal(f.effects[0][3], 'cancelled');
});

test('approval must match the GitHub Actions app, label request and exact head', () => {
  const f = fixture();
  assert.equal(findApproval([f.marker], 12, f.label, head), f.marker);
  for (const change of [{ app: { id: 999 } }, { head_sha: updated },
    { external_id: approvalId(12, { ...f.label, id: 101 }, head) }, { conclusion: 'cancelled' }]) {
    assert.equal(findApproval([{ ...f.marker, ...change }], 12, f.label, head), undefined);
  }
});

test('new source code pauses the queue even though the ready label remains', async () => {
  const f = fixture();
  f.pr.head.sha = updated;
  assert.equal((await processQueue(f.api)).state, 'blocked');
  assert.deepEqual(mutations(f), []);
});

test('main, shared branches, fork branches and drafts never merge or update', async () => {
  for (const alter of [p => { p.base.ref = 'main'; }, p => { p.head.ref = 'main'; },
    p => { p.head.ref = 'partial'; }, p => { p.head.repo.full_name = 'other/fork'; },
    p => { p.head.repo = null; }, p => { p.draft = true; }]) {
    const f = fixture();
    alter(f.pr);
    await authorize(f.api, f.event);
    await processQueue(f.api);
    assert.deepEqual(mutations(f), []);
  }
});

test('an empty queue needs no credential and performs no writes', async () => {
  const f = fixture();
  f.api.canMutate = false;
  f.pr.labels = [];
  assert.equal((await processQueue(f.api)).state, 'idle');
  assert.deepEqual(f.effects, []);
});

test('missing mutation credentials pause with a setup instruction', async () => {
  const f = fixture();
  f.api.canMutate = false;
  const outcome = await processQueue(f.api);
  assert.equal(outcome.state, 'blocked');
  assert(outcome.reason.includes('MERGE_QUEUE_TOKEN'));
  assert.deepEqual(mutations(f), []);
});

test('the squash title must follow the repository Conventional Commit convention', async () => {
  const f = fixture();
  f.pr.title = 'random title';
  assert.equal((await processQueue(f.api)).state, 'blocked');
  assert.deepEqual(mutations(f), []);
});

test('an expired or under-scoped credential stops with a setup instruction', async () => {
  const f = fixture();
  f.api.compare = async () => ({ status: 'diverged' });
  f.api.updateBranch = async () => { throw new ApiError(403, 'insufficient permissions'); };
  const outcome = await processQueue(f.api);
  assert.equal(outcome.state, 'blocked');
  assert(outcome.reason.includes('MERGE_QUEUE_TOKEN'));
  assert.deepEqual(mutations(f), []);
});

test('conflicts stop the entire queue before any update', async () => {
  const f = fixture();
  f.pr.mergeable = false;
  f.api.compare = async () => ({ status: 'diverged' });
  assert.equal((await processQueue(f.api)).state, 'blocked');
  assert.deepEqual(mutations(f), []);
});

test('unknown mergeability waits instead of guessing', async () => {
  const f = fixture();
  f.pr.mergeable = null;
  assert.equal((await processQueue(f.api)).state, 'waiting');
  assert.deepEqual(mutations(f), []);
});

test('queue order uses label time, not PR creation time or response order', async () => {
  const f = fixture();
  const second = { ...structuredClone(f.pr), number: 2 };
  f.api.openPulls = async () => [second, f.pr];
  f.api.labelEvent = async number => number === 12 ? f.label : { ...f.label, id: 101, created_at: '2026-09-29T14:00:00Z' };
  assert.equal((await processQueue(f.api)).number, 12);
  assert.equal(mutations(f).length, 1);
  assert.deepEqual(mutations(f)[0], ['merge', 12, head, f.pr.title]);
});

test('a failed first PR blocks later passing PRs', async () => {
  const f = fixture();
  f.runs[0].conclusion = 'failure';
  f.api.openPulls = async () => [f.pr, { ...f.pr, number: 20 }];
  assert.equal((await processQueue(f.api)).state, 'blocked');
  assert.deepEqual(mutations(f), []);
});

test('cancelled, skipped, missing, pending and impostor checks never pass', () => {
  for (const conclusion of ['CANCELLED', 'SKIPPED', 'FAILURE', 'NEUTRAL']) {
    const f = fixture();
    f.checks[0].conclusion = conclusion;
    assert.equal(checkState(f.checks, f.runs, f.pr).state, 'blocked');
  }
  const f = fixture();
  assert.equal(checkState([], f.runs, f.pr).state, 'waiting');
  f.checks[0].status = 'IN_PROGRESS';
  assert.equal(checkState(f.checks, f.runs, f.pr).state, 'waiting');
  f.checks[0].status = 'COMPLETED';
  f.checks[0].checkSuite.app.databaseId = 999;
  assert.equal(checkState(f.checks, f.runs, f.pr).state, 'waiting');
});

test('the latest CI attempt wins over old passing checks on the same SHA', () => {
  const f = fixture();
  const rerun = { ...f.runs[0], id: 3, status: 'in_progress', conclusion: null };
  assert.equal(checkState(f.checks, [...f.runs, rerun], f.pr).state, 'waiting');
  rerun.status = 'completed'; rerun.conclusion = 'failure';
  assert.equal(checkState(f.checks, [...f.runs, rerun], f.pr).state, 'blocked');
  rerun.conclusion = 'success';
  assert.equal(checkState(f.checks, [...f.runs, rerun], f.pr).state, 'waiting');
  // Re-running the same workflow ID also invalidates its earlier passing result.
  f.runs[0].status = 'queued';
  assert.equal(checkState(f.checks, f.runs, f.pr).state, 'waiting');
});

test('checks from workflow_dispatch, another branch or another SHA never qualify', () => {
  for (const change of [{ event: 'workflow_dispatch' }, { head_sha: updated }, { head_branch: 'other' }]) {
    const f = fixture();
    Object.assign(f.runs[0], change);
    assert.equal(checkState(f.checks, f.runs, f.pr).state, 'waiting');
  }
});

test('only a verified base merge inherits approval and must await fresh CI', async () => {
  const f = fixture();
  f.api.compare = async () => ({ status: 'diverged' });
  f.api.updateBranch = async (...args) => { f.effects.push(['update', ...args]); f.pr.head.sha = updated; };
  assert.equal((await processQueue(f.api)).state, 'updated');
  assert.deepEqual(mutations(f).map(effect => effect[0]), ['update', 'approve']);
  assert.deepEqual(mutations(f)[0], ['update', 12, head]);
  assert.equal(mutations(f)[1][1].head.sha, updated);
  // Next cycle cannot reuse checks from the original SHA.
  f.marker.head_sha = updated;
  f.marker.external_id = approvalId(12, f.label, updated);
  f.api.compare = async () => ({ status: 'ahead' });
  f.effects.length = 0;
  assert.equal((await processQueue(f.api)).state, 'waiting');
  assert.deepEqual(mutations(f), []);
});

test('a rejected or ambiguous update cannot transfer approval to new code', async () => {
  for (const scenario of ['race', 'conflict', 'unexpected-parent', 'label-changed', 'slow']) {
    const f = fixture();
    f.api.compare = async () => ({ status: 'diverged' });
    f.api.updateBranch = async () => {
      if (scenario === 'race') throw new ApiError(422, 'head changed');
      if (scenario === 'conflict') throw new ApiError(409, 'conflict');
      if (scenario !== 'slow') f.pr.head.sha = updated;
      if (scenario === 'label-changed') f.label = { ...f.label, id: 101 };
    };
    if (scenario === 'label-changed') f.api.labelEvent = async () => f.label;
    if (scenario === 'unexpected-parent') f.api.commit = async () => ({ parents: [{ sha: 'd'.repeat(40) }, { sha: base }] });
    assert.equal((await processQueue(f.api)).state, 'blocked', scenario);
    assert.deepEqual(mutations(f), [], scenario);
  }
});

test('a source push, removed label, or new label request before merge aborts', async () => {
  for (const scenario of ['push', 'withdraw', 'relabel', 'close', 'retarget']) {
    const f = fixture();
    let reads = 0;
    f.api.pr = async () => {
      if (++reads === 2) {
        if (scenario === 'push') f.pr.head.sha = updated;
        if (scenario === 'withdraw') f.pr.labels = [];
        if (scenario === 'relabel') f.label = { ...f.label, id: 101 };
        if (scenario === 'close') f.pr.state = 'closed';
        if (scenario === 'retarget') f.pr.base.ref = 'main';
      }
      return structuredClone(f.pr);
    };
    f.api.labelEvent = async () => f.label;
    assert.equal((await processQueue(f.api)).state, 'blocked', scenario);
    assert.deepEqual(mutations(f), []);
  }
});

test('partial advancing during a cycle causes a wait, not a stale merge', async () => {
  const f = fixture();
  let reads = 0;
  f.api.partialSha = async () => ++reads === 1 ? base : updated;
  assert.equal((await processQueue(f.api)).state, 'waiting');
  assert.deepEqual(mutations(f), []);
});

test('normal branch protection rejection pauses without retrying or bypassing', async () => {
  for (const status of [403, 405, 409, 422]) {
    const f = fixture();
    f.api.merge = async () => { throw new ApiError(status, 'rules forbid merge'); };
    assert.equal((await processQueue(f.api)).state, 'blocked');
    assert.deepEqual(mutations(f), []);
  }
});

test('transport uses the dedicated token only for SHA-guarded mutation calls', async () => {
  const calls = [];
  const api = new GitHub(repository, 'read-and-check-token', 'mutation-token', 'https://example.test/run', async (url, request) => {
    calls.push({ url, ...request });
    return { ok: true, status: 200, json: async () => ({}) };
  });
  await api.pr(12);
  await api.updateCheck(50, 'Waiting', 'neutral');
  await api.updateBranch(12, head);
  await api.merge(12, head, 'fix: change');
  assert.deepEqual(calls.map(call => call.headers.Authorization),
    ['Bearer read-and-check-token', 'Bearer read-and-check-token', 'Bearer mutation-token', 'Bearer mutation-token']);
  assert.deepEqual(JSON.parse(calls[2].body), { expected_head_sha: head });
  assert.deepEqual(JSON.parse(calls[3].body), { sha: head, merge_method: 'squash', commit_title: 'fix: change (#12)' });
  const missing = new GitHub(repository, 'read', undefined, '', async () => { assert.fail('Must not call mutation API'); });
  await assert.rejects(missing.merge(12, head, 'title'), /MERGE_QUEUE_TOKEN/);
});

test('API transport paginates beyond the first page', async () => {
  const api = new GitHub(repository, 'read', '', '', async url => {
    const page = new URL(url).searchParams.get('page');
    return { ok: true, status: 200, json: async () => page === '1' ? Array.from({ length: 100 }, (_, i) => i) : [100] };
  });
  assert.equal((await api.openPulls()).length, 101);
});
