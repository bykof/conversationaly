-- Full-text search across transcripts and summaries (SQLite FTS5).
-- Replaces the LIKE '%q%' scan in TranscriptsRepository::search_transcripts.
--
-- src_id identifies the source row so update/delete are point deletes:
-- transcripts.id for transcript rows, meeting_id for summaries (one per meeting).
-- FTS5 has no upsert, hence delete-then-insert.
--
-- json_tree(CASE WHEN json_valid(x) THEN x END) is the guard idiom throughout:
-- json_tree() raises on non-JSON text, and a raise inside a trigger aborts the
-- caller's INSERT/UPDATE — a legacy result blob would break summary saving, not
-- just indexing. json_tree(NULL) yields no rows, so this also covers result IS NULL.

CREATE VIRTUAL TABLE IF NOT EXISTS search_index USING fts5(
    text,
    meeting_id UNINDEXED,
    kind UNINDEXED,
    ts UNINDEXED,
    src_id UNINDEXED,
    tokenize = 'unicode61 remove_diacritics 2'
);

-- transcripts ---------------------------------------------------------------

CREATE TRIGGER IF NOT EXISTS search_index_transcripts_ai
AFTER INSERT ON transcripts BEGIN
    INSERT INTO search_index (text, meeting_id, kind, ts, src_id)
    VALUES (new.transcript, new.meeting_id, 'transcript', new.timestamp, new.id);
END;

CREATE TRIGGER IF NOT EXISTS search_index_transcripts_au
AFTER UPDATE OF transcript ON transcripts BEGIN
    DELETE FROM search_index WHERE kind = 'transcript' AND src_id = old.id;
    INSERT INTO search_index (text, meeting_id, kind, ts, src_id)
    VALUES (new.transcript, new.meeting_id, 'transcript', new.timestamp, new.id);
END;

CREATE TRIGGER IF NOT EXISTS search_index_transcripts_ad
AFTER DELETE ON transcripts BEGIN
    DELETE FROM search_index WHERE kind = 'transcript' AND src_id = old.id;
END;

-- summaries -----------------------------------------------------------------
-- summary_processes.result is the serialized summary Value; only 'content' and
-- 'title' strings are prose. Indexing it raw would put block UUIDs and
-- type/color values in the index, so "text" would match every summary.

CREATE TRIGGER IF NOT EXISTS search_index_summary_ai
AFTER INSERT ON summary_processes BEGIN
    INSERT INTO search_index (text, meeting_id, kind, ts, src_id)
    SELECT s.txt, new.meeting_id, 'summary', new.updated_at, new.meeting_id
    FROM (SELECT group_concat(value, ' ') AS txt
          FROM json_tree(CASE WHEN json_valid(new.result) THEN new.result END)
          WHERE key IN ('content', 'title') AND type = 'text') s
    WHERE s.txt IS NOT NULL;
END;

CREATE TRIGGER IF NOT EXISTS search_index_summary_au
AFTER UPDATE OF result ON summary_processes BEGIN
    DELETE FROM search_index WHERE kind = 'summary' AND src_id = new.meeting_id;
    INSERT INTO search_index (text, meeting_id, kind, ts, src_id)
    SELECT s.txt, new.meeting_id, 'summary', new.updated_at, new.meeting_id
    FROM (SELECT group_concat(value, ' ') AS txt
          FROM json_tree(CASE WHEN json_valid(new.result) THEN new.result END)
          WHERE key IN ('content', 'title') AND type = 'text') s
    WHERE s.txt IS NOT NULL;
END;

-- meetings ------------------------------------------------------------------
-- Purges every kind by meeting_id. This is also what removes summary rows on
-- delete_meeting(), so summary_processes needs no delete trigger of its own.
-- Explicit rather than relying on ON DELETE CASCADE to fire the child tables'
-- triggers: cascade only fires triggers when recursive_triggers is on.

CREATE TRIGGER IF NOT EXISTS search_index_meetings_ad
AFTER DELETE ON meetings BEGIN
    DELETE FROM search_index WHERE meeting_id = old.id;
END;

-- backfill ------------------------------------------------------------------

INSERT INTO search_index (text, meeting_id, kind, ts, src_id)
SELECT transcript, meeting_id, 'transcript', timestamp, id FROM transcripts;

INSERT INTO search_index (text, meeting_id, kind, ts, src_id)
SELECT txt, meeting_id, 'summary', updated_at, meeting_id FROM (
    SELECT p.meeting_id, p.updated_at,
           (SELECT group_concat(value, ' ')
            FROM json_tree(CASE WHEN json_valid(p.result) THEN p.result END)
            WHERE key IN ('content', 'title') AND type = 'text') AS txt
    FROM summary_processes p
) WHERE txt IS NOT NULL;
