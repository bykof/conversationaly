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

use crate::audio::common::{
    split_segment_at_silence, DIARIZED_MAX_SEGMENT_SAMPLES, LIVE_MAX_SEGMENT_SAMPLES,
};
use crate::audio::transcription::ports::{Transcriber, TranscriptChunk, TranscriptSink};
use crate::audio::vad::{ContinuousVadProcessor, SpeechSegment};
use crate::transcribe_engine::{keep_partial_on_truncation, mean_token_confidence, speaker_turns};
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

const LEVELS_SLACK_SAMPLES: usize = 5 * SAMPLE_RATE as usize;

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

pub struct DecodedTurn {
    pub text: String,
    pub speaker_id: i32,
    pub start_ms: f64,
    pub end_ms: f64,
    pub confidence: Option<f32>,
}

impl Decoder {
    fn decode(&mut self, samples: &[f32]) -> Result<Vec<DecodedTurn>> {
        match self {
            Decoder::Local { session, run_options } => {
                let transcript = keep_partial_on_truncation(session.run(samples, run_options))?;
                let confidence = Some(mean_token_confidence(&transcript));

                let turns = speaker_turns(&transcript);
                if !turns.is_empty() {
                    return Ok(turns
                        .into_iter()
                        .map(|t| DecodedTurn {
                            text: t.text,
                            speaker_id: t.speaker_id,
                            start_ms: t.start_ms,
                            end_ms: t.end_ms,
                            confidence,
                        })
                        .collect());
                }

                let text = transcript.text.trim().to_string();
                if text.is_empty() {
                    return Ok(vec![]);
                }
                Ok(vec![DecodedTurn {
                    text,
                    speaker_id: 0,
                    start_ms: 0.0,
                    end_ms: 0.0,
                    confidence,
                }])
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
                    return Ok(vec![]);
                }
                // A chat completion carries no token probabilities.
                Ok(vec![DecodedTurn {
                    text,
                    speaker_id: 0,
                    start_ms: 0.0,
                    end_ms: 0.0,
                    confidence: None,
                }])
            }
        }
    }
}

#[derive(Default)]
struct SourceLevels {
    entries: VecDeque<(f64, f32, f32)>,
}

impl SourceLevels {
    fn note(&mut self, start_s: f64, mic_rms: f32, sys_rms: f32) {
        self.entries.push_back((start_s, mic_rms, sys_rms));
        let cutoff = start_s - (MAX_BACKLOG_SAMPLES + LEVELS_SLACK_SAMPLES) as f64
            / SAMPLE_RATE as f64;
        while self.entries.front().is_some_and(|&(t, _, _)| t < cutoff) {
            self.entries.pop_front();
        }
    }

    fn dominant(&self, start_s: f64, end_s: f64) -> Option<Side> {
        let (mic, sys) = self
            .entries
            .iter()
            .filter(|&&(t, _, _)| t >= start_s && t < end_s)
            .fold((0.0f64, 0.0f64), |(m, s), &(_, mic, sys)| {
                (m + mic as f64, s + sys as f64)
            });

        if mic <= 0.0 && sys <= 0.0 {
            return None;
        }
        Some(if mic > sys { Side::Mic } else { Side::System })
    }
}

#[derive(Debug, PartialEq)]
enum Side {
    Mic,
    System,
}

fn attribute(
    levels: &SourceLevels,
    owns_its_span: bool,
    start: f64,
    end: f64,
    speaker_id: i32,
) -> Option<String> {
    let side = owns_its_span.then(|| levels.dominant(start, end)).flatten();
    label(side, speaker_id)
}

fn label(side: Option<Side>, speaker_id: i32) -> Option<String> {
    match side {
        Some(Side::Mic) => Some("you".to_string()),
        _ if speaker_id > 0 => Some(speaker_id.to_string()),
        _ => None,
    }
}

pub struct SegmentedTranscriber {
    decoder: Decoder,
    vad: ContinuousVadProcessor,
    pending: VecDeque<SpeechSegment>,
    backlog_warned: bool,
    max_segment_samples: usize,
    attribute_speakers: bool,
    levels: SourceLevels,
}

impl SegmentedTranscriber {
    pub fn new(decoder: Decoder) -> Result<Self> {
        Self::with_attribution(decoder, false)
    }

