import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { branchRules, labels, tagRules, queueEventPolicy } from './config.mjs';

const args = process.argv.slice(2);
assert(args.every(arg => arg === '--apply' || /^--reviews=[0-6]$/.test(arg)), 'Use --apply and optionally --reviews=1');
const apply = args.includes('--apply');
const reviews = Number(args.find(arg => arg.startsWith('--reviews='))?.split('=')[1] ?? 0);
const gh = args => execFileSync('gh', args, { encoding: 'utf8' });
const repo = JSON.parse(gh(['repo', 'view', '--json', 'nameWithOwner,viewerPermission']));
assert.equal(repo.viewerPermission, 'ADMIN', 'Repository administration permission is required');
const prefix = `repos/${repo.nameWithOwner}`;
function api(path, method = 'GET', body) {
  if (method !== 'GET' && !apply) {
    console.log(`${method} ${path}\n${JSON.stringify(body, null, 2)}`);
    return;
  }
  return JSON.parse(execFileSync('gh', ['api', path ? `${prefix}/${path}` : prefix, '--method', method,
    ...(body ? ['--input', '-'] : [])], { encoding: 'utf8', input: body ? JSON.stringify(body) : undefined }) || 'null');
}

// Read the complete current state before changing repository settings.
const branches = JSON.parse(gh(['api', `${prefix}/branches?per_page=100`, '--paginate', '--slurp'])).flat();
const rulesets = JSON.parse(gh(['api', `${prefix}/rulesets?per_page=100`, '--paginate', '--slurp'])).flat();
const existingLabels = JSON.parse(gh(['api', `${prefix}/labels?per_page=100`, '--paginate', '--slurp'])).flat();
const actionPolicies = JSON.parse(gh(['api', `${prefix}/actions/policies?per_page=100&has_parents=false`,
  '--paginate', '--slurp'])).flatMap(page => page.policies);
if (!branches.some(branch => branch.name === 'partial')) {
  const main = api('git/ref/heads/main');
  api('git/refs', 'POST', { ref: 'refs/heads/partial', sha: main.object.sha });
}

for (const branch of ['partial', 'main']) {
  const rules = branchRules(branch, reviews);
  const existing = rulesets.find(rule => rule.name === rules.name);
  api(existing ? `rulesets/${existing.id}` : 'rulesets', existing ? 'PUT' : 'POST', rules);
}
const tags = tagRules();
const existingTags = rulesets.find(rule => rule.name === tags.name);
api(existingTags ? `rulesets/${existingTags.id}` : 'rulesets', existingTags ? 'PUT' : 'POST', tags);

const queueEvents = queueEventPolicy();
const existingQueueEvents = actionPolicies.find(policy => policy.name === queueEvents.name);
api(existingQueueEvents ? `actions/policies/${existingQueueEvents.id}` : 'actions/policies',
  existingQueueEvents ? 'PUT' : 'POST', queueEvents);

api('', 'PATCH', {
  default_branch: 'partial', allow_auto_merge: true, allow_update_branch: true,
  allow_merge_commit: true, allow_squash_merge: true, allow_rebase_merge: false,
  delete_branch_on_merge: false,
  squash_merge_commit_title: 'PR_TITLE', squash_merge_commit_message: 'PR_BODY',
  merge_commit_title: 'PR_TITLE', merge_commit_message: 'PR_BODY',
  security_and_analysis: {
    secret_scanning: { status: 'enabled' },
    secret_scanning_push_protection: { status: 'enabled' },
  },
});
api('vulnerability-alerts', 'PUT');
api('automated-security-fixes', 'PUT');
api('private-vulnerability-reporting', 'PUT', {});

for (const [name, color, description] of labels) {
  const existing = existingLabels.some(label => label.name === name);
  api(existing ? `labels/${encodeURIComponent(name)}` : 'labels', existing ? 'PATCH' : 'POST', { name, color, description });
}

console.log(apply ? 'Applied repository policy. Existing unrelated rulesets were preserved.' : 'Dry run complete. Pass --apply to configure GitHub.');
console.log(`Required approvals: ${reviews}. Raise this to 1 when another reviewer is available.`);
