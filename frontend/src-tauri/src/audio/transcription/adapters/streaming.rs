// audio/transcription/adapters/streaming.rs
//
// Driving adapter for models that stream, which is the preferred path: one
// transcribe.cpp Stream stays open for the whole meeting and audio is fed to it
// continuously, so the model keeps its context across pauses.
//
// The split that makes this work is transcribe.cpp's own:
//   - `committed` text is append-only and never rewritten -> a TranscriptChunk,
//     which is exactly what the transcript table assumes.
//   - `tentative` text is the volatile suffix -> shown greyed, never saved.
// Because committed text can only grow, nothing downstream needs revision or
// reconciliation logic.

use crate::audio::transcription::ports::{Transcriber, TranscriptChunk, TranscriptSink};
use anyhow::Result;
use log::{info, warn};
use std::time::{Duration, Instant};

/// Bucket width of the `buffered_ms` histogram below.
const BUCKET_MS: i64 = 100;

/// 100ms buckets covering 0..30s, with the last one as the overflow.
const BUCKETS: usize = 301;

pub struct StreamingTranscriber<'a> {
    stream: transcribe_cpp::Stream<'a>,
    /// How much of the stream's committed text has already been sent on.
    cursor: CommittedCursor,
    /// Worst decoder-internal lag seen: audio fed but not yet committed.
    peak_buffered_ms: i64,
    /// The same signal as a distribution, so one bad stall does not stand in
    /// for the whole meeting the way a peak does.
    buffered: BufferedMsHistogram,
    /// Total wall time spent inside `Stream::feed`. Against the audio actually
    /// fed, this is the measured real-time factor of the decoder itself.
    feed_wall: Duration,
    /// Audio the stream reports receiving so far. Taken from the library rather
    /// than counted from sample lengths, so it cannot disagree with what the
    /// stream thinks it has.
    audio_received_ms: i64,
    commits: u64,
}

impl<'a> StreamingTranscriber<'a> {
    pub fn new(stream: transcribe_cpp::Stream<'a>) -> Self {
        Self {
            stream,
            cursor: CommittedCursor::default(),
            peak_buffered_ms: 0,
            buffered: BufferedMsHistogram::new(),
            feed_wall: Duration::ZERO,
            audio_received_ms: 0,
            commits: 0,
        }
    }

    /// Note what a feed cost and how far behind it left the decoder.
    ///
    /// `buffered_ms` is the stream's own `input_received - audio_committed`, so
    /// it measures the decoder's internal lag — not the depth of the channel
    /// feeding it. A model slower than real time shows up here first.
    fn measure(&mut self, update: &transcribe_cpp::StreamUpdate) {
        self.peak_buffered_ms = self.peak_buffered_ms.max(update.buffered_ms);
        self.buffered.add(update.buffered_ms);
        self.audio_received_ms = update.input_received_ms;
    }

    /// One line, at the end of the recording, with the numbers that decide
    /// whether this model can keep up — and whether the stream's lookahead
    /// could be tightened without it falling behind.
    fn report(&self) {
        let audio_secs = self.audio_received_ms as f64 / 1000.0;
        let wall_secs = self.feed_wall.as_secs_f64();
        let rtf = if audio_secs > 0.0 { wall_secs / audio_secs } else { f64::NAN };

        info!(
            "BENCH stream: {} commits over {audio_secs:.1}s of audio; feed RTF {rtf:.3} \
             ({wall_secs:.1}s decoding); buffered_ms peak {}, median ~{}",
            self.commits,
            self.peak_buffered_ms,
            self.buffered
                .median_ms()
                .map_or_else(|| "n/a".to_string(), |ms| ms.to_string()),
        );
    }

    /// Send on whatever committed text is new since the last call.
    fn emit_committed(&mut self, audio_committed_ms: i64, sink: &mut dyn TranscriptSink) {
        let committed = self.stream.text().committed;
        let audio_end = audio_committed_ms as f64 / 1000.0;

        if let Some(chunk) = self.cursor.advance(&committed, audio_end) {
            self.commits += 1;
            sink.committed(chunk);
        }
    }
}

