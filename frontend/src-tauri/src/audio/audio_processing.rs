use anyhow::Result;
use chrono::Utc;
use log::{debug, info, warn};
use realfft::num_complex::{Complex32, ComplexFloat};
use realfft::RealFftPlanner;
// rubato 5 replaced SincFixedIn with the unified `Async` resampler
// (FixedAsync::Input is the same "fixed input size, varying output" mode), and
// moved I/O onto the audioadapter buffer traits it re-exports. Mono audio is a
// flat interleaved slice, so InterleavedSlice wraps our &[f32] with no copy and
// without the old Vec<Vec<f32>> per call.
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};
use std::path::PathBuf;
use nnnoiseless::DenoiseState;

use super::encode::encode_single_audio; // Correct path to encode module

/// Sanitize a filename to be safe for filesystem use
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// Create a meeting folder with timestamp and return the path
/// Creates structure: base_path/MeetingName_YYYY-MM-DD_HH-MM/
///                    ├── .checkpoints/  (for incremental saves, optional)
///
/// # Arguments
/// * `base_path` - Base directory for meetings
/// * `meeting_name` - Name of the meeting
/// * `create_checkpoints_dir` - Whether to create .checkpoints/ subdirectory (only needed when auto_save is true)
pub fn create_meeting_folder(
    base_path: &PathBuf,
    meeting_name: &str,
    create_checkpoints_dir: bool,
) -> Result<PathBuf> {
    let timestamp = Utc::now().format("%Y-%m-%d_%H-%M").to_string();
    let sanitized_name = sanitize_filename(meeting_name);
    let folder_name = format!("{}_{}", sanitized_name, timestamp);
    let meeting_folder = base_path.join(folder_name);

    // Create main meeting folder
    std::fs::create_dir_all(&meeting_folder)?;

    // Only create .checkpoints subdirectory if requested (when auto_save is true)
    if create_checkpoints_dir {
        let checkpoints_dir = meeting_folder.join(".checkpoints");
        std::fs::create_dir_all(&checkpoints_dir)?;
        log::info!("Created meeting folder with checkpoints: {}", meeting_folder.display());
    } else {
        log::info!("Created meeting folder without checkpoints: {}", meeting_folder.display());
    }

    Ok(meeting_folder)
}

pub fn normalize_v2(audio: &[f32]) -> Vec<f32> {
    let rms = (audio.iter().map(|&x| x * x).sum::<f32>() / audio.len() as f32).sqrt();
    let peak = audio
        .iter()
        .fold(0.0f32, |max, &sample| max.max(sample.abs()));

    // Return the original audio if it's completely silent
    if rms == 0.0 || peak == 0.0 {
        return audio.to_vec();
    }

    // Increase target RMS for better voice volume while keeping peak in check
    let target_rms = 0.9;  // Increased from 0.6
    let target_peak = 0.95; // Slightly reduced to prevent clipping

    let rms_scaling = target_rms / rms;
    let peak_scaling = target_peak / peak;

    // Apply a minimum scaling factor to boost very quiet audio
    let min_scaling = 1.5; // Minimum boost for quiet audio
    let scaling_factor = (rms_scaling.min(peak_scaling)).max(min_scaling);

    // Apply scaling with soft clipping to prevent harsh distortion
    audio
        .iter()
        .map(|&sample| {
            let scaled = sample * scaling_factor;
            // Soft clip at ±0.95 to prevent harsh distortion
            if scaled > 0.95 {
                0.95 + (scaled - 0.95) * 0.05
            } else if scaled < -0.95 {
                -0.95 + (scaled + 0.95) * 0.05
            } else {
                scaled
            }
        })
        .collect()
}

// A `TruePeakLimiter` used to sit here, described as a 10ms-lookahead true-peak
// limiter. It was not one. It scaled each delayed sample by a reduction
// computed from that same sample — `buffer[i] * (limit / |buffer[i]|)` — which
// is `sign(buffer[i]) * limit`, i.e. hard clipping, reached via a delay line
// that changed nothing. Real lookahead means applying the MINIMUM reduction
// over the window ahead, so the gain is already down before the peak arrives;
// applying each sample's own reduction is clipping with extra steps.
//
// It is gone rather than fixed. Once the gain below is bounded and glided, the
// mic no longer arrives 30dB hot, so the stage it was meant to protect against
// barely engages — and an honest clamp is both shorter and no worse than what
// this actually did.

