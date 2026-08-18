use crate::api::{TranscriptSearchResult, TranscriptSegment};
use chrono::Utc;
use sqlx::{Connection, Error as SqlxError, SqlitePool};
use tracing::{error, info};
use uuid::Uuid;

pub struct TranscriptsRepository;

impl TranscriptsRepository {
    /// Saves a new meeting and its associated transcript segments.
    /// This function uses a transaction to ensure that either both the meeting
    /// and all its transcripts are saved, or none of them are.
    pub async fn save_transcript(
        pool: &SqlitePool,
        meeting_title: &str,
        transcripts: &[TranscriptSegment],
        folder_path: Option<String>,
    ) -> Result<String, SqlxError> {
        let meeting_id = format!("meeting-{}", Uuid::new_v4());

        let mut conn = pool.acquire().await?;
        let mut transaction = conn.begin().await?;

        let now = Utc::now();

        // 1. Create the new meeting
        let result = sqlx::query(
            "INSERT INTO meetings (id, title, created_at, updated_at, folder_path) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&meeting_id)
        .bind(meeting_title)
        .bind(now)
        .bind(now)
        .bind(&folder_path)
        .execute(&mut *transaction)
        .await;

        if let Err(e) = result {
            error!("Failed to create meeting '{}': {}", meeting_title, e);
            transaction.rollback().await?;
            return Err(e);
        }

        info!("Successfully created meeting with id: {}", meeting_id);

        // 2. Save each transcript segment with audio timing fields
        for segment in transcripts {
            let transcript_id = format!("transcript-{}", Uuid::new_v4());
            let result = sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp, audio_start_time, audio_end_time, duration, speaker)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
            )
            .bind(&transcript_id)
            .bind(&meeting_id)
            .bind(&segment.text)
            .bind(&segment.timestamp)
            .bind(segment.audio_start_time)
            .bind(segment.audio_end_time)
            .bind(segment.duration)
            .bind(&segment.speaker)
            .execute(&mut *transaction)
            .await;

            if let Err(e) = result {
                error!(
                    "Failed to save transcript segment for meeting {}: {}",
                    meeting_id, e
                );
                transaction.rollback().await?;
                return Err(e);
            }
        }

        info!(
            "Successfully saved {} transcript segments for meeting {}",
            transcripts.len(),
            meeting_id
        );

        // Commit the transaction
        transaction.commit().await?;

        Ok(meeting_id)
    }

    /// Full-text search over transcripts and summaries, best-ranked hit per
    /// meeting. Backed by the `search_index` FTS5 table, which triggers keep
    /// current — see migration `20260811000000_add_search_index.sql`.
    pub async fn search_transcripts(
        pool: &SqlitePool,
        query: &str,
    ) -> Result<Vec<TranscriptSearchResult>, SqlxError> {
        let fts_query = to_fts_query(query);
        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        // bm25()/snippet() cannot share a SELECT with a window function, so the
        // FTS scan and the per-meeting collapse are separate CTEs. Aliasing
        // search_index also breaks the auxiliary functions, hence the bare name.
        let rows = sqlx::query_as::<_, (String, String, String, String, String)>(
            r#"
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
            FROM ranked r
            JOIN meetings m ON m.id = r.meeting_id
            WHERE r.rn = 1
            ORDER BY r.rank
            LIMIT 50
            "#,
        )
        .bind(&fts_query)
        .fetch_all(pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(
                |(id, title, match_context, timestamp, kind)| TranscriptSearchResult {
                    id,
                    title,
                    match_context,
                    timestamp,
                    kind,
                },
            )
            .collect())
    }
}

