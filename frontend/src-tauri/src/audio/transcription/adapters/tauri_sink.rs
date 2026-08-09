// audio/transcription/adapters/tauri_sink.rs
//
// Driven adapter: the only place in live transcription that knows Tauri exists.
//
// Everything about how transcript text reaches the user — event names, payload
// shapes, sequence numbering, wall-clock formatting — lives here, so the
// decoders and the use case can be read (and tested) without any of it.

use crate::audio::transcription::ports::{TranscriptChunk, TranscriptSink};
use log::error;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tauri::{AppHandle, Emitter, Runtime};

static SEQUENCE_COUNTER: AtomicU64 = AtomicU64::new(0);
static SPEECH_DETECTED_EMITTED: AtomicBool = AtomicBool::new(false);

/// Reset per-session state for a new recording.
pub fn reset_speech_detected_flag() {
    SPEECH_DETECTED_EMITTED.store(false, Ordering::SeqCst);
}

/// Emitted per committed chunk. Field-for-field what the frontend and the
/// recording manager's persistence listener already consume.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TranscriptUpdate {
    pub text: String,
    pub timestamp: String, // Wall-clock time for reference (e.g., "14:30:05")
    pub source: String,
    pub sequence_id: u64,
    pub chunk_start_time: f64, // Legacy field, kept for compatibility
    pub is_partial: bool,
    /// Omitted entirely when the decoder reports none, so the UI's confidence
    /// badge does not render. A synthetic 1.0 would paint a green
    /// high-confidence badge on text that nothing actually scored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    pub audio_start_time: f64, // Seconds from recording start
    pub audio_end_time: f64,   // Seconds from recording start
    pub duration: f64,         // Segment duration in seconds
}

/// The volatile live-text event. Deliberately a different event from
/// `transcript-update`: anything listening to `transcript-update` persists it,
/// and this text is not final.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TranscriptPartial {
    pub text: String,
    /// Monotonic stream revision, so a late event can be discarded.
    pub revision: i32,
}

pub struct TauriSink<R: Runtime> {
    app: AppHandle<R>,
}

impl<R: Runtime> TauriSink<R> {
    pub fn new(app: AppHandle<R>) -> Self {
        Self { app }
    }

    /// The live transcript is over for this recording. The recording itself is
    /// not: capture, mixing and the WAV file all carry on, and that is the
    /// right outcome — the meeting is still being saved and can be transcribed
    /// from the file afterwards. Tearing down someone's meeting because a model
    /// failed to load would turn a recoverable problem into lost audio.
    ///
    /// What was wrong here was the wording: it used to say "Recording failed",
    /// which is not what happened, so the user either stopped a recording that
    /// was working or assumed nothing was being captured at all.
    pub fn fatal(app: &AppHandle<R>, message: &str) {
        error!("{message}");
        let _ = app.emit(
            "transcription-error",
            serde_json::json!({
                "error": message,
                "userMessage": format!(
                    "Live transcription stopped: {message}. \
                     The meeting is still recording and the audio is being saved — \
                     you can transcribe it from the recording once this is fixed."
                ),
                "actionable": true
            }),
        );
    }
}

impl<R: Runtime> TranscriptSink for TauriSink<R> {
    fn committed(&mut self, chunk: TranscriptChunk) {
        if !SPEECH_DETECTED_EMITTED.swap(true, Ordering::SeqCst) {
            let _ = self.app.emit(
                "speech-detected",
                serde_json::json!({ "message": "Speech activity detected" }),
            );
        }

        let update = TranscriptUpdate {
            timestamp: format_current_timestamp(),
            source: "Audio".to_string(),
            sequence_id: SEQUENCE_COUNTER.fetch_add(1, Ordering::SeqCst),
            chunk_start_time: chunk.audio_start,
            // Both decode paths only emit text the model considers final.
            is_partial: false,
            confidence: chunk.confidence,
            audio_start_time: chunk.audio_start,
            audio_end_time: chunk.audio_end,
            duration: chunk.duration(),
            text: chunk.text,
        };

        if let Err(e) = self.app.emit("transcript-update", &update) {
            error!("Failed to emit transcript update: {e}");
        }
    }

    fn tentative(&mut self, text: &str) {
        let _ = self.app.emit(
            "transcript-partial",
            TranscriptPartial {
                text: text.to_string(),
                // Partials are emitted from one blocking thread, so they arrive
                // in order and the frontend ignores this. It exists so a future
                // out-of-order source can be discarded rather than flicker.
                revision: 0,
            },
        );
    }

    fn warn(&mut self, message: &str) {
        let _ = self.app.emit("transcription-warning", message.to_string());
    }
}

fn format_current_timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let hours = (now.as_secs() / 3600) % 24;
    let minutes = (now.as_secs() / 60) % 60;
    let seconds = now.as_secs() % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}