/// How much of the stream's committed text has already been sent on.
///
/// Pure on purpose — no stream, no sink, no clock — because this is where the
/// append-only invariant actually lives, and an untested invariant is a comment.
/// What it rules out is the obvious-looking alternative: re-read the whole
/// committed prefix each tick and emit it again, letting the UI reconcile. That
/// is quadratic in meeting length, and at meeting length the difference between
/// quadratic and linear is the difference between keeping up and not.
#[derive(Default)]
struct CommittedCursor {
    /// Byte offset into `committed` already sent on. Committed text is
    /// append-only, so this only moves forward and the tail past it is exactly
    /// what is new.
    emitted_len: usize,
    /// Audio position (seconds) that `emitted_len` corresponds to.
    emitted_audio_secs: f64,
}

impl CommittedCursor {
    /// Take whatever tail of `committed` has not been emitted yet.
    ///
    /// `None` means nothing new is owed: the text did not grow, it stopped
    /// being an extension of itself and the cursor resynced, or the new tail
    /// was only whitespace.
    fn advance(&mut self, committed: &str, audio_end_secs: f64) -> Option<TranscriptChunk> {
        if committed.len() <= self.emitted_len {
            return None;
        }

        // Committed text is documented append-only, so emitted_len is a valid
        // boundary into it. Recover rather than panic if that ever stops
        // holding — a slice panic here takes down a live recording.
        let Some(new_text) = committed.get(self.emitted_len..) else {
            warn!(
                "Committed text is not an extension of what was already emitted \
                 (len {} vs offset {}); re-syncing to the current end",
                committed.len(),
                self.emitted_len
            );
            self.emitted_len = committed.len();
            return None;
        };

        let text = new_text.trim().to_string();
        self.emitted_len = committed.len();

        let audio_start = std::mem::replace(&mut self.emitted_audio_secs, audio_end_secs);

        if text.is_empty() {
            return None;
        }

        Some(TranscriptChunk {
            text,
            audio_start,
            audio_end: audio_end_secs,
            // No confidence on this path, deliberately. The only way to get one
            // is `Stream::snapshot()`, which materialises the ENTIRE session —
            // every segment, word and token as an owned String — to average it
            // and throw it away. Doing that per commit, on this thread, over a
            // transcript that grows all meeting, is why the live transcript
            // used to fall further behind the longer a meeting ran. What it
            // bought was a running mean over the whole session, so every line
            // carried the same number anyway.
            confidence: None,
            speaker: None,
        })
    }
}

impl Transcriber for StreamingTranscriber<'_> {
    fn feed(&mut self, pcm_16k: &[f32], sink: &mut dyn TranscriptSink) -> Result<()> {
        // Timed around the call rather than after the `?`, so a decode that
        // ends in an error still costs what it cost. Instant, not wall clock:
        // an RTF computed across a clock adjustment is a fiction.
        let started = Instant::now();
        let update = self.stream.feed(pcm_16k);
        self.feed_wall += started.elapsed();
        let update = update?;
        self.measure(&update);

        if update.committed_changed {
            self.emit_committed(update.audio_committed_ms, sink);
        }
        if update.tentative_changed {
            sink.tentative(&self.stream.text().tentative);
        }
        Ok(())
    }

    fn finish(&mut self, sink: &mut dyn TranscriptSink) -> Result<()> {
        let update = self.stream.finalize();
        if let Ok(update) = update.as_ref() {
            self.emit_committed(update.audio_committed_ms, sink);
        }
        // Reported before the `?` propagates: a stream that failed to finalize
        // is exactly the one whose numbers you want to see.
        self.report();
        update?;
        Ok(())
    }
}

/// A fixed-width histogram of `buffered_ms`.
///
/// Keeping every sample so a median can be taken at the end would be a leak
/// with a nicer name: a feed lands every few tens of milliseconds, so a
/// two-hour meeting is hundreds of thousands of samples accumulated to compute
/// one number read once. Buckets give the same answer to a resolution nobody
/// will act on differently, in memory that does not depend on meeting length.
struct BufferedMsHistogram {
    buckets: [u32; BUCKETS],
    count: u64,
}

impl BufferedMsHistogram {
    fn new() -> Self {
        Self { buckets: [0; BUCKETS], count: 0 }
    }

    fn add(&mut self, ms: i64) {
        // Everything past 30s collapses into the last bucket. By then the exact
        // figure has stopped mattering and only "hopelessly behind" does.
        let bucket = (ms.max(0) / BUCKET_MS).min(BUCKETS as i64 - 1) as usize;
        self.buckets[bucket] += 1;
        self.count += 1;
    }

