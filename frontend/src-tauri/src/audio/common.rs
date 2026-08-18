use crate::api::TranscriptSegment;
use anyhow::Result;
use log::{debug, info};
use once_cell::sync::Lazy;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use uuid::Uuid;

static ENGINE_LIFECYCLE_LOCK: Lazy<Arc<AsyncMutex<()>>> =
    Lazy::new(|| Arc::new(AsyncMutex::new(())));

pub(crate) async fn acquire_engine_lifecycle_lock() -> OwnedMutexGuard<()> {
    ENGINE_LIFECYCLE_LOCK.clone().lock_owned().await
}

/// When the transcription engine was last wanted by anybody.
///
/// `Instant`, not wall clock: the deadline below must not move when the user
/// crosses a DST boundary or NTP steps the clock mid-meeting.
static ENGINE_LAST_USE: Lazy<AsyncMutex<Instant>> = Lazy::new(|| AsyncMutex::new(Instant::now()));

/// How long the loaded transcription model may sit unused before it is dropped.
///
/// Deliberately the sidecar's number (`DEFAULT_IDLE_TIMEOUT_SECS`, 5 minutes)
/// rather than a second one to keep in sync. `TRANSCRIBE_IDLE_TIMEOUT`
/// overrides it, mirroring the sidecar's `LLAMA_IDLE_TIMEOUT`.
fn engine_idle_timeout() -> Duration {
    let secs = std::env::var("TRANSCRIBE_IDLE_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(crate::summary::summary_engine::models::DEFAULT_IDLE_TIMEOUT_SECS);
    Duration::from_secs(secs)
}

/// Push the idle deadline out. Call this whenever the engine is loaded, used,
/// or released — recording start, recording stop, batch engine init.
pub(crate) async fn touch_engine_idle() {
    *ENGINE_LAST_USE.lock().await = Instant::now();
}

/// How long since the last [`touch_engine_idle`].
pub(crate) async fn engine_idle_elapsed() -> Duration {
    ENGINE_LAST_USE.lock().await.elapsed()
}

/// Is a batch job (import or retranscription) holding the engine right now?
fn batch_in_progress() -> bool {
    crate::audio::import::is_import_in_progress()
        || crate::audio::retranscription::is_retranscription_in_progress()
}

/// The decision one tick of the idle unloader makes, as a pure function so the
/// policy is testable without a model, a recording, or a Tauri app.
fn should_unload_idle(
    recording: bool,
    batch_running: bool,
    idle: Duration,
    timeout: Duration,
) -> bool {
    !recording && !batch_running && idle > timeout
}

/// Drop the transcription model's weights once nobody has wanted them for
/// [`engine_idle_timeout`].
///
/// This is not an optimisation, it is the other half of *not* unloading at
/// `stop_recording`. transcribe.cpp does not mmap — it streams the GGUF through
/// `std::ifstream` and copies each tensor in, so the weights are ~716 MB of
/// non-purgeable resident memory that nothing would ever reclaim on its own.
///
/// Modelled on `SidecarManager::start_idle_check_loop`: same timeout, same 60s
/// tick, same `Skip` behaviour so a stalled tick does not fire a burst of
/// catch-up ticks.
pub(crate) fn spawn_engine_idle_unloader() {
    tauri::async_runtime::spawn(async move {
        let timeout = engine_idle_timeout();
        log::info!(
            "Transcription engine idle unloader started (timeout: {}s)",
            timeout.as_secs()
        );

        let mut interval = tokio::time::interval(Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;

            if !should_unload_idle(
                crate::audio::recording_commands::is_recording_now(),
                batch_in_progress(),
                engine_idle_elapsed().await,
                timeout,
            ) {
                continue;
            }

            let engine = {
                let guard = crate::transcribe_engine::commands::TRANSCRIBE_ENGINE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                guard.as_ref().cloned()
            };
            let Some(engine) = engine else { continue };
            if !engine.is_model_loaded().await {
                continue;
            }

            let _engine_lifecycle_guard = acquire_engine_lifecycle_lock().await;

            // Re-check under the lock. Everything above raced whoever else was
            // holding it: a recording that started while we waited would have
            // its weights pulled out from under a live stream. This re-check is
            // the whole correctness argument for running unattended.
            if crate::audio::recording_commands::is_recording_now() || batch_in_progress() {
                continue;
            }

            let model = engine
                .get_current_model()
                .await
                .unwrap_or_else(|| "unknown".to_string());
            if engine.unload_model().await {
                log::info!(
                    "Unloaded transcription model '{}' after {}s idle",
                    model,
                    timeout.as_secs()
                );
            }
        }
    });
}

/// Unload the transcription engine after a batch job (import or retranscription).
/// Skips unloading if a live recording is currently in progress, since recording
/// uses the same global engine instances.
pub(crate) async fn unload_engine_after_batch() {
    let _engine_lifecycle_guard = acquire_engine_lifecycle_lock().await;

    if crate::audio::recording_commands::is_recording().await {
        log::info!("Skipping model unload after batch: recording in progress");
        return;
    }

    use crate::transcribe_engine::commands::TRANSCRIBE_ENGINE;
    let engine = {
        let guard = TRANSCRIBE_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    };
    if let Some(e) = engine {
        e.unload_model().await;
    }
}

/// Create transcript segments from transcription results.
/// Each tuple is (text, start_ms, end_ms, speaker) from VAD timestamps.
pub(crate) fn create_transcript_segments(
    transcripts: &[(String, f64, f64, Option<String>)],
) -> Vec<TranscriptSegment> {
    transcripts
        .iter()
        .map(|(text, start_ms, end_ms, speaker)| {
            let start_seconds = start_ms / 1000.0;
            let end_seconds = end_ms / 1000.0;
            let duration = end_seconds - start_seconds;

            TranscriptSegment {
                id: format!("transcript-{}", Uuid::new_v4()),
                text: text.trim().to_string(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                audio_start_time: Some(start_seconds),
                audio_end_time: Some(end_seconds),
                duration: Some(duration),
                speaker: speaker.clone(),
            }
        })
        .collect()
}

/// Write transcripts.json to a meeting folder (atomic write with temp file)
pub(crate) fn write_transcripts_json(folder: &Path, segments: &[TranscriptSegment]) -> Result<()> {
    let transcript_path = folder.join("transcripts.json");
    let temp_path = folder.join(".transcripts.json.tmp");

    let json = serde_json::json!({
        "version": "1.0",
        "last_updated": chrono::Utc::now().to_rfc3339(),
        "total_segments": segments.len(),
        "segments": segments.iter().enumerate().map(|(i, s)| {
            serde_json::json!({
                "id": s.id,
                "text": s.text,
                "timestamp": s.timestamp,
                "audio_start_time": s.audio_start_time,
                "audio_end_time": s.audio_end_time,
                "duration": s.duration,
                "speaker": s.speaker,
                "sequence_id": i
            })
        }).collect::<Vec<_>>()
    });

    let json_string = serde_json::to_string_pretty(&json)?;
    std::fs::write(&temp_path, &json_string)?;
    std::fs::rename(&temp_path, &transcript_path)?;

    info!(
        "Wrote transcripts.json with {} segments to {}",
        segments.len(),
        transcript_path.display()
    );
    Ok(())
}

pub(crate) fn speaker_label(speaker: &str) -> String {
    if speaker == "you" {
        "You".to_string()
    } else {
        format!("Speaker {speaker}")
    }
}

/// Segments in the (start seconds, text) shape [`write_transcript_md`] takes.

pub(crate) fn markdown_segments(segments: &[TranscriptSegment]) -> Vec<(f64, String)> {
    segments
        .iter()
        .map(|s| {
            let text = match &s.speaker {
                Some(speaker) => format!("{}: {}", speaker_label(speaker), s.text),
                None => s.text.clone(),
            };
            (s.audio_start_time.unwrap_or(0.0), text)
        })
        .collect()
}

/// Render a transcript as markdown: heading, an updated line, then one
/// `[HH:MM:SS] text` paragraph per segment.
pub(crate) fn transcript_markdown(
    meeting_name: Option<&str>,
    segments: &[(f64, String)],
) -> String {
    let mut out = format!("# {}\n\n", meeting_name.unwrap_or("Meeting"));
    out.push_str(&format!(
        "_{} segments · updated {}_\n\n",
        segments.len(),
        chrono::Utc::now().to_rfc3339()
    ));
    for (start, text) in segments {
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "[{}] {}\n\n",
            crate::utils::format_timestamp(*start),
            text
        ));
    }
    out
}