    pub fn with_attribution(decoder: Decoder, attribute_speakers: bool) -> Result<Self> {
        Ok(Self {
            decoder,
            vad: ContinuousVadProcessor::new(SAMPLE_RATE, VAD_REDEMPTION_MS)?,
            pending: VecDeque::new(),
            backlog_warned: false,
            max_segment_samples: if attribute_speakers {
                DIARIZED_MAX_SEGMENT_SAMPLES
            } else {
                LIVE_MAX_SEGMENT_SAMPLES
            },
            attribute_speakers,
            levels: SourceLevels::default(),
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
            let segment_start = segment.start_timestamp_ms / 1000.0;
            let segment_end = segment.end_timestamp_ms / 1000.0;

            match self.decoder.decode(&segment.samples) {
                Ok(turns) => {
                    let alone = turns.len() == 1;
                    for turn in turns {
                        let timed = turn.end_ms > turn.start_ms;
                        let (start, end) = if timed {
                            (
                                segment_start + turn.start_ms / 1000.0,
                                segment_start + turn.end_ms / 1000.0,
                            )
                        } else {
                            (segment_start, segment_end)
                        };

                        let speaker = self.attribute_speakers.then(|| {
                            attribute(&self.levels, timed || alone, start, end, turn.speaker_id)
                        });

                        sink.committed(TranscriptChunk {
                            text: turn.text,
                            audio_start: start,
                            audio_end: end,
                            confidence: turn.confidence,
                            speaker: speaker.flatten(),
                        });
                    }
                }
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
            Ok(segments) => enqueue(segments, &mut self.pending, self.max_segment_samples),
            Err(e) => warn!("VAD processing failed: {e}"),
        }
        self.drain(sink);
        Ok(())
    }

    fn finish(&mut self, sink: &mut dyn TranscriptSink) -> Result<()> {
        match self.vad.flush() {
            Ok(segments) => enqueue(segments, &mut self.pending, self.max_segment_samples),
            Err(e) => warn!("VAD flush failed: {e}"),
        }
        self.drain(sink);
        Ok(())
    }

    fn note_levels(&mut self, start_s: f64, mic_rms: f32, sys_rms: f32) {
        if self.attribute_speakers {
            self.levels.note(start_s, mic_rms, sys_rms);
        }
    }
}

/// Queue segments, splitting any too long for a single decode.
///
/// The cap is what the speaker experiences as latency: one decode must not be
/// allowed to hold the whole transcript hostage.
fn enqueue(segments: Vec<SpeechSegment>, pending: &mut VecDeque<SpeechSegment>, cap: usize) {
    for segment in segments {
        if segment.samples.len() > cap {
            pending.extend(split_segment_at_silence(&segment, cap));
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

    fn levels(entries: &[(f64, f32, f32)]) -> SourceLevels {
        let mut l = SourceLevels::default();
        for &(t, mic, sys) in entries {
            l.note(t, mic, sys);
        }
        l
    }

    #[test]
    fn the_microphone_side_outranks_the_model() {
        let l = levels(&[(0.0, 0.4, 0.01), (1.0, 0.5, 0.02)]);
        assert_eq!(l.dominant(0.0, 2.0), Some(Side::Mic));
        assert_eq!(
            label(l.dominant(0.0, 2.0), 3),
            Some("you".to_string()),
            "the mic is known to be this user; a cluster id is only a guess"
        );
    }

    #[test]
    fn the_far_side_keeps_the_models_cluster_id() {
        let l = levels(&[(0.0, 0.01, 0.6)]);
        assert_eq!(l.dominant(0.0, 1.0), Some(Side::System));
        assert_eq!(label(l.dominant(0.0, 1.0), 2), Some("2".to_string()));
    }

    #[test]
    fn unattributed_far_side_speech_gets_no_label() {
        let l = levels(&[(0.0, 0.01, 0.6)]);
        assert_eq!(
            label(l.dominant(0.0, 1.0), 0),
            None,
            "a row nothing attributed must not render a prefix claiming otherwise"
        );
    }

    #[test]
    fn a_turn_only_weighs_the_levels_inside_its_own_span() {
        let l = levels(&[(0.0, 0.9, 0.0), (1.0, 0.9, 0.0), (2.0, 0.0, 0.5), (3.0, 0.0, 0.5)]);
        assert_eq!(l.dominant(2.0, 4.0), Some(Side::System));
        assert_eq!(label(l.dominant(2.0, 4.0), 1), Some("1".to_string()));
    }

    #[test]
    fn untimed_turns_do_not_inherit_one_loudness_verdict() {
        // Granite attributes speakers but reports no turn timing, so several
        // turns land on one span. The mic being louder across it says nothing
        // about which of them the mic owner spoke.
        let l = levels(&[(0.0, 0.9, 0.2), (1.0, 0.9, 0.2)]);

        assert_eq!(
            attribute(&l, false, 0.0, 2.0, 2),
            Some("2".to_string()),
            "a shared span must not stamp the mic owner onto another speaker"
        );
        assert_eq!(
            attribute(&l, true, 0.0, 2.0, 2),
            Some("you".to_string()),
            "a turn that owns its span is still resolved by loudness"
        );
    }

    #[test]
    fn silence_on_both_sides_falls_through_to_the_model() {
        let l = levels(&[(0.0, 0.0, 0.0)]);
        assert_eq!(l.dominant(0.0, 1.0), None);
        assert_eq!(label(None, 2), Some("2".to_string()));
        assert_eq!(label(None, 0), None);
    }

    #[test]
    fn level_history_does_not_grow_for_the_whole_meeting() {
        let mut l = SourceLevels::default();
        for i in 0..4000 {
            l.note(i as f64, 0.1, 0.1);
        }
        let span = (MAX_BACKLOG_SAMPLES + LEVELS_SLACK_SAMPLES) / SAMPLE_RATE as usize;
        assert!(
            l.entries.len() <= span + 1,
            "kept {} entries for a {}s window",
            l.entries.len(),
            span
        );
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
        enqueue(vec![segment(40.0, 0.0)], &mut pending, LIVE_MAX_SEGMENT_SAMPLES);

        assert!(pending.len() > 1, "a 40s utterance must be split, got {}", pending.len());
        assert!(
            pending.iter().all(|s| s.samples.len() <= LIVE_MAX_SEGMENT_SAMPLES * 2),
            "no sub-segment should be wildly past the cap"
        );
    }

    #[test]
    fn short_segments_pass_through_unsplit() {
        let mut pending = VecDeque::new();
        enqueue(vec![segment(3.0, 0.0)], &mut pending, LIVE_MAX_SEGMENT_SAMPLES);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].samples.len(), 3 * SAMPLE_RATE as usize);
    }
}