    /// Median to the nearest bucket. `None` before any sample, rather than a
    /// made-up zero that would read as "perfectly keeping up".
    fn median_ms(&self) -> Option<i64> {
        if self.count == 0 {
            return None;
        }
        let half = self.count.div_ceil(2);
        let mut seen = 0u64;
        for (bucket, &n) in self.buckets.iter().enumerate() {
            seen += u64::from(n);
            if seen >= half {
                return Some(bucket as i64 * BUCKET_MS);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_new_tail_is_emitted() {
        let mut cursor = CommittedCursor::default();

        let first = cursor.advance("Hello there", 1.5).expect("the first text is all new");
        assert_eq!(first.text, "Hello there");
        assert_eq!(first.audio_start, 0.0);
        assert_eq!(first.audio_end, 1.5);

        let second = cursor.advance("Hello there world", 3.0).expect("the tail is new");
        assert_eq!(second.text, "world", "the already-emitted prefix must not be sent twice");
        assert_eq!(second.audio_start, 1.5, "a chunk starts where the last one ended");
        assert_eq!(second.audio_end, 3.0);

        assert!(
            cursor.advance("Hello there world", 4.5).is_none(),
            "unchanged text owes nothing, however often it is looked at"
        );
    }

    /// The guard against reintroducing a growing-prefix preview loop — the
    /// design that re-decodes and re-emits the whole session every tick. It
    /// looks harmless on a ten-second demo and is ~180x real-time compute at
    /// meeting length. A comment would not have stopped it; this does.
    #[test]
    fn emitting_is_proportional_to_the_delta_not_the_session() {
        let mut cursor = CommittedCursor::default();
        let mut committed = String::new();
        let mut emitted_bytes = 0usize;
        let mut longest_chunk = 0usize;

        for i in 0..500 {
            committed.push_str(&format!("word{i} "));
            let chunk = cursor.advance(&committed, i as f64).expect("each tick adds text");
            emitted_bytes += chunk.text.len();
            longest_chunk = longest_chunk.max(chunk.text.len());
        }

        // Every delta is "wordN " and loses only its trailing space to the trim,
        // so the session emits the transcript once — not a prefix 500 times.
        assert_eq!(emitted_bytes, committed.len() - 500);

        assert!(
            longest_chunk <= "word499".len(),
            "no chunk may grow with the session; the longest was {longest_chunk} bytes"
        );

        // What a re-emit-the-whole-prefix loop would have cost over the same
        // 500 ticks. The gap is already an order of magnitude here and widens
        // with every further commit, because one side is linear and the other
        // is not.
        let growing_prefix_bytes: usize = {
            let mut prefix = String::new();
            (0..500)
                .map(|i| {
                    prefix.push_str(&format!("word{i} "));
                    prefix.trim().len()
                })
                .sum()
        };
        assert!(
            emitted_bytes * 10 < growing_prefix_bytes,
            "emitted {emitted_bytes} bytes against a growing prefix's {growing_prefix_bytes}"
        );
    }

    #[test]
    fn no_samples_means_no_median_rather_than_a_confident_zero() {
        assert_eq!(BufferedMsHistogram::new().median_ms(), None);
    }

    #[test]
    fn the_median_lands_in_the_bucket_holding_the_middle_sample() {
        let mut histogram = BufferedMsHistogram::new();
        for _ in 0..30 {
            histogram.add(150);
        }
        for _ in 0..70 {
            histogram.add(2_540);
        }
        assert_eq!(
            histogram.median_ms(),
            Some(2_500),
            "70 of 100 samples sit in the 2.5s bucket, so the median is there"
        );
    }

    #[test]
    fn a_long_meeting_costs_no_more_memory_than_a_short_one() {
        let mut histogram = BufferedMsHistogram::new();
        // Two hours of feeds arriving every 50ms — the volume that makes a
        // kept-every-sample implementation a leak.
        for i in 0..144_000 {
            histogram.add(if i % 2 == 0 { 200 } else { 900 });
        }
        assert_eq!(histogram.count, 144_000);
        assert_eq!(histogram.median_ms(), Some(200));
    }

    #[test]
    fn a_hopelessly_late_decoder_lands_in_the_overflow_bucket() {
        let mut histogram = BufferedMsHistogram::new();
        histogram.add(10 * 60 * 1_000);
        assert_eq!(histogram.median_ms(), Some(30_000), "clamped, not lost or panicking");
    }
}
