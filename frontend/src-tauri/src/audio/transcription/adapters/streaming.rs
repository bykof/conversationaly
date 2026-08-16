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
    /// Byte offset into `committed` already sent on. Committed text is
    /// append-only, so this only moves forward and the tail past it is exactly
    /// what is new.
    emitted_len: usize,
    /// Audio position (seconds) that `emitted_len` corresponds to.
    emitted_audio_secs: f64,
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
            emitted_len: 0,
            emitted_audio_secs: 0.0,
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
        if committed.len() <= self.emitted_len {
            return;
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
            return;
        };

        let text = new_text.trim().to_string();
        self.emitted_len = committed.len();

        let audio_end = audio_committed_ms as f64 / 1000.0;
        let audio_start = std::mem::replace(&mut self.emitted_audio_secs, audio_end);

        if text.is_empty() {
            return;
        }

        self.commits += 1;
        sink.committed(TranscriptChunk {
            text,
            audio_start,
            audio_end,
            // No confidence on this path, deliberately. The only way to get one
            // is `Stream::snapshot()`, which materialises the ENTIRE session —
            // every segment, word and token as an owned String — to average it
            // and throw it away. Doing that per commit, on this thread, over a
            // transcript that grows all meeting, is why the live transcript
            // used to fall further behind the longer a meeting ran. What it
            // bought was a running mean over the whole session, so every line
            // carried the same number anyway.
            confidence: None,
        });
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
