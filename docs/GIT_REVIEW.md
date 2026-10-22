# Read-only Git review

Run `bw review` from a Git working tree, or `/review` during CLI or desktop Agent chat. Review uses the configured
model and connection, with a separate request containing only review instructions and Git evidence.
The dedicated CLI command does not add messages to an Agent conversation; desktop Agent reviews
retain their result in the conversation.

```sh
bw review                       # default: --uncommitted
bw review --uncommitted         # net staged + unstaged changes against HEAD, plus untracked text
bw review --commit HEAD         # one commit against its first parent (root commits supported)
bw review --base origin/partial # HEAD against its merge base with the supplied base
bw review --json --base main    # one JSON object for scripts
```

The three targets are mutually exclusive. CLI `/review` accepts the same flags; desktop Agent
`/review` accepts the three target flags and displays text results. References resolve
to commit IDs before comparison. Base and commit review exclude working-tree changes. Reviews
from subdirectories cover the repository root. An unborn branch supports working-tree review.

Findings are sorted by priority (P0 critical, P1 high, P2 normal, P3 low), with a relative file,
line, actionable title, and explanation. Unverified hypotheses are listed separately. Empty
findings are valid. JSON output contains `target`, `findings`, `hypotheses`, and
`omittedUntrackedFiles`. Findings must reference a changed file and a positive line number;
model conclusions and exact line accuracy still need human review. Invalid model output fails
explicitly. The exit status reports whether the review completed, rather than whether it found bugs.

Review has no model tools. It cannot write files, run shell commands, browse the web, change
memory, launch MCP servers or command tools, read skills, or delegate. Saved tool settings and
allow policies cannot expand these capabilities. Project instructions and conversation history
are not loaded as instructions; instructions present in a diff are untrusted evidence. A model
response requesting tools is rejected without executing anything. Git external diff drivers,
text conversions, fsmonitor helpers, pagers, and recursive submodule diffs are disabled; index
refreshes are disabled and inherited Git directory/config overrides are removed. Lazy object fetching
and all Git transport protocols are disabled, so partial clones with missing objects fail offline.

Evidence is bounded to 256 KiB; larger patches fail instead of silently truncating. Untracked
files respect Git ignores, with at most 100 candidates and 64 KiB per supported text file.
Unsupported, binary, oversized, or inaccessible untracked files are listed as omitted. Symlinks
are skipped and reads stay inside the workspace. Tracked binary files and submodules have Git's
summary only. The model sees diff context, rather than complete source files, and cannot run
tests, so concerns requiring more evidence belong in hypotheses. The configured context budget
may impose a smaller limit than the evidence cap.

Ctrl+C cancels collection or the model request. Dropping the core review future also cancels it;
Git children are killed on drop and have a 30-second timeout. No approvals are needed. The dedicated CLI path never creates the Agent; the shared Agent
dispatches `/review` before loading any capabilities and never creates a review tool dispatcher. An unchanged tree returns no findings
without contacting the model service.
