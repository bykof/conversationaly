// audio/transcription/adapters/segmented.rs
//
// Driving adapter for models that cannot stream: speech is cut into segments by
// VAD and each one is decoded whole.
//
// There is no tentative text on this path, so it is silent between utterances,
// and its latency floor is the segment length. Two decoders sit behind one
// adapter because segmentation and backlog policy are identical between them —
// only the call that turns samples into a string differs:
//   - `Decoder::Local`    — `Session::run()`, for the batch-only catalog
//                           families (whisper, canary, qwen3-asr, ...).
//   - `Decoder::AudioLlm` — one llama-helper sidecar request per segment, for
//                           audio-capable LLMs (Gemma 4 E2B/E4B).
//
// Single-threaded on purpose: transcribe.cpp allows one in-flight compute per
// Model, so a second concurrent `run()` fails with `Error::Busy`. The sidecar
// has the same constraint for a different reason — one process, one loaded
// model.

use crate::audio::common::{split_segment_at_silence, LIVE_MAX_SEGMENT_SAMPLES};
use crate::audio::transcription::ports::{Transcriber, TranscriptChunk, TranscriptSink};
use crate::audio::vad::{ContinuousVadProcessor, SpeechSegment};
use crate::transcribe_engine::{keep_partial_on_truncation, mean_token_confidence};
use anyhow::Result;
use log::warn;
use std::collections::VecDeque;
use transcribe_cpp::{RunOptions, Session};

/// Pipeline audio is already mono 16kHz, which is what the VAD and every model
/// want.
const SAMPLE_RATE: u32 = 16_000;

/// How long VAD waits through a pause before closing a segment. Matches the
/// import path so live and file transcripts segment the same way.
const VAD_REDEMPTION_MS: u32 = 2_000;

/// Most un-transcribed audio to hold before dropping the oldest segments. A
/// model slower than real time otherwise grows this queue for the whole
/// meeting, and the transcript falls further behind the longer it runs.
const MAX_BACKLOG_SAMPLES: usize = 30 * SAMPLE_RATE as usize;

/// How a segment becomes text.
pub enum Decoder {
    /// Local GGUF through transcribe.cpp.
    Local { session: Session, run_options: RunOptions },
    /// Audio-capable LLM in the built-in sidecar.
    AudioLlm {
        handle: tokio::runtime::Handle,
        app_data_dir: std::path::PathBuf,
        model: String,
    },
}

impl Decoder {
    /// Decode one segment. `Ok(None)` means nothing intelligible, which is
    /// normal for noise and must not become an empty transcript row.
    fn decode(&mut self, samples: &[f32]) -> Result<Option<(String, Option<f32>)>> {
        match self {
            Decoder::Local { session, run_options } => {
                let transcript = keep_partial_on_truncation(session.run(samples, run_options))?;
                let text = transcript.text.trim().to_string();
                if text.is_empty() {
                    return Ok(None);
                }
                Ok(Some((text, Some(mean_token_confidence(&transcript)))))
            }
            Decoder::AudioLlm { handle, app_data_dir, model } => {
                // The sidecar call is async and this adapter is driven from a
                // blocking thread, which is exactly where block_on is legal.
                let text = handle.block_on(
                    crate::summary::summary_engine::client::transcribe_with_builtin(
                        app_data_dir,
                        model,
                        samples,
                    ),
                )?;
                if text.is_empty() {
                    return Ok(None);
                }
                // A chat completion carries no token probabilities.
                Ok(Some((text, None)))
            }
        }
    }
}

pub struct SegmentedTranscriber {
    decoder: Decoder,
    vad: ContinuousVadProcessor,
    pending: VecDeque<SpeechSegment>,
    backlog_warned: bool,
}

impl SegmentedTranscriber {
    pub fn new(decoder: Decoder) -> Result<Self> {
        Ok(Self {
            decoder,
            vad: ContinuousVadProcessor::new(SAMPLE_RATE, VAD_REDEMPTION_MS)?,
            pending: VecDeque::new(),
            backlog_warned: false,
        })
    }

    /// Decode everything queued, shedding backlog first if the decoder has
    /// fallen too far behind to catch up.
    fn drain(&mut self, sink: &mut dyn TranscriptSink) {
        let dropped = trim_backlog(&mut self.pending);
        if dropped > 0 {
            let secs = dropped as f64 / SAMPLE_RATE as f64;
            warn!(
                "Transcription is behind real time; dropped {secs:.1}s of audio to stay \
                 within the {}s backlog cap",
                MAX_BACKLOG_SAMPLES / SAMPLE_RATE as usize
            );
            // Once per recording: this fires repeatedly on a slow model, and
            // the point is to tell the user to switch models, not to bury the
            // UI in toasts.
            if !self.backlog_warned {
                self.backlog_warned = true;
                sink.warn(&format!(
                    "This model is transcribing slower than you are speaking, so some audio \
                     is being skipped. Pick a faster or streaming model in settings. \
                     ({secs:.0}s skipped so far)"
                ));
            }
        }

        while let Some(segment) = self.pending.pop_front() {
            match self.decoder.decode(&segment.samples) {
                // Nothing intelligible in that segment; normal for noise.
                Ok(None) => {}
                Ok(Some((text, confidence))) => sink.committed(TranscriptChunk {
                    text,
                    audio_start: segment.start_timestamp_ms / 1000.0,
                    audio_end: segment.end_timestamp_ms / 1000.0,
                    confidence,
                }),
                Err(e) => {
                    warn!("Batch transcription of a segment failed: {e}");
                    sink.warn(&e.to_string());
                }
            }
        }
    }
}

