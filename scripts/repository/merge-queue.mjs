import assert from 'node:assert/strict';
import { appendFileSync, readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

export const queueLabel = 'ready-to-merge';
export const queueCheck = 'Merge queue';
const actionsApp = 15368;
const required = [
  ['Quality gate', '.github/workflows/ci.yml'],
  ['Production promotion', '.github/workflows/branch-policy.yml'],
];

export class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

export function eligible(pr, repository) {
  return pr.state === 'open' && pr.base.ref === 'partial'
    && pr.base.repo.full_name === repository && pr.head.repo?.full_name === repository
    && !['partial', 'main'].includes(pr.head.ref) && !pr.draft;
}

export function approvalId(number, event, sha) {
  return `blackwall-queue:${number}:${event.id}:${sha}`;
}

export function findApproval(checks, number, event, sha) {
  return checks.filter(check => check.name === queueCheck && check.app?.id === actionsApp
    && check.head_sha === sha && check.external_id === approvalId(number, event, sha)
    && check.conclusion !== 'cancelled').sort((a, b) => b.id - a.id)[0];
}

export function checkState(checks, runs, pr) {
  let waiting = false;
  for (const [name, path] of required) {
    const run = runs.filter(run => run.path === path && run.event === 'pull_request'
      && run.head_sha === pr.head.sha && run.head_branch === pr.head.ref)
      .sort((a, b) => b.id - a.id)[0];
    if (!run || run.status !== 'completed') { waiting = true; continue; }
    if (run.conclusion !== 'success') return { state: 'blocked', reason: `${name}: ${run.conclusion}` };
    const check = checks.filter(check => check.__typename === 'CheckRun' && check.name === name
      && check.checkSuite?.app?.databaseId === actionsApp
      && check.checkSuite?.workflowRun?.databaseId === run.id)
      .sort((a, b) => (b.startedAt ?? '').localeCompare(a.startedAt ?? ''))[0];
    if (!check || check.status !== 'COMPLETED') { waiting = true; continue; }
    if (check.conclusion !== 'SUCCESS') return { state: 'blocked', reason: `${name}: ${check.conclusion}` };
  }
  return waiting ? { state: 'waiting', reason: 'Waiting for the latest required PR checks.' }
    : { state: 'passed', reason: 'The latest required PR checks passed.' };
}

function hasLabel(pr) { return pr.labels.some(label => label.name === queueLabel); }
function validTitle(title) {
  return /^(?:build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test)(?:\([^()\r\n]+\))?!?: [^\r\n]+$/.test(title);
}

// Only a maintainer label event can create approval. A later source push has no
// approval on its new SHA; the worker never infers approval from a label alone.
export async function authorize(api, event) {
  const number = event.pull_request.number;
  const pr = await api.pr(number);
  if (event.action === 'unlabeled') {
    if (hasLabel(pr)) return { state: 'ignored', reason: 'The label was reapplied.' };
    for (const check of await api.queueChecks(pr.head.sha)) {
      if (check.name === queueCheck && check.app?.id === actionsApp
        && check.external_id?.startsWith(`blackwall-queue:${number}:`)) {
        await api.updateCheck(check.id, 'Removed from the queue.', 'cancelled');
      }
    }
    return { state: 'removed', number, reason: 'Removed from the queue.' };
  }
  if (!eligible(pr, api.repository) || !hasLabel(pr)) {
    return { state: 'ignored', reason: 'Only ready, same-repository feature PRs into partial are supported.' };
  }
  if (pr.head.sha !== event.pull_request.head.sha) {
    return { state: 'blocked', number, reason: 'The head changed after labeling; review and reapply the label.' };
  }
  const label = await api.labelEvent(number);
  if (!label || label.actor.login !== event.sender.login) {
    return { state: 'blocked', number, reason: 'A newer label event superseded this request.' };
  }
  const permission = await api.permission(event.sender.login);
  if (!['admin', 'maintain', 'write'].includes(permission)) {
    return { state: 'blocked', number, reason: 'A maintainer with write access must apply the label.' };
  }
  const existing = findApproval(await api.queueChecks(pr.head.sha), number, label, pr.head.sha);
  if (!existing) await api.approve(pr, label, 'Reviewed head recorded. Waiting for its turn in the queue.');
  return { state: 'queued', number, reason: 'Reviewed head recorded.' };
}

export async function processQueue(api) {
  const candidates = [];
  for (const pr of await api.openPulls()) {
    if (pr.base.ref === 'partial' && hasLabel(pr)) {
      candidates.push({ number: pr.number, label: await api.labelEvent(pr.number) });
    }
  }
  candidates.sort((a, b) => (a.label?.created_at ?? '').localeCompare(b.label?.created_at ?? '')
    || a.number - b.number);
  if (!candidates.length) return { state: 'idle', reason: 'No pull requests are marked ready-to-merge.' };
  const { number, label } = candidates[0];
  const result = (state, reason) => ({ state, number, reason });
  let pr = await api.pr(number);
  if (!hasLabel(pr) || pr.state !== 'open') return result('waiting', 'Queue changed; the next event will refresh it.');
  if (!eligible(pr, api.repository)) return result('blocked', 'The first PR must be a non-draft, same-repository feature PR into partial.');
  if (!label) return result('blocked', 'Label history is unavailable; remove and reapply ready-to-merge.');
  const approval = findApproval(await api.queueChecks(pr.head.sha), number, label, pr.head.sha);
  if (!approval) return result('blocked', 'This head is not reviewed. Review the latest code, then remove and reapply ready-to-merge.');
  const report = async (state, reason) => {
    await api.updateCheck(approval.id, reason, state === 'blocked' ? 'action_required' : 'neutral');
    return result(state, reason);
  };
  if (!validTitle(pr.title)) return report('blocked', 'Use a Conventional Commit PR title, such as fix: describe the change.');
  if (!api.canMutate) return report('blocked', 'Add the repository-scoped MERGE_QUEUE_TOKEN Actions secret to enable updates and merges.');
  const checks = await api.checks(number);
  const runs = await api.runs(pr.head.sha);
  const readiness = checkState(checks, runs, pr);
  if (readiness.state === 'blocked') return report('blocked', `${readiness.reason}. Fix or rerun the checks, or remove the label to unblock the queue.`);
  if (pr.mergeable === null) return report('waiting', 'GitHub is calculating mergeability.');
  if (pr.mergeable === false) return report('blocked', 'Resolve the merge conflict, review the new head, then reapply ready-to-merge.');
  const base = await api.partialSha();
  const comparison = await api.compare(base, pr.head.sha);
  if (comparison.status === 'behind' || comparison.status === 'diverged') {
    // expected_head_sha makes a racing source push reject this update. Approval
    // moves only to the exact two-parent base merge produced by this request.
    try { await api.updateBranch(number, pr.head.sha); }
    catch (error) {
      if ([401, 403].includes(error.status)) return report('blocked', 'The automation credential could not update the branch. Check MERGE_QUEUE_TOKEN permissions and expiry.');
      if ([405, 409, 422].includes(error.status)) return report('blocked', 'Branch update was rejected. Resolve conflicts or review a changed head before requeueing.');
      throw error;
    }
    const previous = pr.head.sha;
    for (let attempt = 0; attempt < 10; attempt++) {
      pr = await api.pr(number);
      if (pr.head.sha !== previous) break;
      await api.pause();
    }
    if (pr.head.sha === previous) return report('blocked', 'Branch update is not visible yet. Review the updated head and reapply the label once it completes.');
    const commit = await api.commit(pr.head.sha);
    if (!eligible(pr, api.repository) || !hasLabel(pr) || (await api.labelEvent(number))?.id !== label.id
      || commit.parents.length !== 2 || commit.parents[0].sha !== previous || commit.parents[1].sha !== base) {
      return report('blocked', 'The head or queue request changed during the update. Review the new head and reapply the label.');
    }
    await api.approve(pr, label, 'Merged the latest partial into the reviewed branch. Waiting for fresh CI.');
    return result('updated', 'Updated from partial. Fresh PR CI must pass before merging.');
  }
  if (!['ahead', 'identical'].includes(comparison.status)) return report('blocked', 'Could not establish that partial is an ancestor of the reviewed head.');
  if (readiness.state !== 'passed') return report('waiting', readiness.reason);
  // Re-read the opt-in and head immediately before the SHA-guarded merge.
  const fresh = await api.pr(number);
  if (!eligible(fresh, api.repository) || !hasLabel(fresh) || fresh.head.sha !== pr.head.sha
    || (await api.labelEvent(number))?.id !== label.id) {
    return report('blocked', 'The PR or queue request changed before merging; review and requeue it.');
  }
  if (await api.partialSha() !== base) return report('waiting', 'partial advanced; the next cycle will update the branch first.');
  if (!validTitle(fresh.title)) return report('blocked', 'Use a Conventional Commit PR title before merging.');
  try {
    const merge = await api.merge(number, pr.head.sha, fresh.title);
    if (!merge.merged) return report('blocked', 'GitHub did not merge the PR. Inspect branch protections and review requirements.');
  } catch (error) {
    if ([403, 405, 409, 422].includes(error.status)) return report('blocked', 'GitHub rejected the merge. Inspect reviews, conversations, protections, or a racing push; the queue never bypasses them.');
    throw error;
  }
  await api.updateCheck(approval.id, 'Merged into partial after the latest required checks passed.', 'success');
  return result('merged', 'Merged into partial. The next cycle will process the next PR.');
}

export class GitHub {
  constructor(repository, token, mutationToken, runUrl, request = fetch) {
    assert(/^[\w.-]+\/[\w.-]+$/.test(repository), 'Set GITHUB_REPOSITORY');
    assert(token, 'Set GH_TOKEN');
    this.repository = repository;
    this.token = token;
    this.mutationToken = mutationToken;
    this.canMutate = Boolean(mutationToken);
    this.runUrl = runUrl;
    this.request = request;
  }
  async call(path, method = 'GET', body, mutate = false) {
    const token = mutate ? this.mutationToken : this.token;
    assert(token, 'MERGE_QUEUE_TOKEN is required for branch updates and merges');
    const response = await this.request(`https://api.github.com${path}`, {
      method, headers: { Authorization: `Bearer ${token}`, Accept: 'application/vnd.github+json',
        'X-GitHub-Api-Version': '2022-11-28', 'Content-Type': 'application/json' },
      ...(body ? { body: JSON.stringify(body) } : {}), signal: AbortSignal.timeout(30_000),
    });
    const data = response.status === 204 ? {} : await response.json();
    if (!response.ok) throw new ApiError(response.status, `GitHub request failed (${response.status}): ${data.message ?? 'unknown error'}`);
    if (data.errors) throw new Error('GitHub GraphQL could not load required checks');
    return data;
  }
  rest(path, method, body, mutate) { return this.call(`/repos/${this.repository}/${path}`, method, body, mutate); }
  async pages(path, key) {
    const all = [];
    for (let page = 1; ; page++) {
      const data = await this.rest(`${path}${path.includes('?') ? '&' : '?'}per_page=100&page=${page}`);
      const items = key ? data[key] : data;
      all.push(...items);
      if (items.length < 100) return all;
    }
  }
  pr(number) { return this.rest(`pulls/${number}`); }
  openPulls() { return this.pages('pulls?state=open&base=partial'); }
  async labelEvent(number) {
    return (await this.pages(`issues/${number}/events`)).filter(event => event.event === 'labeled'
      && event.label?.name === queueLabel).sort((a, b) => b.id - a.id)[0];
  }
  async permission(login) { return (await this.rest(`collaborators/${encodeURIComponent(login)}/permission`)).permission; }
  queueChecks(sha) { return this.pages(`commits/${sha}/check-runs?check_name=${encodeURIComponent(queueCheck)}`, 'check_runs'); }
  async approve(pr, event, reason) {
    return this.rest('check-runs', 'POST', {
      name: queueCheck, head_sha: pr.head.sha, external_id: approvalId(pr.number, event, pr.head.sha),
      status: 'completed', conclusion: 'neutral', details_url: this.runUrl,
      output: { title: 'Reviewed for the partial queue', summary: reason },
    });
  }
  updateCheck(id, reason, conclusion) {
    return this.rest(`check-runs/${id}`, 'PATCH', { status: 'completed', conclusion,
      output: { title: 'Partial merge queue', summary: reason } });
  }
  async partialSha() { return (await this.rest('git/ref/heads/partial')).object.sha; }
  compare(base, head) { return this.rest(`compare/${base}...${head}`); }
  commit(sha) { return this.rest(`git/commits/${sha}`); }
  updateBranch(number, sha) { return this.rest(`pulls/${number}/update-branch`, 'PUT', { expected_head_sha: sha }, true); }
  merge(number, sha, title) { return this.rest(`pulls/${number}/merge`, 'PUT', {
    sha, merge_method: 'squash', commit_title: `${title} (#${number})`,
  }, true); }
  runs(sha) { return this.pages(`actions/runs?event=pull_request&head_sha=${sha}`, 'workflow_runs'); }
  pause() { return new Promise(resolve => setTimeout(resolve, 500)); }
  async checks(number) {
    const [owner, name] = this.repository.split('/');
    const checks = [];
    let cursor = null;
    do {
      const data = await this.call('/graphql', 'POST', { query: `
        query($owner:String!,$name:String!,$number:Int!,$cursor:String) {
          repository(owner:$owner,name:$name) { pullRequest(number:$number) {
            commits(last:1) { nodes { commit { statusCheckRollup { contexts(first:100,after:$cursor) {
              pageInfo { hasNextPage endCursor }
              nodes { __typename ... on CheckRun { name status conclusion startedAt
                checkSuite { app { databaseId } workflowRun { databaseId } } } }
            } } } } }
          } }
        }`, variables: { owner, name, number, cursor } });
      const connection = data.data.repository.pullRequest.commits.nodes[0]?.commit.statusCheckRollup?.contexts;
      checks.push(...(connection?.nodes ?? []));
      cursor = connection?.pageInfo.hasNextPage ? connection.pageInfo.endCursor : null;
    } while (cursor);
    return checks;
  }
}

async function main() {
  const mode = process.argv[2];
  assert(['authorize', 'process'].includes(mode), 'Use authorize or process');
  const api = new GitHub(process.env.GITHUB_REPOSITORY, process.env.GH_TOKEN,
    process.env.MERGE_QUEUE_TOKEN, `${process.env.GITHUB_SERVER_URL}/${process.env.GITHUB_REPOSITORY}/actions/runs/${process.env.GITHUB_RUN_ID}`);
  const outcome = mode === 'authorize'
    ? await authorize(api, JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, 'utf8')))
    : await processQueue(api);
  const message = `${outcome.number ? `PR #${outcome.number}: ` : ''}${outcome.state}: ${outcome.reason}`;
  console.log(message);
  if (process.env.GITHUB_STEP_SUMMARY) appendFileSync(process.env.GITHUB_STEP_SUMMARY, `${message}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch(error => { console.error(error.message); process.exitCode = 1; });
}
