// audio/transcription/adapters/bench_sink.rs
//
// A measuring decorator on the driven side of the hexagon.
//
// The question it exists to answer is "how far behind the speaker is the
// transcript?", and nothing else in the app can answer it. A decoder knows how
// long its own decode took; a sink knows what text arrived. Only the pair of
// (wall clock, audio position) captured at the instant text is committed gives
// the lag a user actually experiences.
//
// A decorator rather than a macro sprinkled through the adapters, because every
// backend commits through a `TranscriptSink`. Wrapping the sink at the two
// composition points in `transcription/mod.rs` therefore instruments the
// streaming path, the local batch path and the sidecar audio-LLM path at once,
// and none of the three adapters has to know that measurement exists.

use crate::audio::transcription::ports::{TranscriptChunk, TranscriptSink};
use log::info;
use std::time::{Duration, Instant};

/// How often a `BENCH` line is allowed out, after the first one.
///
/// Commits arrive every couple of seconds on a streaming model, and the point
/// of the line is a trend over a meeting, not a running commentary.
const LOG_EVERY: Duration = Duration::from_secs(15);

/// Wraps a sink and reports how far behind real time the transcript is.
pub struct BenchSink<S> {
    inner: S,
    /// Monotonic, never wall clock. A meeting can outlive an NTP correction or
    /// a DST jump, and a lag measured against a clock that stepped backwards is
    /// worse than no measurement at all.
    started: Instant,
    commits: u64,
    last_log: Instant,
}

impl<S: TranscriptSink> BenchSink<S> {
    pub fn new(inner: S) -> Self {
        let started = Instant::now();
        Self { inner, started, commits: 0, last_log: started }
    }

    /// Whether this commit gets a log line.
    ///
    /// Deliberately checked *before* any formatting: this runs on the decode
    /// thread for every committed chunk, and building a string only to throw it
    /// away is the entire cost of instrumenting a hot path.
    fn due(&mut self, now: Instant) -> bool {
        // The first commit always reports — it is the one that tells you the
        // pipeline produced anything at all, and waiting 15s for that is how a
        // silent failure gets mistaken for a slow model.
        if self.commits == 1 || now.duration_since(self.last_log) >= LOG_EVERY {
            self.last_log = now;
            true
        } else {
            false
        }
    }
}

/// Milliseconds of wall clock between the audio being spoken and its text
/// arriving.
///
/// Negative is possible and meaningful: a decoder handed a backlog at the start
/// of a stream can chew through it faster than real time, so its audio position
/// runs ahead of the clock measured from the first sample fed.
fn lag_ms(elapsed: Duration, audio_end_secs: f64) -> f64 {
    (elapsed.as_secs_f64() - audio_end_secs) * 1000.0
}

impl<S: TranscriptSink> TranscriptSink for BenchSink<S> {
    fn committed(&mut self, chunk: TranscriptChunk) {
        self.commits += 1;
        let now = Instant::now();

        if self.due(now) {
            let elapsed = now.duration_since(self.started);
            info!(
                "BENCH n={} t={:.1}s audio_end={:.1}s lag_ms={:.0} chars={}",
                self.commits,
                elapsed.as_secs_f64(),
                chunk.audio_end,
                lag_ms(elapsed, chunk.audio_end),
                chunk.text.chars().count(),
            );
        }

        self.inner.committed(chunk);
    }

    fn tentative(&mut self, text: &str) {
        // Never logged, on purpose. Tentative text changes on nearly every
        // feed — several times a second — so a line here would bury the commit
        // lines it exists to make readable, and cost more than what it measures.
        self.inner.tentative(text);
    }

    fn warn(&mut self, message: &str) {
        self.inner.warn(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::transcription::service::tests::FakeSink;

    fn chunk(text: &str, audio_end: f64) -> TranscriptChunk {
        TranscriptChunk {
            text: text.to_string(),
            audio_start: (audio_end - 1.0).max(0.0),
            audio_end,
            confidence: None,
        }
    }

    #[test]
    fn measuring_does_not_change_what_the_sink_is_told() {
        let inner = FakeSink::default();
        let recorded = inner.0.clone();
        let mut bench = BenchSink::new(inner);

        bench.committed(chunk("hello", 1.0));
        bench.tentative("half a wo");
        bench.committed(chunk("world", 2.0));
        bench.warn("something slipped");
        bench.tentative("");

        let log = recorded.lock().unwrap();
        assert_eq!(log.committed, vec!["hello", "world"]);
        assert_eq!(log.tentative, vec!["half a wo", ""]);
        assert_eq!(log.warnings, vec!["something slipped"]);
    }

    #[test]
    fn lag_is_the_clock_minus_the_audio_position() {
        // 12s of wall clock have passed and the text just committed describes
        // audio up to the 10s mark: two seconds behind the speaker.
        assert_eq!(lag_ms(Duration::from_secs(12), 10.0), 2_000.0);

        // A decoder faster than real time reports negative lag rather than
        // clamping to zero, because "ahead" is a real state worth seeing.
        assert_eq!(lag_ms(Duration::from_secs(3), 5.0), -2_000.0);
    }

    #[test]
    fn the_first_commit_reports_and_then_at_most_one_line_every_15s() {
        let mut bench = BenchSink::new(FakeSink::default());
        let t0 = bench.started;

        bench.commits = 1;
        assert!(bench.due(t0), "the first commit always reports");

        bench.commits = 2;
        assert!(!bench.due(t0 + Duration::from_secs(14)), "14s is inside the window");
        assert!(bench.due(t0 + Duration::from_secs(15)), "15s reopens it");
        assert!(!bench.due(t0 + Duration::from_secs(29)), "window restarts from the last line");
        assert!(bench.due(t0 + Duration::from_secs(30)));
    }
}
