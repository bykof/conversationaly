use anyhow::{anyhow, Result};
use silero_rs::{VadConfig, VadSession, VadTransition};
use log::{debug, info, warn};
use std::collections::VecDeque;
use std::time::Duration;

/// Represents a complete speech segment detected by VAD
#[derive(Debug, Clone)]
pub struct SpeechSegment {
    pub samples: Vec<f32>,
    pub start_timestamp_ms: f64,
    pub end_timestamp_ms: f64,
    pub confidence: f32,
}

/// Processes audio in 30ms chunks but returns complete speech segments
pub struct ContinuousVadProcessor {
    session: VadSession,
    chunk_size: usize,
    buffer: Vec<f32>,
    speech_segments: VecDeque<SpeechSegment>,
    current_speech: Vec<f32>,
    in_speech: bool,
    processed_samples: usize,
    /// Set once this utterance has been cut short by [`MAX_UTTERANCE_SAMPLES`].
    /// The VAD's own SpeechEnd carries the *whole* utterance, so after a cut only
    /// the tail we accumulated since is new text.
    mid_utterance_cut: bool,
    // State tracking for smart logging
    last_logged_state: bool,
}

/// Longest unbroken speech that can accumulate before a segment is cut anyway.
///
/// A segment normally closes on silence (`redemption_time`), which means a
/// speaker who never pauses produces nothing at all: the live batch path decodes
/// whole segments, so the transcript would stay empty until they stopped talking
/// and then arrive in one lump — most of it discarded by the backlog cap.
const MAX_UTTERANCE_SAMPLES: usize = crate::audio::common::LIVE_MAX_SEGMENT_SAMPLES;

/// Upper bound on a VAD frame, so a frame can live on the stack. The frame is
/// 30ms at 16kHz (480 samples); this leaves room without being a real limit.
const MAX_CHUNK_SAMPLES: usize = 1024;

/// How much audio to keep in front of speech that has not been reported yet.
///
/// Silero cannot announce speech until it has heard `min_speech_time` (250ms)
/// of it, and its own segments are then backdated by `pre_speech_pad` (300ms).
/// Anything we build ourselves has to hold that much history or it starts
/// mid-word — which is what happened to every force-cut and flushed segment.
const PRE_ROLL_SAMPLES: usize = 9600; // 600ms @ 16kHz

impl ContinuousVadProcessor {
    pub fn new(input_sample_rate: u32, redemption_time_ms: u32) -> Result<Self> {
        // Silero VAD MUST use 16kHz - this is hardcoded requirement
        const VAD_SAMPLE_RATE: u32 = 16000;

        // Use STRICT settings to prevent silence from reaching Whisper
        let mut config = VadConfig::default();
        config.sample_rate = VAD_SAMPLE_RATE as usize;

        // CONTINUOUS SPEECH FIX: Tuned for capturing complete 5+ second utterances
        // Previous: 0.55/0.40 with 400ms redemption was fragmenting speech into 40ms segments
        // New: More lenient thresholds + longer redemption for continuous speech
        config.positive_speech_threshold = 0.50;  // Silero default - good for continuous speech
        config.negative_speech_threshold = 0.35;  // Silero default - allows natural pauses

        // CRITICAL FIX: Removed redemption_time capping to support long continuous speech
        // Previous: capped at 400ms, causing VAD to fragment 5-second speech into 40ms segments
        // New: Use full redemption_time from pipeline (2000ms) to bridge natural pauses
        config.redemption_time = Duration::from_millis(redemption_time_ms as u64);
        config.pre_speech_pad = Duration::from_millis(300);   // Pre-speech padding for context
        config.post_speech_pad = Duration::from_millis(400);  // Increased: more context at end

        // CRITICAL FIX: Increased min_speech_time to prevent tiny 40ms fragments
        // Previous: 100ms allowed too-short segments that Whisper rejects
        // New: 250ms ensures segments are substantial enough for Whisper (>100ms requirement)
        config.min_speech_time = Duration::from_millis(250);  // Prevent tiny fragments

        debug!("Creating VAD session with: sample_rate={}Hz, redemption={}ms, min_speech={}ms, input_rate={}Hz",
               VAD_SAMPLE_RATE, redemption_time_ms, 250, input_sample_rate);

        let session = VadSession::new(config)
            .map_err(|e| anyhow!("Failed to create VAD session: {:?}", e))?;

        // VAD uses 30ms chunks at 16kHz (480 samples)
        let vad_chunk_size = (VAD_SAMPLE_RATE as f32 * 0.03) as usize; // 480 samples

        info!("VAD processor created: input={}Hz, vad={}Hz, chunk_size={} samples",
              input_sample_rate, VAD_SAMPLE_RATE, vad_chunk_size);

        Ok(Self {
            session,
            chunk_size: vad_chunk_size,
            buffer: Vec::with_capacity(vad_chunk_size * 2),
            speech_segments: VecDeque::new(),
            current_speech: Vec::new(),
            in_speech: false,
            processed_samples: 0,
            mid_utterance_cut: false,
            // Initialize state tracking
            last_logged_state: false,
        })
    }

