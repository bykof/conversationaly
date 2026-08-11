# Search across all meetings (SQLite FTS5)

**Date:** 2026-08-07
**Status:** implemented 2026-08-11, in
`migrations/20260811000000_add_search_index.sql` and
`database/repositories/transcript.rs`. Scope narrowed and the query corrected
during implementation — see "Corrections" at the end.

## Problem

Search today only reaches raw transcript rows. Ask "when did we decide to drop
CoreML" and you get nothing, because that sentence lives in the *summary*, which
the index cannot see. The existing query also scans, cannot rank, and returns an
unbounded result set.

Current implementation, `database/repositories/transcript.rs:87-140`:

```sql
SELECT m.id, m.title, t.transcript, t.timestamp
FROM meetings m JOIN transcripts t ON m.id = t.meeting_id
WHERE LOWER(t.transcript) LIKE ?          -- '%query%'
```

Five concrete defects:

1. **Scope.** Only `transcripts.transcript`. Summaries (`summary_processes.result`),
   notes (`meeting_notes.notes_markdown`), and titles are invisible.
2. **Leading wildcard** defeats any index — full scan of every transcript row.
3. **No ranking.** Rows come back in arbitrary storage order.
4. **No limit.** Every match crosses the IPC boundary, even though the UI keeps
   at most one per meeting (`Sidebar/index.tsx:197`, `searchResults.find`).
5. **Substring, not word-aware.** `drop` matches `eavesdropping`; `coreml metal`
   matches nothing unless the two words happen to be adjacent.

## Approach

One FTS5 virtual table covering transcripts and summaries, kept current by SQL
triggers, queried by a rewritten `search_transcripts`. FTS5 fixes 2, 3 and 5
outright and supplies `snippet()` and `bm25()`, so the hand-rolled snippet
helper goes away.

**No new dependency.** `sqlx`'s `sqlite` feature forces `sqlx-sqlite/bundled`,
and `libsqlite3-sys`'s bundled build passes `-DSQLITE_ENABLE_FTS5`
unconditionally. FTS5 is already in the binary.

### Schema

New migration, `<timestamp>_add_search_index.sql`:

```sql
CREATE VIRTUAL TABLE search_index USING fts5(
  text,
  meeting_id UNINDEXED,
  kind UNINDEXED,
  ts UNINDEXED,
  src_id UNINDEXED,
  tokenize = 'unicode61 remove_diacritics 2'
);
```

`kind` is `transcript` or `summary`. One row per transcript segment; one row per
meeting summary.

`src_id` identifies the source row so update and delete triggers are point
deletes: `transcripts.id` for transcript rows, `meeting_id` for summaries. Without
it, an update trigger has to match on text, which deletes both rows whenever two
segments happen to share a string (`"Yeah."` is common).

`ts` carries the source row's own timestamp — `transcripts.timestamp` for
transcript rows, `summary_processes.updated_at` for summaries. Both are on the
row the trigger already has, so no join.

Text is duplicated into the index rather than using an external-content table.
External content would avoid the copy but needs a `content_rowid` join and three
sets of triggers against `TEXT PRIMARY KEY` tables. A year of meetings is a few
MB of prose; duplicating it is not worth that complexity.

### Extracting prose from the summary JSON

`summary_processes.result` is a serialized `serde_json::Value` with a shallow,
regular shape (`frontend/src/types/index.ts:39-53`):

```json
{ "SectionKey": { "title": "…", "blocks": [ { "id": "…", "type": "…", "content": "…", "color": "…" } ] } }
```

Indexing it raw would put block UUIDs and `color`/`type` values into the index,
so a search for "text" would match every summary ever generated. The prose comes
out in pure SQL via JSON1, which keeps index maintenance inside the triggers:

```sql
SELECT group_concat(value, ' ')
  FROM json_tree(CASE WHEN json_valid(new.result) THEN new.result END)
 WHERE key IN ('content', 'title') AND type = 'text'
```

The `json_valid` guard is not decoration. `json_tree()` raises on non-JSON text,
and a raise inside a trigger aborts the caller's `INSERT`/`UPDATE` — one legacy
`result` blob would break summary *saving*, not just indexing. `json_tree(NULL)`
yields no rows, so the same `CASE` covers `result IS NULL`, which
`create_or_reset_process` writes on every regeneration.

### Keeping the index current

Triggers, not Rust call sites, so a future write path cannot silently skip the
index:

| Table | Events |
|---|---|
| `transcripts` | insert, update of `transcript`, delete |
| `summary_processes` | insert, update of `result` |
| `meetings` | delete → purge every row for that `meeting_id` |

Update triggers delete the prior row for that source then re-insert, since FTS5
has no upsert. The delete runs unconditionally while the re-insert is guarded, so
overwriting a good summary with an unparseable one removes the stale index row
rather than leaving it to match forever.

`summary_processes` needs no delete trigger: nothing in the codebase deletes a
summary row without deleting its meeting, and the `meetings` purge covers that.
`transcripts` does need one — `retranscription.rs:413` deletes a meeting's
transcript rows in place.

The `meetings` delete trigger purges by `meeting_id` explicitly rather than
relying on `ON DELETE CASCADE` to fire the child tables' delete triggers —
cascade only fires triggers when `recursive_triggers` is on, which is not
something to depend on.

The same migration backfills existing rows with two `INSERT … SELECT`
statements.

### Query