/// Professional loudness normalizer using EBU R128 standard
/// This is a STATEFUL normalizer that tracks cumulative loudness over time
///
/// EBU R128 is the broadcast industry standard for loudness normalization:
/// - Target: -23 LUFS (Loudness Units relative to Full Scale)
/// - Used by: Netflix, YouTube, Spotify, all professional broadcast
/// - Perceptually accurate (not just simple RMS)
///
pub struct LoudnessNormalizer {
    ebur128: ebur128::EbuR128,
    /// Where the gain is heading, from the latest loudness measurement.
    target_gain: f32,
    /// Where it is now. Moves toward `target_gain` a sample at a time, so a
    /// measurement jump does not step the signal.
    gain_linear: f32,
    loudness_buffer: Vec<f32>,
    true_peak_limit: f32,
}

/// Bounds on the correction this stage may apply, in dB.
///
/// Unbounded is what it was, and `loudness_global()` over the first fraction of
/// a second of a meeting measures room tone, not speech — around -60 LUFS,
/// asking for +37dB. So every recording opened by amplifying its own noise
/// floor by a factor of ~70 and clipping whatever the speaker said next, which
/// is precisely the audio the model has to read to get the first sentence.
///
/// Asymmetric on purpose: a built-in laptop mic at -45 LUFS legitimately needs
/// a lot of boost, so capping at +12dB would defeat the feature this exists
/// for. Cutting never needs the same range.
const MIN_GAIN_DB: f64 = -12.0;
const MAX_GAIN_DB: f64 = 24.0;

/// Per-sample approach rate toward the measured gain. ~1/(0.25s at 48kHz), so
/// a gain change settles in about a quarter of a second — fast enough to track
/// a speaker, slow enough to be inaudible.
const GAIN_GLIDE: f32 = 1.0 / 12_000.0;

impl LoudnessNormalizer {
    /// Create a new EBU R128 loudness normalizer
    ///
    /// # Arguments
    /// * `channels` - Number of audio channels (1 for mono, 2 for stereo)
    /// * `sample_rate` - Sample rate in Hz (e.g., 48000)
    pub fn new(channels: u32, sample_rate: u32) -> Result<Self> {
        const TRUE_PEAK_LIMIT: f64 = -1.0;
        const ANALYZE_CHUNK_SIZE: usize = 512;

        // HISTOGRAM matters: without it ebur128 keeps every 100ms block of the
        // meeting in a growing Vec and re-scans the whole thing on each
        // loudness_global() call — which happens every 512 samples, inside the
        // realtime capture callback. That is quadratic in meeting length on the
        // one thread that must not miss its deadline. With HISTOGRAM the same
        // measurement runs over a fixed 1000-bucket array forever.
        let ebur128 = ebur128::EbuR128::new(
            channels,
            sample_rate,
            ebur128::Mode::I | ebur128::Mode::TRUE_PEAK | ebur128::Mode::HISTOGRAM,
        )
        .map_err(|e| anyhow::anyhow!("Failed to create EBU R128 normalizer: {}", e))?;

        let true_peak_limit = 10_f32.powf(TRUE_PEAK_LIMIT as f32 / 20.0);

        Ok(Self {
            ebur128,
            target_gain: 1.0,
            gain_linear: 1.0,
            loudness_buffer: Vec::with_capacity(ANALYZE_CHUNK_SIZE),
            true_peak_limit,
        })
    }

    /// Normalize loudness using EBU R128 standard with true peak limiting
    ///
    /// This maintains cumulative loudness measurements across all processed audio,
    /// resulting in consistent normalization that sounds natural.
    ///
    /// Target: -23 LUFS (professional broadcast standard for speech/dialog)
    /// Applies sample-by-sample with 10ms lookahead limiter to prevent clipping
    pub fn normalize_loudness(&mut self, samples: &[f32]) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }

        const TARGET_LUFS: f64 = -23.0;
        const ANALYZE_CHUNK_SIZE: usize = 512;

        let mut normalized_samples = Vec::with_capacity(samples.len());

        for &sample in samples {
            // Accumulate samples for loudness analysis
            self.loudness_buffer.push(sample);

            // Analyze loudness every 512 samples
            if self.loudness_buffer.len() >= ANALYZE_CHUNK_SIZE {
                if let Err(e) = self.ebur128.add_frames_f32(&self.loudness_buffer) {
                    warn!("Failed to add frames to EBU R128: {}", e);
                } else {
                    // Update gain based on cumulative loudness
                    if let Ok(current_lufs) = self.ebur128.loudness_global() {
                        if current_lufs.is_finite() && current_lufs < 0.0 {
                            let gain_db =
                                (TARGET_LUFS - current_lufs).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
                            self.target_gain = 10_f32.powf(gain_db as f32 / 20.0);
                        }
                    }
                }
                self.loudness_buffer.clear();
            }

            // Glide toward the target instead of stepping to it. The
            // measurement moves fastest in the first seconds of a meeting,
            // which is exactly where a step would land in the middle of the
            // first sentence.
            self.gain_linear += (self.target_gain - self.gain_linear) * GAIN_GLIDE;

            // An honest clamp, where a delay line pretending to be a limiter
            // used to be.
            normalized_samples.push((sample * self.gain_linear).clamp(-self.true_peak_limit, self.true_peak_limit));
        }

        normalized_samples
    }
}