    /// Process incoming 16kHz audio and return any complete speech segments.
    ///
    /// Input must already be 16kHz — every caller resamples before this point,
    /// so the resampling branch that used to live here was dead code wrapped
    /// around the app's worst resampler.
    pub fn process_audio(&mut self, samples: &[f32]) -> Result<Vec<SpeechSegment>> {
        self.buffer.extend_from_slice(samples);

        // Consume whole frames, then drop them in one go. Draining 480 samples
        // off the front per frame memmoved the remaining buffer every time and
        // allocated a Vec per 30ms of audio.
        let consumable = self.buffer.len() - self.buffer.len() % self.chunk_size;
        for start in (0..consumable).step_by(self.chunk_size) {
            // Copied out because process_chunk needs &mut self. One fixed-size
            // frame on the stack, not a heap allocation per frame.
            let mut frame = [0.0f32; MAX_CHUNK_SAMPLES];
            let frame = &mut frame[..self.chunk_size];
            frame.copy_from_slice(&self.buffer[start..start + self.chunk_size]);
            // Silero rejects a frame outright if any sample is outside [-1, 1],
            // and we drop the whole 30ms when it does. Every producer can
            // overshoot: sinc resampling rings past the peak it was handed (the
            // live mixer clamps to exactly 1.0, so its output is the worst
            // case), and imported float WAVs are not bounded at all.
            for sample in frame.iter_mut() {
                *sample = sample.clamp(-1.0, 1.0);
            }
            self.process_chunk(frame)?;
        }
        self.buffer.drain(..consumable);

        Ok(self.speech_segments.drain(..).collect())
    }

    /// Flush any remaining audio and return final speech segments
    pub fn flush(&mut self) -> Result<Vec<SpeechSegment>> {
        debug!("VAD flush: in_speech={}, current_speech_len={}, buffer_len={}, speech_segments_queued={}",
              self.in_speech, self.current_speech.len(), self.buffer.len(), self.speech_segments.len());

        // Carry the sub-frame remainder into the final segment rather than
        // zero-padding it up to a frame and running Silero on it. Padding
        // injected up to 30ms of manufactured silence into the emitted audio
        // and pushed processed_samples (and so every timestamp after it) past
        // the real end of the recording. Silero's verdict on a partial frame is
        // worth nothing here anyway — we are force-ending regardless.
        if !self.buffer.is_empty() {
            self.processed_samples += self.buffer.len();
            self.current_speech.extend_from_slice(&self.buffer);
            self.buffer.clear();
        }

        // Force end any ongoing speech
        if self.in_speech {
            self.cut_current_speech(0.8, 0); // estimated confidence for a forced end
            self.in_speech = false;
            self.mid_utterance_cut = false;
        }

        Ok(self.speech_segments.drain(..).collect())
    }