`TranscriptsRepository::search_transcripts` keeps its signature and, apart from
one added field, its return type:

```sql
WITH hits AS (
  SELECT meeting_id, kind, ts,
         bm25(search_index) AS rank,
         snippet(search_index, 0, '', '', '…', 12) AS ctx
  FROM search_index
  WHERE search_index MATCH ?
),
ranked AS (
  SELECT *, ROW_NUMBER() OVER (PARTITION BY meeting_id ORDER BY rank) AS rn
  FROM hits
)
SELECT r.meeting_id, m.title, r.ctx, r.ts, r.kind
FROM ranked r JOIN meetings m ON m.id = r.meeting_id
WHERE r.rn = 1
ORDER BY r.rank
LIMIT 50;
```

Two CTEs, not one. FTS5 auxiliary functions cannot share a `SELECT` with a window
function — `bm25()` in the same block as `ROW_NUMBER()` fails at prepare time with
`unable to use function bm25 in the requested context`. Aliasing the table breaks
them too (`bm25(s)` → `no such column: s`), so the FTS scan uses the bare name and
the collapse happens one level up.

Best-ranked row per meeting via `ROW_NUMBER()`, not the `GROUP BY` bare-column
trick — that shortcut only picks the matching row under `min()`/`max()`, and the
ranking expression here is `bm25()`. Window functions need SQLite 3.25+; the
bundled build is far newer.

`snippet()` gets empty delimiters rather than `<mark>` tags: the sidebar renders
`matchContext` as JSX text, so any markup would show up literally.

`LIMIT 50` matches how the UI consumes results (one hit per meeting, filtering a
sidebar list).

### Sanitizing the query string

This is the one trust boundary and the one place not to be terse. Raw user text
in `MATCH` is a syntax error on `"`, `*`, `-`, `:` and `(`, so an unsanitized
query makes search fail on ordinary typing.

```rust
/// `coreml drop` -> `"coreml" "drop"*`
fn to_fts_query(raw: &str) -> String
```

Quote every whitespace-separated token, doubling any `"` inside it; join with a
space for implicit AND; suffix `*` on the final token for prefix matching as the
user types. Quoting renders every special character literal, so there is no
character class to get wrong. An empty result means the caller returns an empty
vec, as it does today.

Tokens containing no alphanumeric character are dropped rather than quoted. A
quoted all-punctuation token is *safe* — `"-"*` and `""""*` both parse and return
zero rows — but it matches nothing, and the implicit AND then zeroes an otherwise
good query, so typing `metal -` would blank the results.

### Result type and UI

`TranscriptSearchResult` (`api/api.rs:41-47`) gains one field:

```rust
pub kind: String,   // "transcript" | "summary"
```

`Sidebar/index.tsx` labels summary hits. Without it a summary hit is
indistinguishable from a transcript hit, which makes the widened scope invisible
to the user. Transcript hits stay unlabelled — they are the default, and labelling
both would put a badge on every row of an already dense list. This is the only
frontend change.

### Deletions

`TranscriptsRepository::get_match_context` (`transcript.rs:123-140`) is removed;
`snippet()` replaces it.

## Testing

One unit test on `to_fts_query`: multi-word input, embedded quotes, punctuation,
empty and whitespace-only input.

One integration test against an in-memory SQLite pool: run the migration, insert
a transcript row and a summary row for two meetings, assert a two-word query
matches, that a summary-only term is found, that block UUIDs and `type`/`color`
values did *not* reach the index, and that results carry the right `kind` and
collapse to one row per meeting. It also deletes a meeting's transcript rows and
re-queries, covering the retranscription path through the delete trigger.

The pool must be `max_connections(1)`: every connection to `sqlite::memory:`
opens its own database, so a default pool migrates one and queries another.

No test framework, no fixtures.

## Explicitly out of scope

- **Per-kind bm25 weighting.** Add when transcript noise visibly outranks summary
  hits in practice.
- **Stemming** (`porter` tokenizer). Meeting search is mostly proper nouns and
  jargon, where stemming hurts more than it helps.
- **Embeddings / semantic search.** FTS5 first; revisit only if lexical search
  provably misses what users look for.
- **`LIKE` fallback.** FTS5 is compiled in unconditionally, so there is nothing
  to fall back from.
- **Match highlighting.** `snippet()` can wrap hits in `<mark>`, but the sidebar
  renders the snippet as text. Add when the snippet render path can take markup.

## Corrections

Three things in the original design did not survive contact with the code.

**Notes were dropped from scope.** `meeting_notes` has no write path and no read
path anywhere in the repo — only the migration that created it mentions it.
Indexing it meant three triggers and a backfill against a permanently empty
table. Re-add in the commit that ships the notes feature.

**Titles were dropped from scope.** `Sidebar/index.tsx:189-193` already filters
the meeting list by `title.toLowerCase().includes(q)`, client-side, over the full
loaded list. Title search works today and substring matching is strictly more
permissive than FTS5 tokenised matching, so indexing titles duplicated existing
behaviour and added a `meetings` update trigger for it. Defect 1 in the problem
statement overstated the gap: titles were never invisible, only summaries were.

**The query did not run.** See the note under "Query" — `bm25()` and
`ROW_NUMBER()` cannot share a `SELECT`. The integration test running the real
migration is what catches this class of error; a unit test on the sanitiser alone
would have shipped it.

Net: ten triggers became five, four content kinds became two.