impl Transcriber for SegmentedTranscriber {
    fn feed(&mut self, pcm_16k: &[f32], sink: &mut dyn TranscriptSink) -> Result<()> {
        // Losing a chunk to VAD is recoverable; ending the meeting's transcript
        // over it is not.
        match self.vad.process_audio(pcm_16k) {
            Ok(segments) => enqueue(segments, &mut self.pending),
            Err(e) => warn!("VAD processing failed: {e}"),
        }
        self.drain(sink);
        Ok(())
    }

    fn finish(&mut self, sink: &mut dyn TranscriptSink) -> Result<()> {
        match self.vad.flush() {
            Ok(segments) => enqueue(segments, &mut self.pending),
            Err(e) => warn!("VAD flush failed: {e}"),
        }
        self.drain(sink);
        Ok(())
    }
}

/// Queue segments, splitting any too long for a single decode.
///
/// The cap is what the speaker experiences as latency: one decode must not be
/// allowed to hold the whole transcript hostage.
fn enqueue(segments: Vec<SpeechSegment>, pending: &mut VecDeque<SpeechSegment>) {
    for segment in segments {
        if segment.samples.len() > LIVE_MAX_SEGMENT_SAMPLES {
            pending.extend(split_segment_at_silence(&segment, LIVE_MAX_SEGMENT_SAMPLES));
        } else {
            pending.push_back(segment);
        }
    }
}

/// Drop the oldest segments until the queue fits the budget, returning how many
/// samples were discarded. Pure, so the policy is testable on its own.
fn trim_backlog(pending: &mut VecDeque<SpeechSegment>) -> usize {
    let mut backlog: usize = pending.iter().map(|s| s.samples.len()).sum();
    let mut dropped = 0usize;
    // Drop from the front: the newest speech is the part still worth showing.
    while backlog > MAX_BACKLOG_SAMPLES {
        let Some(segment) = pending.pop_front() else { break };
        backlog -= segment.samples.len();
        dropped += segment.samples.len();
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(secs: f64, start_ms: f64) -> SpeechSegment {
        SpeechSegment {
            samples: vec![0.0; (secs * SAMPLE_RATE as f64) as usize],
            start_timestamp_ms: start_ms,
            end_timestamp_ms: start_ms + secs * 1000.0,
            confidence: 1.0,
        }
    }

    #[test]
    fn backlog_under_budget_is_left_alone() {
        let mut pending: VecDeque<_> = (0..3).map(|i| segment(5.0, i as f64 * 5000.0)).collect();
        assert_eq!(trim_backlog(&mut pending), 0);
        assert_eq!(pending.len(), 3, "15s of audio is inside the 30s budget");
    }

    #[test]
    fn backlog_over_budget_drops_oldest_until_it_fits() {
        // 10 x 5s = 50s queued against a 30s cap.
        let mut pending: VecDeque<_> = (0..10).map(|i| segment(5.0, i as f64 * 5000.0)).collect();

        let dropped = trim_backlog(&mut pending);

        assert_eq!(dropped, 20 * SAMPLE_RATE as usize, "should shed exactly 20s");
        let remaining: usize = pending.iter().map(|s| s.samples.len()).sum();
        assert!(remaining <= MAX_BACKLOG_SAMPLES, "still over budget: {remaining}");
        assert_eq!(
            pending.front().unwrap().start_timestamp_ms,
            20_000.0,
            "the oldest speech must be what goes, not the newest"
        );
    }

    /// A single over-long segment cannot be trimmed to fit without discarding
    /// everything, so enqueue() has to have split it before it gets here.
    /// A single over-long segment cannot be trimmed to fit without discarding
    /// everything, so enqueue() has to have split it before it gets here.
    #[test]
    fn a_long_utterance_is_split_before_it_can_starve_the_backlog() {
        let mut pending = VecDeque::new();
        enqueue(vec![segment(40.0, 0.0)], &mut pending);

        assert!(pending.len() > 1, "a 40s utterance must be split, got {}", pending.len());
        assert!(
            pending.iter().all(|s| s.samples.len() <= LIVE_MAX_SEGMENT_SAMPLES * 2),
            "no sub-segment should be wildly past the cap"
        );
    }

    #[test]
    fn short_segments_pass_through_unsplit() {
        let mut pending = VecDeque::new();
        enqueue(vec![segment(3.0, 0.0)], &mut pending);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].samples.len(), 3 * SAMPLE_RATE as usize);
    }
}