    fn process_chunk(&mut self, chunk: &[f32]) -> Result<()> {
        // Track accumulated speech buffer size to detect memory issues
        let current_speech_size = self.current_speech.len();
        if current_speech_size > 1_000_000 {
            // More than ~62 seconds of accumulated speech at 16kHz
            warn!("VAD: Accumulated speech buffer is large: {} samples ({:.1}s) - possible memory issue",
                  current_speech_size, current_speech_size as f64 / 16000.0);
        }

        let transitions = self.session.process(chunk)
            .map_err(|e| anyhow!("VAD processing failed: {}", e))?;

        // Silero has consumed this frame by the time process() returns, so
        // count it now: the transitions below carry timestamps on that clock,
        // and comparing them against a sample counter that is one frame behind
        // is how trailing silence gets mis-measured.
        self.processed_samples += chunk.len();

        // Accumulate unconditionally, in speech or not. This buffer used to be
        // cleared on SpeechStart, which sounds right and is not: Silero cannot
        // raise SpeechStart until min_speech_time has already passed, and it
        // backdates its own segments by pre_speech_pad to compensate. Clearing
        // at that point threw away the ~570ms of audio containing the first
        // word or two of the utterance. Silero's own segments were unaffected,
        // so this only ever showed up on force-cut and flushed segments — long
        // utterances and the end of every recording — as transcripts that
        // start mid-sentence.
        self.current_speech.extend_from_slice(chunk);

        // Log transitions for debugging
        if !transitions.is_empty() {
            debug!("VAD transitions at sample {}: {} transitions", self.processed_samples, transitions.len());
        }

        // Handle VAD transitions
        for transition in transitions {
            match transition {
                VadTransition::SpeechStart { timestamp_ms } => {
                    // Only log if state changed
                    if !self.last_logged_state {
                        debug!("VAD: Speech started at {}ms", timestamp_ms);
                        self.last_logged_state = true;
                    }
                    self.in_speech = true;
                }
                VadTransition::SpeechEnd { start_timestamp_ms, end_timestamp_ms, samples } => {
                    // Only log if we were previously in speech state
                    if self.last_logged_state {
                        debug!("VAD: Speech ended at {}ms (duration: {}ms)", end_timestamp_ms, end_timestamp_ms - start_timestamp_ms);
                        self.last_logged_state = false;
                    }
                    self.in_speech = false;

                    if self.mid_utterance_cut {
                        // The head of this utterance already went out as forced
                        // cuts, and `samples` is the WHOLE utterance — emitting
                        // it would transcribe the same speech twice. So emit
                        // only the tail we have accumulated since the last cut,
                        // minus the redemption silence: we kept accumulating
                        // through it, because in_speech stays true until Silero
                        // finally calls the end, and 2s of silence handed to a
                        // decoder is 2s of hallucination risk and a segment
                        // whose end timestamp is 2s late.
                        self.mid_utterance_cut = false;
                        let trailing = self.samples_since_ms(end_timestamp_ms as f64);
                        self.cut_current_speech(0.9, trailing);
                    } else {
                        // Silero's own segment already carries pre/post padding
                        // and excludes the redemption silence, so prefer it.
                        let speech_samples = if samples.is_empty() {
                            std::mem::take(&mut self.current_speech)
                        } else {
                            samples
                        };

                        if !speech_samples.is_empty() {
                            let segment = SpeechSegment {
                                samples: speech_samples,
                                start_timestamp_ms: start_timestamp_ms as f64,
                                end_timestamp_ms: end_timestamp_ms as f64,
                                confidence: 0.9, // VAD confidence
                            };

                            info!("VAD: Completed speech segment: {:.1}ms duration, {} samples",
                                  end_timestamp_ms - start_timestamp_ms, segment.samples.len());

                            self.speech_segments.push_back(segment);
                        }

                        self.current_speech.clear();
                    }
                }
            }
        }

        if self.in_speech {
            // Cut BEFORE the next frame would take us past the cap, not after.
            // Overshooting by one frame put every forced cut over
            // LIVE_MAX_SEGMENT_SAMPLES, so stream_worker's enqueue() split each
            // one again — turning every long utterance into two decodes with a
            // seam in the middle of a word.
            if self.current_speech.len() + self.chunk_size > MAX_UTTERANCE_SAMPLES {
                self.mid_utterance_cut = true;
                self.cut_current_speech(0.8, 0); // forced cut, not a VAD-confirmed end
            }
        } else {
            // Idle: keep only enough history to open the next utterance cleanly.
            let excess = self.current_speech.len().saturating_sub(PRE_ROLL_SAMPLES);
            if excess > 0 {
                self.current_speech.drain(..excess);
            }
        }

        Ok(())
    }