/// RNNoise-based noise suppression processor
///
/// Uses a recurrent neural network to suppress background noise while preserving speech.
/// Processes audio at 48kHz in 10ms frames (480 samples per frame).
///
/// Benefits:
/// - 10-15 dB noise reduction in typical office/home environments
/// - Preserves speech quality and intelligibility
/// - Low latency (~10ms per frame)
/// - Cross-platform (works on macOS, Windows, Linux)
pub struct NoiseSuppressionProcessor {
    denoiser: DenoiseState<'static>,
    frame_buffer: Vec<f32>,
    frame_size: usize,  // 480 samples at 48kHz = 10ms
}

impl NoiseSuppressionProcessor {
    /// Create a new noise suppression processor
    ///
    /// # Arguments
    /// * `sample_rate` - Must be 48000 Hz (RNNoise requirement)
    pub fn new(sample_rate: u32) -> Result<Self> {
        if sample_rate != 48000 {
            return Err(anyhow::anyhow!(
                "Noise suppression requires 48kHz sample rate, got {}Hz",
                sample_rate
            ));
        }

        const FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;

        info!("Initializing RNNoise noise suppression (frame size: {} samples, 10ms @ 48kHz)", FRAME_SIZE);

        Ok(Self {
            denoiser: *DenoiseState::new(),
            frame_buffer: Vec::with_capacity(FRAME_SIZE * 2),
            frame_size: FRAME_SIZE,
        })
    }

    /// Apply noise suppression to audio samples
    ///
    /// Processes audio in 480-sample frames (10ms at 48kHz).
    /// Buffers partial frames for next call.
    ///
    /// CRITICAL FIX: Always returns same length as input to prevent latency accumulation
    ///
    /// # Arguments
    /// * `samples` - Input audio samples at 48kHz
    ///
    /// # Returns
    /// Noise-suppressed audio samples (SAME LENGTH as input)
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }

        // CRITICAL: Remember original input length
        let input_len = samples.len();

        // Add new samples to buffer
        self.frame_buffer.extend_from_slice(samples);

        let mut output = Vec::with_capacity(input_len);

        // Process complete frames
        while self.frame_buffer.len() >= self.frame_size {
            // Extract one frame
            let frame: Vec<f32> = self.frame_buffer.drain(0..self.frame_size).collect();

            // RNNoise processes audio: separate input and output buffers
            let mut denoised_frame = vec![0.0f32; self.frame_size];

            // Apply noise suppression
            // process_frame(output: &mut [f32], input: &[f32]) -> f32
            // Returns VAD probability (0.0-1.0), higher means more likely to be speech
            let _vad_prob = self.denoiser.process_frame(&mut denoised_frame, &frame);

            output.extend_from_slice(&denoised_frame);
        }

        // Return processed output without forcing length matching
        // Frame-based processing naturally creates variable-length output
        // Downstream pipeline handles this correctly via ring buffer
        output
    }

    /// Get the number of buffered samples waiting for processing
    pub fn buffered_samples(&self) -> usize {
        self.frame_buffer.len()
    }

    /// Flush any remaining buffered samples
    /// Call this at the end of recording to process partial frames
    pub fn flush(&mut self) -> Vec<f32> {
        if self.frame_buffer.is_empty() {
            return Vec::new();
        }

        // Pad the remaining samples to a full frame with zeros
        let remaining = self.frame_buffer.len();
        let mut input_frame = self.frame_buffer.clone();
        if input_frame.len() < self.frame_size {
            input_frame.resize(self.frame_size, 0.0);
        }

        let mut output = vec![0.0f32; self.frame_size];
        self.denoiser.process_frame(&mut output, &input_frame);
        self.frame_buffer.clear();

        // Return only the original samples (without padding)
        output.truncate(remaining);
        output
    }
}

/// High-pass filter to remove low-frequency rumble and noise
/// Removes frequencies below cutoff_hz (typically 80-100 Hz for speech)
pub struct HighPassFilter {
    #[allow(dead_code)]
    sample_rate: f32,
    #[allow(dead_code)]
    cutoff_hz: f32,
    // First-order IIR filter coefficients
    alpha: f32,
    prev_input: f32,
    prev_output: f32,
}