/// Turns raw user input into an FTS5 MATCH expression: `coreml drop` becomes
/// `"coreml" "drop"*` — implicit AND, prefix match on the last token so results
/// narrow as the user types.
///
/// This is the one trust boundary here. Quoting every token makes `"`, `*`,
/// `-`, `:` and `(` literal, so there is no character class to get wrong. Tokens
/// with no alphanumeric character are dropped rather than quoted: they match
/// nothing, and ANDing one in would zero an otherwise good query (`metal -`).
fn to_fts_query(raw: &str) -> String {
    let mut tokens: Vec<String> = raw
        .split_whitespace()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if let Some(last) = tokens.last_mut() {
        last.push('*');
    }
    tokens.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn fts_query_quotes_tokens_and_prefixes_the_last() {
        assert_eq!(to_fts_query("coreml drop"), r#""coreml" "drop"*"#);
        assert_eq!(to_fts_query("metal"), r#""metal"*"#);
        assert_eq!(to_fts_query("  spaced   out  "), r#""spaced" "out"*"#);
    }

    #[test]
    fn fts_query_neutralises_syntax_characters() {
        assert_eq!(to_fts_query(r#"say "hi""#), r#""say" """hi"""*"#);
        assert_eq!(to_fts_query("re: drop-in (v2)"), r#""re:" "drop-in" "(v2)"*"#);
    }

    #[test]
    fn fts_query_is_empty_when_nothing_searchable_remains() {
        assert_eq!(to_fts_query(""), "");
        assert_eq!(to_fts_query("   "), "");
        assert_eq!(to_fts_query("- : ("), "");
    }

    /// max_connections(1): every connection to `sqlite::memory:` opens its own
    /// database, so a multi-connection pool would migrate one and query another.
    async fn migrated_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn search_spans_transcripts_and_summaries() {
        let pool = migrated_pool().await;
        let now = "2026-08-11T10:00:00Z";

        for (id, title) in [("m1", "Standup"), ("m2", "Retro")] {
            sqlx::query(
                "INSERT INTO meetings (id, title, created_at, updated_at) VALUES (?, ?, ?, ?)",
            )
            .bind(id)
            .bind(title)
            .bind(now)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }

        // Both m1 rows match, so a single m1 result proves the per-meeting collapse.
        for (id, text) in [
            ("t1", "we should drop coreml and use metal"),
            ("t2", "dropping coreml was the right call"),
        ] {
            sqlx::query(
                "INSERT INTO transcripts (id, meeting_id, transcript, timestamp) VALUES (?, 'm1', ?, ?)",
            )
            .bind(id)
            .bind(text)
            .bind(now)
            .execute(&pool)
            .await
            .unwrap();
        }

        sqlx::query(
            "INSERT INTO summary_processes (meeting_id, status, created_at, updated_at, result)
             VALUES ('m2', 'completed', ?, ?, ?)",
        )
        .bind(now)
        .bind(now)
        .bind(
            r#"{"Decisions":{"title":"Decisions","blocks":[{"id":"b-uuid","type":"bullet","content":"Dropped CoreML for Metal","color":"default"}]}}"#,
        )
        .execute(&pool)
        .await
        .unwrap();

        let hits = TranscriptsRepository::search_transcripts(&pool, "coreml drop")
            .await
            .unwrap();
        let by_id: Vec<(&str, &str)> = hits
            .iter()
            .map(|h| (h.id.as_str(), h.kind.as_str()))
            .collect();
        assert_eq!(by_id, vec![("m2", "summary"), ("m1", "transcript")]);
        assert!(hits[0].match_context.contains("Dropped CoreML"));
        assert_eq!(hits[1].timestamp, now);

        // A term that only ever appeared in the summary — the whole point of the change.
        let summary_only = TranscriptsRepository::search_transcripts(&pool, "decisions")
            .await
            .unwrap();
        assert_eq!(summary_only.len(), 1);
        assert_eq!(summary_only[0].id, "m2");

        // Block scaffolding (uuid, type, color) must not be indexed.
        for noise in ["bullet", "b-uuid", "default"] {
            assert!(
                TranscriptsRepository::search_transcripts(&pool, noise)
                    .await
                    .unwrap()
                    .is_empty(),
                "summary JSON scaffolding leaked into the index: {noise}"
            );
        }

        // retranscription.rs deletes a meeting's transcript rows in place.
        sqlx::query("DELETE FROM transcripts WHERE meeting_id = 'm1'")
            .execute(&pool)
            .await
            .unwrap();
        let after = TranscriptsRepository::search_transcripts(&pool, "coreml drop")
            .await
            .unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].id, "m2");
    }
}