    /// How many samples we have taken in since the given point on Silero's
    /// millisecond clock. Both counters start at the session's first sample.
    fn samples_since_ms(&self, timestamp_ms: f64) -> usize {
        let at = (timestamp_ms / 1000.0 * 16000.0) as usize;
        self.processed_samples.saturating_sub(at)
    }

    /// Close a segment on the speech accumulated so far without ending the
    /// utterance. Timestamps come from our own sample counter because the VAD
    /// has not reported an end for this speech yet.
    ///
    /// `trailing_silence` is how many samples at the end are known not to be
    /// speech (the redemption gap Silero waited through before calling the
    /// end); they are dropped so the decoder is not handed silence and the
    /// segment's end timestamp is honest.
    fn cut_current_speech(&mut self, confidence: f32, trailing_silence: usize) {
        let keep = self.current_speech.len().saturating_sub(trailing_silence);
        self.current_speech.truncate(keep);
        if self.current_speech.is_empty() {
            return;
        }
        let sample_to_ms = |s: usize| (s as f64 / 16000.0) * 1000.0;
        let end_sample = self.processed_samples.saturating_sub(trailing_silence);
        let end_ms = sample_to_ms(end_sample);
        let start_ms = sample_to_ms(end_sample.saturating_sub(self.current_speech.len()));

        info!(
            "VAD: Cut speech segment at {:.1}ms ({} samples, {:.1}s)",
            end_ms,
            self.current_speech.len(),
            self.current_speech.len() as f64 / 16000.0
        );

        self.speech_segments.push_back(SpeechSegment {
            samples: std::mem::take(&mut self.current_speech),
            start_timestamp_ms: start_ms,
            end_timestamp_ms: end_ms,
            confidence,
        });
    }
}

/// Legacy function for backward compatibility - now uses the optimized approach
pub fn extract_speech_16k(samples_mono_16k: &[f32]) -> Result<Vec<f32>> {
    let mut processor = ContinuousVadProcessor::new(16000, 400)?;

    // Process all audio
    let mut all_segments = processor.process_audio(samples_mono_16k)?;
    let final_segments = processor.flush()?;
    all_segments.extend(final_segments);

    // Concatenate all speech segments
    let mut result = Vec::new();
    let num_segments = all_segments.len();
    for segment in &all_segments {
        result.extend_from_slice(&segment.samples);
    }

    // Apply balanced energy filtering for very short segments
    if result.len() < 1600 { // Less than 100ms at 16kHz
        let input_energy: f32 = samples_mono_16k.iter().map(|&x| x * x).sum::<f32>() / samples_mono_16k.len() as f32;
        let rms = input_energy.sqrt();
        let peak = samples_mono_16k.iter().map(|&x| x.abs()).fold(0.0f32, f32::max);

        // BALANCED FIX: Lowered thresholds to preserve quiet speech while still filtering silence
        // Previous aggressive values (0.08/0.15) were discarding valid quiet speech
        // New values (0.03/0.08) are more balanced - catch quiet speech, reject pure silence
        if rms < 0.2 || peak < 0.20 {
            info!("-----VAD detected silence/noise (RMS: {:.6}, Peak: {:.6}), skipping to prevent hallucinations-----", rms, peak);
            return Ok(Vec::new());
        } else {
            info!("VAD detected speech with sufficient energy (RMS: {:.6}, Peak: {:.6})", rms, peak);
            return Ok(samples_mono_16k.to_vec());
        }
    }

    debug!("VAD: Processed {} samples, extracted {} speech samples from {} segments",
           samples_mono_16k.len(), result.len(), num_segments);

    Ok(result)
}