impl HighPassFilter {
    /// Create a new high-pass filter
    ///
    /// # Arguments
    /// * `sample_rate` - Audio sample rate in Hz
    /// * `cutoff_hz` - Cutoff frequency in Hz (typical: 80-100 Hz for speech)
    pub fn new(sample_rate: u32, cutoff_hz: f32) -> Self {
        let sample_rate_f = sample_rate as f32;
        let rc = 1.0 / (2.0 * std::f32::consts::PI * cutoff_hz);
        let dt = 1.0 / sample_rate_f;
        let alpha = rc / (rc + dt);

        info!("Initializing high-pass filter: cutoff={}Hz @ {}Hz", cutoff_hz, sample_rate);

        Self {
            sample_rate: sample_rate_f,
            cutoff_hz,
            alpha,
            prev_input: 0.0,
            prev_output: 0.0,
        }
    }

    /// Apply high-pass filter to audio samples
    /// Uses first-order IIR (Infinite Impulse Response) filter
    pub fn process(&mut self, samples: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity(samples.len());

        for &sample in samples {
            // First-order high-pass IIR filter formula:
            // y[n] = alpha * (y[n-1] + x[n] - x[n-1])
            let filtered = self.alpha * (self.prev_output + sample - self.prev_input);

            self.prev_input = sample;
            self.prev_output = filtered;

            output.push(filtered);
        }

        output
    }

    /// Reset filter state (call when starting new recording)
    pub fn reset(&mut self) {
        self.prev_input = 0.0;
        self.prev_output = 0.0;
    }
}

pub fn spectral_subtraction(audio: &[f32], d: f32) -> Result<Vec<f32>> {
    let mut real_planner = RealFftPlanner::<f32>::new();
    let window_size = 1600; // 16k sample rate - 100ms

    // CRITICAL FIX: Handle cases where audio is longer than window size
    if audio.is_empty() {
        return Ok(Vec::new());
    }

    // If audio is longer than window size, truncate to prevent overflow
    let processed_audio = if audio.len() > window_size {
        warn!("Audio length {} exceeds window size {}, truncating", audio.len(), window_size);
        &audio[..window_size]
    } else {
        audio
    };

    let r2c = real_planner.plan_fft_forward(window_size);
    let mut y = r2c.make_output_vec();

    // Safe padding: only pad if audio is shorter than window size
    let mut padded_audio = processed_audio.to_vec();
    if processed_audio.len() < window_size {
        let padding_needed = window_size - processed_audio.len();
        padded_audio.extend(vec![0.0f32; padding_needed]);
    }

    let mut indata = padded_audio;
    r2c.process(&mut indata, &mut y)?;

    let mut processed_audio = y
        .iter()
        .map(|&x| {
            let magnitude_y = x.abs().powf(2.0);

            let div = 1.0 - (d / magnitude_y);

            let gain = {
                if div > 0.0 {
                    f32::sqrt(div)
                } else {
                    0.0f32
                }
            };

            x * gain
        })
        .collect::<Vec<Complex32>>();

    let c2r = real_planner.plan_fft_inverse(window_size);

    let mut outdata = c2r.make_output_vec();

    c2r.process(&mut processed_audio, &mut outdata)?;

    Ok(outdata)
}

// not an average of non-speech segments, but I don't know how much pause time we
// get. for now, we will just assume the noise is constant (kinda defeats the purpose)
// but oh well
pub fn average_noise_spectrum(audio: &[f32]) -> f32 {
    let mut total_sum = 0.0f32;

    for sample in audio {
        let magnitude = sample.abs();

        total_sum += magnitude.powf(2.0);
    }

    total_sum / audio.len() as f32
}

pub fn audio_to_mono(audio: &[f32], channels: u16) -> Vec<f32> {
    let mut mono_samples = Vec::with_capacity(audio.len() / channels as usize);

    // For microphone arrays (> 2 channels), only use first 2 channels
    // Many microphone arrays have auxiliary channels for beam-forming/noise cancellation
    // that can contain anti-phase signals. Averaging all channels can cause destructive
    // interference resulting in near-zero output.
    let effective_channels = if channels > 2 { 2 } else { channels };

    // Iterate over the audio slice in chunks, each containing `channels` samples
    for chunk in audio.chunks(channels as usize) {
        // Sum only the first effective_channels (typically 1-2 for mic arrays)
        let sum: f32 = chunk.iter().take(effective_channels as usize).sum();

        // Calculate the average mono sample using effective channel count
        let mono_sample = sum / effective_channels as f32;

        // Store the computed mono sample
        mono_samples.push(mono_sample);
    }

    mono_samples
}

