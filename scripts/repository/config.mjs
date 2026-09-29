export function branchRules(branch, approvingReviews = 0) {
  return {
    name: branch === 'main' ? 'Production releases' : 'Partial integration',
    target: 'branch',
    enforcement: 'active',
    bypass_actors: [],
    conditions: { ref_name: { include: [`refs/heads/${branch}`], exclude: [] } },
    rules: [
      { type: 'deletion' },
      { type: 'non_fast_forward' },
      { type: 'pull_request', parameters: {
        required_approving_review_count: approvingReviews,
        dismiss_stale_reviews_on_push: true,
        require_code_owner_review: approvingReviews > 0,
        require_last_push_approval: approvingReviews > 0,
        required_review_thread_resolution: true,
        // Preserve shared ancestry between the two long-lived branches.
        allowed_merge_methods: branch === 'main' ? ['merge'] : ['merge', 'squash'],
      } },
      { type: 'required_status_checks', parameters: {
        strict_required_status_checks_policy: true,
        do_not_enforce_on_create: true,
        required_status_checks: [
          { context: 'Quality gate', integration_id: 15368 },
          { context: 'Production promotion', integration_id: 15368 },
        ],
      } },
    ],
  };
}

export const labels = [
  ['release', '5319E7', 'A partial-to-main production promotion'],
  ['security', 'B60205', 'Security-sensitive work; report vulnerabilities privately'],
  ['dependencies', '0366D6', 'Dependency maintenance'],
  ['type: feature', 'A2EEEF', 'New behavior'],
  ['type: maintenance', 'D4C5F9', 'Repository, tooling, or operational maintenance'],
  ['priority: high', 'D93F0B', 'Prioritize in the next release batch'],
  ['blocked', 'FBCA04', 'Waiting on a dependency or decision'],
];

export function tagRules() {
  return {
    name: 'Immutable release tags', target: 'tag', enforcement: 'active', bypass_actors: [],
    conditions: { ref_name: { include: ['refs/tags/v*'], exclude: [] } },
    rules: [{ type: 'deletion' }, { type: 'update' }],
  };
}
