# Conversation recall

Search is local and works without a model connection. The CLI and desktop search the same saved
SQLite conversations. Only user and assistant message text is indexed. Attachments, tool arguments,
tool outputs, approval records, and compacted checkpoints are excluded. Text that appears inside a
message remains untrusted historical data.

## Search and open

```sh
bw sessions search "release checks"
bw sessions search checks --project "/absolute/project path" --after 1790812800000 --before 1791935999999 --limit 5
```

`--project` matches the exact saved project path. `--after` and `--before` are inclusive Unix
milliseconds for the **session's last saved update**, rather than the individual message's date.
Search uses literal words joined with AND; FTS operators, regular expressions, and prefixes are
not exposed. Results contain the session ID, message ID (or a stable positional reference for old
messages without IDs), message position, saved mode, date, project, and excerpt.

The CLI `/recall <words>` accepts the same filters. Use `bw resume <session-id>` for an Agent result.
The CLI refuses to convert Chat histories into Agent sessions; open Chat results in desktop.
Desktop **Search history** or `/recall <words>` opens a panel with project and UTC date controls.
Select a result to open its saved conversation and scroll to the referenced message. The saved mode
and project are restored even when the search was started from another mode. Missing project folders
leave the current conversation in place and report an error.

Historical tool activity is labeled **History**. Opening or recalling a result never runs a saved
tool, restores an old pending approval, or reuses a remembered permission. A subsequent new task
still follows the current approval rules.

## Optional agent access

The `session_recall` Agent tool is off by default and independent of reviewed memory learning.
Enable it with `bw config set recall on` (or `/settings recall on`) in the CLI, or the checkbox in
the desktop search panel. CLI settings take precedence over desktop preferences. Turn it off with
`bw config set recall off` or the checkbox; manual owner search remains available.

The tool searches only the current selected project, regardless of any project argument. Guests,
child agents, and Chat mode receive no recall tool. Guests cannot call the local search API; desktop
search requires the unlocked owner application. No search endpoint is exposed by the sharing server.
Enabling recall lets the selected model receive excerpts from this project's saved messages when it
calls the tool. Recall text never grants permission to execute actions.

## Bounds and migration

Each result has at most 600 excerpt characters and 120 title characters. Search returns at most
10 results (5 by default in the CLI), with serialized hit data capped at 14,000 bytes. Agent output
includes only these excerpts and references. Queries are limited to 512 UTF-8 bytes and 32 words;
project filters are limited to 2,048 bytes.

Schema version 4 adds the FTS index while preserving original session JSON. Every database open
indexes at most five unindexed sessions, most recently updated first. Each session body is bounded
by the existing 48 MiB storage limit. Search reports how many older sessions still need indexing;
repeat a search to advance migration. Completed sessions are marked transactionally, so restart
continues without duplicate index entries. Legacy browser imports also enter this migration.

Indexing retains up to 64 KiB of UTF-8 text per message and 512 KiB per session, in transcript order;
text beyond these limits remains saved but is not searchable. New saves update the transcript and
index together. Replacing or removing messages deletes their old index rows; deleting a conversation
cascades to its index and migration marker. SQLite and FTS secure deletion are enabled. The index
is derived local data, not an independent transcript archive. Backups made before deletion may
still contain the original saved content.

Older Blackwall versions reject schema version 4. Upgrade clients sharing this data directory
together. Original transcripts remain available to the upgraded clients; do not downgrade an
in-place migrated database.