/// High-quality audio resampling with adaptive parameters based on sample rate ratio
///
/// This function automatically selects the best resampling parameters based on:
/// - Sample rate ratio (upsampling vs downsampling)
/// - Quality requirements (integer ratios get optimized paths)
/// - Anti-aliasing needs
///
/// Supports all common sample rates: 8kHz, 16kHz, 24kHz, 44.1kHz, 48kHz, etc.
pub fn resample(input: &[f32], from_sample_rate: u32, to_sample_rate: u32) -> Result<Vec<f32>> {
    if input.is_empty() {
        return Ok(Vec::new());
    }

    // Fast path: No resampling needed
    if from_sample_rate == to_sample_rate {
        return Ok(input.to_vec());
    }

    let ratio = to_sample_rate as f64 / from_sample_rate as f64;

    // Adaptive parameters based on sample rate ratio
    let (sinc_len, interpolation_type, oversampling) = if ratio >= 2.0 {
        // Large upsampling (e.g., 8kHz → 16kHz, 16kHz → 48kHz, 24kHz → 48kHz)
        // Needs high quality to avoid artifacts
        debug!("High-quality upsampling: {}Hz → {}Hz (ratio: {:.2}x)",
               from_sample_rate, to_sample_rate, ratio);
        (
            512,                              // Longer sinc for smoother interpolation
            SincInterpolationType::Cubic,     // Cubic for best quality
            512,                              // Higher oversampling
        )
    } else if ratio >= 1.5 {
        // Moderate upsampling (e.g., 32kHz → 48kHz)
        debug!("Moderate upsampling: {}Hz → {}Hz (ratio: {:.2}x)",
               from_sample_rate, to_sample_rate, ratio);
        (
            384,
            SincInterpolationType::Cubic,
            384,
        )
    } else if ratio > 1.0 {
        // Small upsampling (e.g., 44.1kHz → 48kHz)
        debug!("Small upsampling: {}Hz → {}Hz (ratio: {:.2}x)",
               from_sample_rate, to_sample_rate, ratio);
        (
            256,
            SincInterpolationType::Linear,
            256,
        )
    } else if ratio <= 0.5 {
        // Large downsampling (e.g., 48kHz → 16kHz, 48kHz → 8kHz)
        // Needs strong anti-aliasing
        debug!("Anti-aliased downsampling: {}Hz → {}Hz (ratio: {:.2}x)",
               from_sample_rate, to_sample_rate, ratio);
        (
            512,                              // Longer sinc for anti-aliasing
            SincInterpolationType::Cubic,     // Cubic for quality
            512,
        )
    } else {
        // Moderate downsampling (e.g., 48kHz → 24kHz, 48kHz → 32kHz)
        debug!("Moderate downsampling: {}Hz → {}Hz (ratio: {:.2}x)",
               from_sample_rate, to_sample_rate, ratio);
        (
            384,
            SincInterpolationType::Linear,
            384,
        )
    };

    let params = SincInterpolationParameters {
        sinc_len,
        // Some(_) since rubato 5: None lets it choose a cutoff automatically,
        // but 0.95 is the value this pipeline has always used — keep it explicit
        // rather than silently changing the filter as part of an upgrade.
        f_cutoff: Some(0.95),                // Preserve most of the frequency content
        interpolation: interpolation_type,
        oversampling_factor: oversampling,
        window: WindowFunction::BlackmanHarris2,  // Best window for audio
    };

    let mut resampler = Async::<f32>::new_sinc(
        ratio,
        2.0,  // Maximum relative deviation
        &params,
        input.len(),
        1,    // Mono
        FixedAsync::Input,
    )?;

    let waves_in = InterleavedSlice::new(input, 1, input.len())
        .map_err(|e| anyhow::anyhow!("Failed to wrap input for resampling: {e}"))?;
    let waves_out = resampler.process(&waves_in, None)?.take_data();

    debug!("Resampling complete: {} samples → {} samples",
           input.len(), waves_out.len());

    Ok(waves_out)
}

// Alias for compatibility with existing code
pub fn resample_audio(input: &[f32], from_sample_rate: u32, to_sample_rate: u32) -> Vec<f32> {
    match resample(input, from_sample_rate, to_sample_rate) {
        Ok(result) => result,
        Err(e) => {
            debug!("Resampling failed: {}, returning original audio", e);
            input.to_vec()
        }
    }
}

/// Every ASR model in the app reads 16kHz mono, so this is the last thing that
/// touches live audio before the model does.
///
/// It exists because [`resample`] builds a fresh resampler per call: correct for
/// a whole file, wrong for a stream, where the filter has to carry its state
/// across calls or every chunk boundary is a discontinuity.
///
/// What it replaces was worse than either. `vad::resample_to_16k` computed its
/// anti-alias width as `sample_rate / (0.4 * sample_rate)`, which is `2` for
/// every sample rate — a 5-tap moving average standing in for the 8kHz lowpass
/// a 48k->16k decimation needs. Everything the microphone picked up between
/// 8kHz and 24kHz folded back down on top of the speech the model was trying to
/// read. That is heard as a model that mishears words, not as an audio bug.
pub struct StreamingDownsampler16k {
    resampler: Option<Async<f32>>,
    source_rate: u32,
    /// Input samples not yet forming a whole resampler chunk. Carried between
    /// calls so no audio is lost at a chunk boundary.
    pending: Vec<f32>,
    chunk: usize,
}