/// Write transcript.md to a meeting folder (atomic write with temp file).
///
/// `meeting_name` falls back to metadata.json when the caller doesn't have it
/// (retranscription only ever runs on a folder that already has one).
pub(crate) fn write_transcript_md(
    folder: &Path,
    meeting_name: Option<&str>,
    segments: &[(f64, String)],
) -> Result<()> {
    let fallback_name = meeting_name.is_none().then(|| meeting_name_from_metadata(folder)).flatten();
    let markdown = transcript_markdown(meeting_name.or(fallback_name.as_deref()), segments);

    let md_path = folder.join("transcript.md");
    let temp_path = folder.join(".transcript.md.tmp");
    std::fs::write(&temp_path, &markdown)?;
    std::fs::rename(&temp_path, &md_path)?;

    debug!("Wrote transcript.md with {} segments", segments.len());
    Ok(())
}

fn meeting_name_from_metadata(folder: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(folder.join("metadata.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("meeting_name")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
}

/// Read the configured local transcription model from the database.
///
/// Only the local transcribe.cpp engine runs file work. A stored built-in audio
/// model is a sidecar model rather than a catalog entry, so it falls back to the
/// default — otherwise selecting Gemma 4 for live recording would break import and
/// retranscription with "Unknown model: gemma4:e4b".
///
/// Was duplicated in import.rs and retranscription.rs.
pub(crate) async fn configured_local_model<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
) -> Result<String> {
    use crate::config::DEFAULT_TRANSCRIBE_MODEL;
    use tauri::Manager;

    let app_state = app
        .try_state::<crate::state::AppState>()
        .ok_or_else(|| anyhow::anyhow!("App state not available"))?;

    let result: Option<(String, String)> =
        sqlx::query_as("SELECT provider, model FROM transcript_settings WHERE id = '1'")
            .fetch_optional(app_state.db_manager.pool())
            .await
            .map_err(|e| anyhow::anyhow!("Failed to query transcript config: {}", e))?;

    Ok(match result {
        Some((provider, _)) if crate::config::is_builtin_transcript_provider(&provider) => {
            log::info!(
                "Transcript provider is the built-in audio LLM, which does not run file \
                 transcription; using local model '{}' instead",
                DEFAULT_TRANSCRIBE_MODEL
            );
            DEFAULT_TRANSCRIBE_MODEL.to_string()
        }
        Some((_, model)) if !model.is_empty() => model,
        _ => {
            log::warn!(
                "No transcript config found, using default model '{}'",
                DEFAULT_TRANSCRIBE_MODEL
            );
            DEFAULT_TRANSCRIBE_MODEL.to_string()
        }
    })
}

/// Longest speech segment handed to one batch transcription, for file work.
///
/// Import and retranscription optimize for accuracy — a longer segment gives the
/// model more context and costs nobody anything, since the file is already on
/// disk. Was duplicated as a local const in both import.rs and retranscription.rs.
pub(crate) const MAX_SEGMENT_SAMPLES: usize = 25 * 16000;

/// Same cap for live recording, where it is also the latency floor: nothing
/// appears on screen until the segment it belongs to has been decoded. Traded
/// down from 25s for that reason.
///
/// `split_segment_at_silence` searches +/-3s for a quiet cut, so real segments
/// land in the 5-11s range rather than exactly 8.
pub(crate) const LIVE_MAX_SEGMENT_SAMPLES: usize = 8 * 16000;

pub(crate) const DIARIZED_MAX_SEGMENT_SAMPLES: usize = 30 * 16000;

/// Split a long speech segment at the lowest-energy (silence) point near the target size.
///
/// Scans for 100ms windows with minimal RMS energy within +/-3 seconds of each target
/// split point. If no clear silence is found, falls back to a 1-second overlap split
/// to avoid cutting words at boundaries.
pub(crate) fn split_segment_at_silence(
    segment: &crate::audio::vad::SpeechSegment,
    max_samples: usize,
) -> Vec<crate::audio::vad::SpeechSegment> {
    const SAMPLE_RATE: usize = 16000;
    // 100ms window for energy measurement (1600 samples at 16kHz)
    const ENERGY_WINDOW: usize = SAMPLE_RATE / 10;
    // Search +/-3 seconds around the target split point
    const SEARCH_RADIUS: usize = SAMPLE_RATE * 3;
    // RMS threshold below which we consider a window "silent"
    const SILENCE_RMS_THRESHOLD: f32 = 0.02;
    // Overlap to use when no silence boundary is found (1 second)
    const FALLBACK_OVERLAP: usize = SAMPLE_RATE;

    let total = segment.samples.len();
    if total <= max_samples {
        return vec![segment.clone()];
    }

    let ms_per_sample = (segment.end_timestamp_ms - segment.start_timestamp_ms)
        / segment.samples.len() as f64;
    let mut result = Vec::new();
    let mut pos = 0usize;

    while pos < total {
        let remaining = total - pos;
        if remaining <= max_samples {
            // Last chunk - take everything remaining
            let chunk_samples = segment.samples[pos..].to_vec();
            let chunk_start_ms = segment.start_timestamp_ms + (pos as f64 * ms_per_sample);
            let chunk_end_ms = segment.end_timestamp_ms;
            result.push(crate::audio::vad::SpeechSegment {
                samples: chunk_samples,
                start_timestamp_ms: chunk_start_ms,
                end_timestamp_ms: chunk_end_ms,
                confidence: segment.confidence,
            });
            break;
        }

        // Target split point
        let target = pos + max_samples;

        // Search window: [target - SEARCH_RADIUS, target + SEARCH_RADIUS]
        let search_start = target.saturating_sub(SEARCH_RADIUS).max(pos + SAMPLE_RATE);
        let search_end = (target + SEARCH_RADIUS).min(total.saturating_sub(ENERGY_WINDOW));

        // Find the lowest-energy 100ms window in the search range
        let mut best_split = target.min(total); // fallback: exact target
        let mut best_rms = f32::MAX;

        if search_start + ENERGY_WINDOW <= search_end {
            let mut idx = search_start;
            while idx + ENERGY_WINDOW <= search_end {
                let window = &segment.samples[idx..idx + ENERGY_WINDOW];
                let rms = (window.iter().map(|s| s * s).sum::<f32>() / ENERGY_WINDOW as f32).sqrt();
                if rms < best_rms {
                    best_rms = rms;
                    best_split = idx + ENERGY_WINDOW / 2; // split at center of quiet window
                }
                // Step by 10ms (160 samples) for efficiency
                idx += SAMPLE_RATE / 100;
            }
        }

        let split_at = best_split;
        if best_rms <= SILENCE_RMS_THRESHOLD {
            debug!(
                "Splitting at silence boundary: sample {} (RMS={:.4})",
                split_at, best_rms
            );
        } else {
            debug!(
                "No silence found near target (best RMS={:.4}), splitting with overlap at sample {}",
                best_rms, split_at
            );
        }

        // Determine the actual end of this chunk (with overlap if no silence)
        let chunk_end = if best_rms > SILENCE_RMS_THRESHOLD {
            (split_at + FALLBACK_OVERLAP).min(total)
        } else {
            split_at
        };

        let chunk_samples = segment.samples[pos..chunk_end].to_vec();
        let chunk_start_ms = segment.start_timestamp_ms + (pos as f64 * ms_per_sample);
        let chunk_end_ms = segment.start_timestamp_ms + (chunk_end as f64 * ms_per_sample);

        result.push(crate::audio::vad::SpeechSegment {
            samples: chunk_samples,
            start_timestamp_ms: chunk_start_ms,
            end_timestamp_ms: chunk_end_ms,
            confidence: segment.confidence,
        });

        // Advance position to where the current chunk actually ends
        // to avoid transcribing the overlap region twice
        pos = chunk_end;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_engine_lifecycle_lock_serializes_acquirers() {
        let guard = acquire_engine_lifecycle_lock().await;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (acquired_tx, mut acquired_rx) = tokio::sync::oneshot::channel();
        let waiter = tokio::spawn(async {
            started_tx.send(()).unwrap();
            let _guard = acquire_engine_lifecycle_lock().await;
            acquired_tx.send(()).unwrap();
        });

        started_rx.await.unwrap();
        assert!(acquired_rx.try_recv().is_err());
        drop(guard);

        acquired_rx.await.unwrap();
        waiter.await.unwrap();
    }

    const FIVE_MIN: Duration = Duration::from_secs(300);

    /// The reason the re-check under the lifecycle lock exists: a live stream
    /// pins the weights, so idleness is never on its own a reason to unload.
    #[test]
    fn idle_unloader_never_unloads_while_recording() {
        assert!(
            !should_unload_idle(true, false, Duration::from_secs(9999), FIVE_MIN),
            "recording outranks any amount of idle time"
        );
        assert!(
            !should_unload_idle(false, true, Duration::from_secs(9999), FIVE_MIN),
            "so does a batch job — import and retranscription hold the same engine"
        );
    }

    #[test]
    fn idle_unloader_unloads_once_the_timeout_passes() {
        assert!(
            !should_unload_idle(false, false, Duration::from_secs(299), FIVE_MIN),
            "still inside the window"
        );
        assert!(should_unload_idle(
            false,
            false,
            Duration::from_secs(301),
            FIVE_MIN
        ));
    }

    /// Any use of the engine has to reset the clock, or a long meeting followed
    /// by a short pause would look idle enough to unload.
    #[tokio::test]
    async fn touch_engine_idle_pushes_the_deadline_out() {
        *ENGINE_LAST_USE.lock().await = Instant::now() - Duration::from_secs(3600);
        let stale = engine_idle_elapsed().await;
        assert!(should_unload_idle(false, false, stale, FIVE_MIN));

        touch_engine_idle().await;

        let fresh = engine_idle_elapsed().await;
        assert!(fresh < Duration::from_secs(1), "{fresh:?}");
        assert!(!should_unload_idle(false, false, fresh, FIVE_MIN));
    }

    #[test]
    fn transcript_markdown_has_heading_and_stamped_segments() {
        let md = transcript_markdown(
            Some("Team Standup"),
            &[
                (0.0, "Hello everyone.".to_string()),
                (3725.5, "  Wrapping up.  ".to_string()),
                (10.0, "   ".to_string()),
            ],
        );

        assert!(md.starts_with("# Team Standup\n\n"), "{md}");
        assert!(md.contains("[00:00:00] Hello everyone.\n\n"), "{md}");
        assert!(md.contains("[01:02:05] Wrapping up.\n\n"), "{md}");
        assert!(!md.contains("[00:00:10]"), "blank segments are skipped: {md}");
    }

    #[test]
    fn transcript_markdown_falls_back_to_generic_heading() {
        assert!(transcript_markdown(None, &[]).starts_with("# Meeting\n\n"));
    }

    /// Retranscription passes no name, so the heading comes from metadata.json.
    #[test]
    fn write_transcript_md_takes_name_from_metadata() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("metadata.json"),
            r#"{"meeting_name": "Design Review"}"#,
        )
        .unwrap();

        write_transcript_md(dir.path(), None, &[(0.0, "Hello.".to_string())]).unwrap();

        let md = std::fs::read_to_string(dir.path().join("transcript.md")).unwrap();
        assert!(md.starts_with("# Design Review\n\n"), "{md}");
        assert!(md.contains("[00:00:00] Hello.\n\n"), "{md}");
    }
}