/// Simple convenience function to get speech chunks from audio
/// Uses the optimized ContinuousVadProcessor with configurable redemption time
pub fn get_speech_chunks(samples_mono_16k: &[f32], redemption_time_ms: u32) -> Result<Vec<SpeechSegment>> {
    get_speech_chunks_with_progress(samples_mono_16k, redemption_time_ms, |_, _| true)
}

/// Get speech chunks with progress callback and cancellation support
/// The callback receives (progress_percent, segments_found) and returns false to cancel
pub fn get_speech_chunks_with_progress<F>(
    samples_mono_16k: &[f32],
    redemption_time_ms: u32,
    mut progress_callback: F,
) -> Result<Vec<SpeechSegment>>
where
    F: FnMut(u32, usize) -> bool,
{
    let mut processor = ContinuousVadProcessor::new(16000, redemption_time_ms)?;

    let total_samples = samples_mono_16k.len();

    // For large files (>1 minute at 16kHz = 960,000 samples), process in chunks with progress logging
    const LARGE_FILE_THRESHOLD: usize = 960_000;
    const CHUNK_SIZE: usize = 160_000; // 10 seconds at 16kHz

    let mut all_segments = Vec::new();

    if total_samples > LARGE_FILE_THRESHOLD {
        info!("VAD: Processing large file ({} samples = {:.1}s), will log progress...",
              total_samples, total_samples as f64 / 16000.0);

        let mut processed = 0;
        let mut last_progress = 0u32;
        let mut chunk_count = 0;
        let total_chunks = (total_samples + CHUNK_SIZE - 1) / CHUNK_SIZE;

        for chunk in samples_mono_16k.chunks(CHUNK_SIZE) {
            chunk_count += 1;

            let start_time = std::time::Instant::now();
            let segments = processor.process_audio(chunk)?;
            let elapsed = start_time.elapsed();

            // Debug log for chunk processing details
            debug!("VAD: Chunk {}/{} processed in {:?}, found {} segments",
                  chunk_count, total_chunks, elapsed, segments.len());

            // Warn if chunk processing took too long (>1 second)
            if elapsed.as_secs() > 1 {
                warn!("VAD: Chunk {} took {:?} - possible performance issue", chunk_count, elapsed);
            }

            all_segments.extend(segments);

            processed += chunk.len();
            let progress = ((processed * 100) / total_samples) as u32;

            // Call progress callback every 5%
            if progress >= last_progress + 5 {
                debug!("VAD: Progress {}% ({} segments found so far)", progress, all_segments.len());

                // Check for cancellation
                if !progress_callback(progress, all_segments.len()) {
                    info!("VAD: Cancelled by callback at {}%", progress);
                    return Err(anyhow!("VAD processing cancelled"));
                }

                last_progress = progress;
            }
        }

        let final_segments = processor.flush()?;
        all_segments.extend(final_segments);

        info!("VAD: Complete! Found {} speech segments", all_segments.len());
    } else {
        // Small file - process all at once
        all_segments = processor.process_audio(samples_mono_16k)?;
        let final_segments = processor.flush()?;
        all_segments.extend(final_segments);
    }

    Ok(all_segments)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate synthetic speech-like audio with alternating speech/silence
    fn generate_test_audio_with_speech(duration_seconds: f32, sample_rate: u32) -> Vec<f32> {
        let total_samples = (duration_seconds * sample_rate as f32) as usize;
        let mut samples = vec![0.0f32; total_samples];

        // Create speech-like patterns: bursts of sine waves with varying amplitude
        // Speech every 10 seconds for 5 seconds
        let speech_interval = 10.0; // seconds between speech starts
        let speech_duration = 5.0;  // seconds of speech

        for i in 0..total_samples {
            let time = i as f32 / sample_rate as f32;
            let cycle_time = time % speech_interval;

            // Speech occurs in the first `speech_duration` seconds of each cycle
            if cycle_time < speech_duration {
                // Generate speech-like signal: multiple frequencies with amplitude modulation
                let freq1 = 200.0 + (time * 50.0).sin() * 100.0; // Varying fundamental
                let freq2 = freq1 * 2.0; // Harmonic
                let freq3 = freq1 * 3.0; // Another harmonic

                let amplitude = 0.3 + 0.1 * (time * 5.0).sin(); // Amplitude modulation
                samples[i] = amplitude * (
                    0.5 * (2.0 * std::f32::consts::PI * freq1 * time).sin() +
                    0.3 * (2.0 * std::f32::consts::PI * freq2 * time).sin() +
                    0.2 * (2.0 * std::f32::consts::PI * freq3 * time).sin()
                );
            }
            // else: silence (already 0.0)
        }

        samples
    }

    /// Regression: a segment only ever closed on VAD-detected silence, so a
    /// speaker who never pauses produced nothing at all until they stopped.
    /// Live transcription showed an empty screen for the whole utterance, and the
    /// backlog cap then discarded most of it when it arrived in one lump.
    ///
    /// Driven through the state directly: what needs testing is the cut policy,
    /// not whether Silero classifies a synthesized tone as speech.
    #[test]
    fn unbroken_speech_is_cut_before_the_vad_reports_an_end() {
        let mut vad = ContinuousVadProcessor::new(16000, 2000).expect("VAD");
        vad.in_speech = true; // mid-utterance, no SpeechEnd coming

        let mut segments = Vec::new();
        let secs = 30;
        for _ in 0..secs * 10 {
            // 100ms of quiet audio per iteration, as the pipeline feeds it. Silero
            // reports no transition on it, which is exactly the stuck-in-speech case.
            segments.extend(vad.process_audio(&vec![0.0f32; 1600]).expect("process"));
        }

        assert!(
            !segments.is_empty(),
            "{secs}s of unbroken speech produced no segment before flush"
        );
        let longest = segments.iter().map(|s| s.samples.len()).max().unwrap();
        assert!(
            longest <= MAX_UTTERANCE_SAMPLES + 1600,
            "segment of {longest} samples ignores the {MAX_UTTERANCE_SAMPLES}-sample cut"
        );
        // Cuts must partition the audio, not overlap it: re-decoding speech that
        // already went out is how the transcript ends up saying things twice.
        let emitted: usize = segments
            .iter()
            .chain(vad.flush().expect("flush").iter())
            .map(|s| s.samples.len())
            .sum();
        assert_eq!(emitted, secs * 16000, "emitted audio should equal audio fed");
    }

    /// Regression: forced cuts landed one frame PAST the cap, so
    /// `stream_worker::enqueue` saw an over-long segment and split every one of
    /// them again — an extra decode per long utterance, with the seam falling
    /// wherever the split heuristic landed rather than at a pause.
    #[test]
    fn a_forced_cut_never_exceeds_the_cap_that_would_re_split_it() {
        let mut vad = ContinuousVadProcessor::new(16000, 2000).expect("VAD");
        vad.in_speech = true; // mid-utterance, no SpeechEnd coming

        let mut segments = Vec::new();
        for _ in 0..400 {
            segments.extend(vad.process_audio(&vec![0.1f32; 1600]).expect("process"));
        }

        assert!(!segments.is_empty(), "expected forced cuts");
        for segment in &segments {
            assert!(
                segment.samples.len() <= MAX_UTTERANCE_SAMPLES,
                "a cut of {} samples is over the {MAX_UTTERANCE_SAMPLES} cap and will be split again",
                segment.samples.len()
            );
        }
    }

    /// Regression: `current_speech` was cleared on SpeechStart, but Silero
    /// cannot raise SpeechStart until min_speech_time has already elapsed. Any
    /// segment we built ourselves therefore began ~570ms into the utterance,
    /// and force-cut and flushed transcripts started mid-word.
    #[test]
    fn speech_before_the_vad_notices_it_is_still_in_the_segment() {
        let mut vad = ContinuousVadProcessor::new(16000, 2000).expect("VAD");

        // 400ms of audio that Silero has not (yet) called speech. This is the
        // window the onset of a real utterance lives in.
        let onset = vec![0.42f32; 6400];
        vad.process_audio(&onset).expect("process");
        assert!(!vad.in_speech, "precondition: the VAD has not called this speech");

        // Now speech is recognised, and the utterance ends by flush.
        vad.in_speech = true;
        vad.process_audio(&vec![0.5f32; 16000]).expect("process");
        let segments = vad.flush().expect("flush");

        let emitted: usize = segments.iter().map(|s| s.samples.len()).sum();
        assert!(
            emitted > 16000,
            "segment holds {emitted} samples: the pre-speech onset was thrown away"
        );
        assert!(
            segments[0].samples.iter().any(|&s| s == 0.42),
            "the segment must actually contain the pre-speech audio, not just be long"
        );
    }

    /// Regression: resampling to 16kHz rings slightly past ±1.0, and Silero
    /// rejects any frame containing such a sample — so the segmented live path
    /// dropped every 30ms frame of loud audio and transcribed nothing.
    #[test]
    fn audio_that_overshoots_full_scale_is_still_processed() {
        let mut vad = ContinuousVadProcessor::new(16000, 2000).expect("VAD");

        // What the sinc downsampler hands over after the mixer clamped to 1.0.
        let overshoot: Vec<f32> = (0..16000)
            .map(|i| if i % 2 == 0 { 1.0003 } else { -1.0007 })
            .collect();

        vad.process_audio(&overshoot)
            .expect("out-of-range samples must not fail the frame");
    }

    /// The pre-roll must not grow without bound through a long silence.
    #[test]
    fn silence_does_not_accumulate_beyond_the_pre_roll() {
        let mut vad = ContinuousVadProcessor::new(16000, 2000).expect("VAD");
        for _ in 0..60 {
            vad.process_audio(&vec![0.0f32; 16000]).expect("process"); // 60s
        }
        assert!(
            vad.current_speech.len() <= PRE_ROLL_SAMPLES,
            "held {} samples of silence, cap is {PRE_ROLL_SAMPLES}",
            vad.current_speech.len()
        );
    }

    #[test]
    fn test_vad_chunked_vs_single_processing() {
        // Generate 60 seconds of audio with speech patterns at 16kHz
        let audio = generate_test_audio_with_speech(60.0, 16000);
        println!("Generated {} samples ({:.1}s)", audio.len(), audio.len() as f32 / 16000.0);

        // Process all at once (like small files)
        let segments_single = get_speech_chunks(&audio, 2000).expect("Single processing failed");
        println!("Single processing found {} segments", segments_single.len());

        // Process in chunks (like large files)
        let segments_chunked = get_speech_chunks_with_progress(&audio, 2000, |progress, segments| {
            println!("Chunked progress: {}%, {} segments", progress, segments);
            true // Don't cancel
        }).expect("Chunked processing failed");
        println!("Chunked processing found {} segments", segments_chunked.len());

        // Both should find the same number of segments (approximately)
        // Allow some variance due to chunk boundary effects
        let diff = (segments_single.len() as i32 - segments_chunked.len() as i32).abs();
        assert!(diff <= 1,
            "Chunked and single processing found different segment counts: {} vs {} (diff: {})",
            segments_single.len(), segments_chunked.len(), diff);
    }

    #[test]
    fn test_vad_large_file_progress() {
        // Generate 120 seconds (2 minutes) of audio - triggers large file threshold
        let audio = generate_test_audio_with_speech(120.0, 16000);
        let total_samples = audio.len();
        println!("Generated {} samples ({:.1}s)", total_samples, total_samples as f32 / 16000.0);

        // This should trigger the large file path (>960,000 samples)
        assert!(total_samples > 960_000, "Audio should be large enough to trigger chunked processing");

        let mut progress_updates = Vec::new();
        let segments = get_speech_chunks_with_progress(&audio, 2000, |progress, segments| {
            progress_updates.push((progress, segments));
            true // Don't cancel
        }).expect("Processing failed");

        println!("Found {} segments with {} progress updates", segments.len(), progress_updates.len());

        // The synthetic signal is not real speech, so Silero may merge it into
        // one long segment. This test is specifically for the large-file path:
        // it must still emit speech and report monotonic progress through 100%.
        assert!(!segments.is_empty(), "Expected at least one speech segment");
        assert!(
            segments.iter().all(|segment| !segment.samples.is_empty()
                && segment.end_timestamp_ms > segment.start_timestamp_ms),
            "Expected all speech segments to contain audio with positive duration"
        );

        // Should have received progress updates
        assert!(!progress_updates.is_empty(), "Expected progress updates for large file");
        assert_eq!(
            progress_updates.last().map(|(progress, _)| *progress),
            Some(100),
            "Expected progress to reach 100%"
        );
        assert!(
            progress_updates
                .windows(2)
                .all(|pair| pair[0].0 < pair[1].0),
            "Expected progress updates to increase monotonically: {:?}",
            progress_updates
        );
    }

    #[test]
    fn test_vad_cancellation() {
        let audio = generate_test_audio_with_speech(120.0, 16000);

        // Cancel at 50%
        let result = get_speech_chunks_with_progress(&audio, 2000, |progress, _| {
            progress < 50 // Cancel when reaching 50%
        });

        // Should return error due to cancellation
        assert!(result.is_err(), "Expected cancellation error");
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("cancelled"), "Error should mention cancellation: {}", err_msg);
    }

    #[test]
    fn test_vad_continuous_processor_state_across_chunks() {
        // Test that VAD state is correctly maintained across chunk boundaries
        let mut processor = ContinuousVadProcessor::new(16000, 2000).expect("Failed to create processor");

        // Generate audio with a speech segment that spans a chunk boundary
        let chunk_size = 160_000; // 10 seconds
        let audio = generate_test_audio_with_speech(30.0, 16000); // 30 seconds

        // Process in 10-second chunks
        let mut all_segments = Vec::new();
        for (i, chunk) in audio.chunks(chunk_size).enumerate() {
            let segments = processor.process_audio(chunk).expect("Processing failed");
            println!("Chunk {}: processed {} samples, found {} segments", i, chunk.len(), segments.len());
            all_segments.extend(segments);
        }

        // Flush remaining
        let final_segments = processor.flush().expect("Flush failed");
        all_segments.extend(final_segments);

        println!("Total segments found: {}", all_segments.len());

        // Should find speech segments
        assert!(all_segments.len() >= 1, "Expected at least 1 speech segment");
    }

    #[test]
    fn test_vad_400ms_vs_2000ms_segmentation() {
        // Demonstrates why 2000ms redemption is needed for batch processing:
        // 400ms creates excessive fragmentation, 2000ms bridges natural pauses.
        //
        // Audio pattern: 60s with 5s speech / 5s silence cycles
        // Natural pauses within speech (sentence gaps) are 500ms-1.5s
        let audio = generate_test_audio_with_speech(60.0, 16000);

        let segments_400 = get_speech_chunks(&audio, 400).expect("400ms processing failed");
        let segments_2000 = get_speech_chunks(&audio, 2000).expect("2000ms processing failed");

        println!(
            "400ms redemption: {} segments, 2000ms redemption: {} segments",
            segments_400.len(),
            segments_2000.len()
        );

        // 2000ms should produce fewer or equal segments (bridges more pauses)
        assert!(
            segments_2000.len() <= segments_400.len(),
            "2000ms redemption ({} segments) should not produce more segments than 400ms ({} segments)",
            segments_2000.len(),
            segments_400.len()
        );

        // Verify segments have reasonable durations with 2000ms
        for (i, seg) in segments_2000.iter().enumerate() {
            let duration_ms = seg.end_timestamp_ms - seg.start_timestamp_ms;
            println!("2000ms segment {}: {:.0}ms duration", i, duration_ms);
            // Each segment should be at least 250ms (min_speech_time)
            assert!(duration_ms >= 200.0, "Segment {} too short: {:.0}ms", i, duration_ms);
        }
    }
}