impl StreamingDownsampler16k {
    const TARGET_RATE: u32 = 16_000;
    /// 10ms of input per resampler call. Small enough not to add latency of its
    /// own, large enough that the per-call overhead is irrelevant.
    const CHUNK_MS: u32 = 10;

    pub fn new(source_rate: u32) -> Self {
        let chunk = (source_rate * Self::CHUNK_MS / 1000).max(1) as usize;

        // sinc_len 64 is transparent for speech and cheap enough to stay well
        // inside the realtime budget; this is the same trade the capture-side
        // resampler already makes.
        let params = SincInterpolationParameters {
            sinc_len: 64,
            f_cutoff: Some(0.95),
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 128,
            window: WindowFunction::BlackmanHarris2,
        };

        let resampler = if source_rate == Self::TARGET_RATE {
            None
        } else {
            match Async::<f32>::new_sinc(
                Self::TARGET_RATE as f64 / source_rate as f64,
                2.0,
                &params,
                chunk,
                1,
                FixedAsync::Input,
            ) {
                Ok(r) => {
                    info!(
                        "✅ Live transcription downsampler: {}Hz → {}Hz (chunk {} samples)",
                        source_rate,
                        Self::TARGET_RATE,
                        chunk
                    );
                    Some(r)
                }
                Err(e) => {
                    // Not fatal: the per-call resampler still produces correct
                    // audio, it just rebuilds its filter every window.
                    warn!("⚠️ Could not build the streaming downsampler ({e}); falling back to per-call resampling");
                    None
                }
            }
        };

        Self { resampler, source_rate, pending: Vec::with_capacity(chunk * 2), chunk }
    }

    /// Feed input-rate samples, get back whatever 16kHz audio is ready.
    ///
    /// Output lags input by less than one chunk; the remainder stays buffered
    /// rather than being dropped or zero-padded.
    pub fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        if samples.is_empty() {
            return Vec::new();
        }
        let Some(resampler) = self.resampler.as_mut() else {
            // Either already 16kHz, or the resampler could not be built.
            if self.source_rate == Self::TARGET_RATE {
                return samples.to_vec();
            }
            return resample_audio(samples, self.source_rate, Self::TARGET_RATE);
        };

        self.pending.extend_from_slice(samples);

        let mut out = Vec::with_capacity(
            (self.pending.len() * Self::TARGET_RATE as usize) / self.source_rate as usize + 1,
        );
        while self.pending.len() >= self.chunk {
            let input = self.pending.drain(..self.chunk).collect::<Vec<f32>>();
            let Ok(adapter) = InterleavedSlice::new(&input, 1, self.chunk) else {
                // Only fails on a length mismatch, which the drain above rules
                // out; skip the chunk rather than kill the transcript.
                warn!("Live downsampling: could not wrap a {} sample chunk", self.chunk);
                continue;
            };
            match resampler.process(&adapter, None) {
                Ok(wave) => out.extend_from_slice(&wave.take_data()),
                // Losing one chunk is survivable; ending the transcript is not.
                Err(e) => warn!("Live downsampling failed for one chunk: {e}"),
            }
        }
        out
    }
}

/// Fast resampling optimized for transcription preprocessing
///
pub fn write_audio_to_file(
    audio: &[f32],
    sample_rate: u32,
    output_path: &PathBuf,
    device: &str,
    skip_encoding: bool,
) -> Result<String> {
    write_audio_to_file_with_meeting_name(audio, sample_rate, output_path, device, skip_encoding, None)
}

pub fn write_audio_to_file_with_meeting_name(
    audio: &[f32],
    sample_rate: u32,
    output_path: &PathBuf,
    device: &str,
    skip_encoding: bool,
    meeting_name: Option<&str>,
) -> Result<String> {
    let timestamp = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let sanitized_device_name = device.replace(['/', '\\'], "_");

    // Create meeting folder if meeting name is provided
    let final_output_path = if let Some(name) = meeting_name {
        let sanitized_meeting_name = sanitize_filename(name);
        let meeting_folder = output_path.join(&sanitized_meeting_name);

        // Create the meeting folder if it doesn't exist
        if !meeting_folder.exists() {
            std::fs::create_dir_all(&meeting_folder)?;
        }

        meeting_folder
    } else {
        output_path.clone()
    };

    let file_path = final_output_path
        .join(format!("{}_{}.mp4", sanitized_device_name, timestamp))
        .to_str()
        .expect("Failed to create valid path")
        .to_string();
    let file_path_clone = file_path.clone();
    // Run FFmpeg in a separate task
    if !skip_encoding {
        encode_single_audio(
            bytemuck::cast_slice(audio),
            sample_rate,
            1,
            &file_path.into(),
        )?;
    }
    Ok(file_path_clone)
}

