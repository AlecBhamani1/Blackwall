import assert from 'node:assert/strict';
import test from 'node:test';
import { branchRules, tagRules } from './config.mjs';

test('both long-lived branches reject deletion, force pushes, direct pushes, and failing checks without admin bypass', () => {
  for (const branch of ['main', 'partial']) {
    const policy = branchRules(branch);
    assert.equal(policy.enforcement, 'active');
    assert.deepEqual(policy.bypass_actors, []);
    assert.deepEqual(policy.conditions.ref_name.include, [`refs/heads/${branch}`]);
    for (const type of ['deletion', 'non_fast_forward', 'pull_request', 'required_status_checks']) {
      assert(policy.rules.some(rule => rule.type === type));
    }
    const checks = policy.rules.find(rule => rule.type === 'required_status_checks').parameters;
    assert.equal(checks.strict_required_status_checks_policy, true);
    assert(checks.required_status_checks.some(check => check.context === 'Quality gate'));
    assert(checks.required_status_checks.every(check => check.integration_id === 15368));
  }
});

test('production preserves merge ancestry and review requirements can grow with the team', () => {
  const pr = branchRules('main', 1).rules.find(rule => rule.type === 'pull_request').parameters;
  assert.deepEqual(pr.allowed_merge_methods, ['merge']);
  assert.equal(pr.required_approving_review_count, 1);
  assert.equal(pr.require_last_push_approval, true);
  assert.equal(pr.dismiss_stale_reviews_on_push, true);
  assert.equal(pr.required_review_thread_resolution, true);
});

test('version tags may be created but never moved or deleted', () => {
  const policy = tagRules();
  assert.equal(policy.target, 'tag');
  assert.deepEqual(policy.bypass_actors, []);
  assert.deepEqual(policy.conditions.ref_name.include, ['refs/tags/v*']);
  assert.deepEqual(policy.rules.map(rule => rule.type).sort(), ['deletion', 'update']);
});
