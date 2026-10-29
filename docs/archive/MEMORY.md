# Reviewed learning

Enable Memory in the desktop Memory dialog or with `bw config set memory on`.
In Agent mode, Blackwall can read reviewed facts with `memory_list` and suggest a
preference, convention, correction, or lesson using `memory_propose`. Proposals
stay out of persistent model context until you review them. Learning adds no
extra model calls; the agent chooses when to propose a useful durable fact during
a task or in response to a correction.

Open **Memory** to review pending suggestions. Each shows its source conversation
and user message, scope, saved text, and proposed text. Edit the proposed text and
choose **Approve**, or **Reject** it. The saved list supports **Edit** and **Forget**.
Review remains available while memory is off.

The same queue and facts are available from the CLI:

```sh
bw memory pending
bw memory approve <proposal-id>
bw memory approve <proposal-id> Revised text to remember
bw memory reject <proposal-id>
bw memory list
bw memory edit <memory-id> Updated text
bw memory forget <memory-id>
```

Use `/memory pending` and the other actions during interactive chat. `/memory on`
and `/memory off` control injection and agent access. CLI settings retain their
existing precedence over desktop settings; each client's memory switch controls
its own runs. Both clients share the same SQLite database in the configured data
folder.

User scope stores user preferences across projects. Project and environment
scopes store conventions and environment facts for the canonical selected project
folder only. Ordinary Chat receives user preferences; Agent runs also receive
facts for their project. Child agents and guest chats have no owner memory tools.
Guest chats use no owner memory injection. Memory is untrusted context and never
grants file access, shell approval, network access, or any other tool permission.

Stable topic keys identify related facts within a scope and project. The agent
should read existing facts and reuse a key or provide a `replaces` ID when refining
one. Whitespace/case-equivalent text and related pending keys are deduplicated.
This is deterministic matching, not semantic similarity: different topics or
wordings still need owner judgment. A proposal captures the saved revision;
editing, another approval, or forgetting invalidates that snapshot. Stale approval
fails without modifying the saved fact. Reject the old proposal and ask the agent
for a new one. Forgetting also removes pending replacements.

Schema version 3 adds scopes, stable keys, revisions, provenance, and the review
queue. Existing memories migrate to user scope without changing their text. Each
proposal references the actual saved user message that prompted the run. Accepted
facts retain that source. No transcripts are copied into memory metadata. The
queue is limited to 200 proposals, the fact store to 2,000 entries, individual
text to 8,000 bytes, tool-list output to 16,000 bytes, and injected context to
4,000 characters including its framing. The database upgrade is transactional;
older app versions will refuse to open the newer schema.