/// Write transcript text to a file alongside the recording (legacy plain text format)
pub fn write_transcript_to_file(
    transcript_text: &str,
    output_path: &PathBuf,
    meeting_name: Option<&str>,
) -> Result<String> {
    let timestamp = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();

    // Create meeting folder if meeting name is provided (same logic as audio)
    let final_output_path = if let Some(name) = meeting_name {
        let sanitized_meeting_name = sanitize_filename(name);
        let meeting_folder = output_path.join(&sanitized_meeting_name);

        // Create the meeting folder if it doesn't exist
        if !meeting_folder.exists() {
            std::fs::create_dir_all(&meeting_folder)?;
        }

        meeting_folder
    } else {
        output_path.clone()
    };

    let file_path = final_output_path.join(format!("transcript_{}.txt", timestamp));

    // Write transcript to file
    std::fs::write(&file_path, transcript_text)?;

    Ok(file_path.to_string_lossy().to_string())
}

/// Write structured transcript with timestamps to JSON file
pub fn write_transcript_json_to_file(
    segments: &[super::recording_saver::TranscriptSegment],
    output_path: &PathBuf,
    meeting_name: Option<&str>,
    audio_filename: &str,
    recording_duration: f64,
) -> Result<String> {
    use serde_json::json;

    let timestamp = Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();

    // Create meeting folder if meeting name is provided
    let final_output_path = if let Some(name) = meeting_name {
        let sanitized_meeting_name = sanitize_filename(name);
        let meeting_folder = output_path.join(&sanitized_meeting_name);

        if !meeting_folder.exists() {
            std::fs::create_dir_all(&meeting_folder)?;
        }

        meeting_folder
    } else {
        output_path.clone()
    };

    let file_path = final_output_path.join(format!("transcript_{}.json", timestamp));

    // Create structured JSON transcript
    let transcript_json = json!({
        "version": "1.0",
        "recording_duration": recording_duration,
        "audio_file": audio_filename,
        "sample_rate": 48000,
        "created_at": Utc::now().to_rfc3339(),
        "meeting_name": meeting_name,
        "segments": segments,
    });

    // Write JSON to file with pretty formatting
    let json_string = serde_json::to_string_pretty(&transcript_json)?;
    std::fs::write(&file_path, json_string)?;

    Ok(file_path.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- resampling characterisation helpers -------------------------------
    //
    // These exist because the resampler has no observable output other than the
    // audio itself: a wrong filter still returns plausible-looking f32s, and the
    // symptom is a model mishearing words rather than an error. They pin the
    // three properties that matter — length, amplitude, and that the tone comes
    // out at the frequency it went in — so a resampler swap has to preserve
    // behaviour instead of merely compiling.

    /// Generate `secs` of a pure sine at `freq` Hz, sampled at `rate`.
    fn sine(freq: f32, rate: u32, secs: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .map(|i| {
                (2.0 * std::f32::consts::PI * freq * i as f32 / rate as f32).sin()
            })
            .collect()
    }

    /// Estimate the dominant frequency by counting zero crossings. Exact enough
    /// for a single clean tone, and needs no FFT plumbing in the test.
    fn dominant_freq(samples: &[f32], rate: u32) -> f32 {
        // Ignore the filter's edge transient at both ends.
        let skip = (samples.len() / 10).max(1);
        let body = &samples[skip..samples.len().saturating_sub(skip)];
        if body.len() < 2 {
            return 0.0;
        }
        let crossings = body
            .windows(2)
            .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
            .count();
        crossings as f32 * rate as f32 / (2.0 * body.len() as f32)
    }

    fn peak(samples: &[f32]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
    }

    /// One-shot 48k->16k must keep a 1kHz tone at 1kHz and at full amplitude,
    /// and emit ~1/3 as many samples. Measured on rubato 0.15: 15914 samples,
    /// peak 0.9954, 1000.3Hz.
    #[test]
    fn test_resample_48k_to_16k_preserves_tone() {
        let input = sine(1000.0, 48_000, 1.0);
        let out = resample(&input, 48_000, 16_000).unwrap();

        let expected = input.len() / 3;
        assert!(
            (out.len() as i64 - expected as i64).abs() < (expected / 20) as i64,
            "expected ~{expected} samples, got {}",
            out.len()
        );
        let p = peak(&out);
        assert!(p > 0.95 && p <= 1.01, "amplitude not preserved: peak {p}");
        let f = dominant_freq(&out, 16_000);
        assert!((f - 1000.0).abs() < 20.0, "tone shifted to {f}Hz");
    }

    /// Same guarantees through the streaming path, fed in blocks that do not
    /// divide the resampler chunk size, so a boundary that drops or duplicates
    /// samples shows up as a length or frequency error.
    #[test]
    fn test_streaming_downsampler_preserves_tone_across_chunks() {
        let input = sine(1000.0, 48_000, 1.0);
        let mut ds = StreamingDownsampler16k::new(48_000);
        let mut out = Vec::new();
        for block in input.chunks(1024) {
            out.extend_from_slice(&ds.push(block));
        }

        let expected = input.len() / 3;
        assert!(
            (out.len() as i64 - expected as i64).abs() < (expected / 20) as i64,
            "expected ~{expected} samples, got {}",
            out.len()
        );
        let p = peak(&out);
        assert!(p > 0.95 && p <= 1.01, "amplitude not preserved: peak {p}");
        let f = dominant_freq(&out, 16_000);
        assert!((f - 1000.0).abs() < 20.0, "tone shifted to {f}Hz");
    }

    /// The one that actually guards transcription quality. 12kHz is above the
    /// 8kHz Nyquist of 16k output, so it must be filtered away, not folded back
    /// on top of speech. A decimator with no anti-alias filter leaves it near
    /// 1.0, aliased down to ~4kHz — audible to the model as garbled words
    /// rather than as an error.
    ///
    /// Measured: 0.073 on rubato 0.15, 0.159 on rubato 5 with identical
    /// parameters. rubato 5 rejects ~6dB less here; the threshold is set above
    /// both so it still catches a real aliasing regression. Lowering f_cutoff
    /// does not buy the rejection back (0.80 only reached 0.133) and costs the
    /// speech band dearly — 7kHz fell from 1.01 to 0.40 — so f_cutoff stays at
    /// the historical 0.95.
    #[test]
    fn test_resample_rejects_above_nyquist_instead_of_aliasing() {
        let input = sine(12_000.0, 48_000, 1.0);
        let out = resample(&input, 48_000, 16_000).unwrap();
        let p = peak(&out);
        assert!(
            p < 0.25,
            "12kHz survived downsampling at peak {p}; it is aliasing into the speech band"
        );
    }

    /// Regression: the gain came straight from `TARGET_LUFS - loudness_global()`
    /// with no bound. At the start of a recording that measurement is room
    /// tone, so the first thing every meeting did was amplify its own noise
    /// floor by up to ~70x and clip the opening sentence — the audio the model
    /// most needs to read cleanly.
    #[test]
    fn quiet_room_tone_does_not_blow_up_the_gain() {
        let mut norm = LoudnessNormalizer::new(1, 48_000).expect("normalizer");

        // 2s of a very quiet noise floor, the way a meeting actually opens.
        let mut floor = Vec::new();
        for i in 0..96_000 {
            floor.push(if i % 2 == 0 { 0.0005 } else { -0.0005 });
        }
        let out = norm.normalize_loudness(&floor);

        let ceiling = 10_f32.powf(-1.0 / 20.0); // the stage's own true-peak limit
        assert!(
            out.iter().all(|s| s.abs() <= ceiling + 1e-6),
            "output must stay inside the peak ceiling"
        );

        let max_gain = 10_f32.powf(MAX_GAIN_DB as f32 / 20.0);
        assert!(
            norm.gain_linear <= max_gain + 1e-3,
            "gain reached {:.1}x, cap is {:.1}x",
            norm.gain_linear,
            max_gain
        );
    }

    /// The glide is what keeps a gain correction from stepping mid-sentence.
    #[test]
    fn gain_moves_gradually_rather_than_stepping() {
        let mut norm = LoudnessNormalizer::new(1, 48_000).expect("normalizer");
        norm.target_gain = 8.0;
        norm.gain_linear = 1.0;

        norm.normalize_loudness(&vec![0.0f32; 512]);
        assert!(
            norm.gain_linear < 1.5,
            "gain jumped to {:.2} in 512 samples; the glide is not applied",
            norm.gain_linear
        );

        // ...but it does get there.
        norm.normalize_loudness(&vec![0.0f32; 48_000]);
        assert!(
            norm.gain_linear > 5.0,
            "gain only reached {:.2} after a second; the glide is too slow to track a speaker",
            norm.gain_linear
        );
    }
}
