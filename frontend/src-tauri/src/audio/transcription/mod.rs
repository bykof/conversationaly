// audio/transcription/mod.rs
//
// Live transcription, arranged as a hexagon:
//
//   ports.rs      the two boundaries — Transcriber (in), TranscriptSink (out)
//   service.rs    the use case, which knows only those two traits
//   adapters/     transcribe.cpp, Silero, the sidecar, and Tauri events
//   mod.rs        this file: the composition root that picks the adapters
//
// Only this file knows which backend a given recording will use, and only
// `adapters/tauri_sink.rs` knows that a UI exists. That split is the point: the
// live loop used to be one 700-line function that could not run without a real
// meeting, a real model and a real Tauri app, so nothing in it was ever tested
// and the bugs that mattered were the ones only a user could find.

pub mod adapters;
pub mod ports;
pub mod service;

use adapters::bench_sink::BenchSink;
use adapters::segmented::{Decoder, SegmentedTranscriber};
use adapters::streaming::StreamingTranscriber;
use adapters::tauri_sink::TauriSink;
use crate::audio::AudioChunk;
use crate::transcribe_engine::TRANSCRIBE_ENGINE;
use log::{error, info};
use tauri::{AppHandle, Runtime};
use transcribe_cpp::{RunOptions, StreamOptions};

pub use adapters::tauri_sink::{reset_speech_detected_flag, TranscriptPartial, TranscriptUpdate};

/// Start live transcription for a recording.
///
/// Picking a backend goes provider first, then model capability:
///   - `builtin-ai` means an audio-capable LLM in the llama-helper sidecar,
///     which has no transcribe.cpp session at all — so it is checked first.
///   - otherwise `Capabilities::supports_streaming` on the loaded model
///     decides. That comes from GGUF metadata, so it cannot disagree with what
///     the model can actually do; the catalog's `streaming` field only labels
///     rows in the picker before download.
pub fn start_transcription_task<R: Runtime>(
    app: AppHandle<R>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        reset_speech_detected_flag();

        if let Some((provider, model)) = transcript_provider_config(&app).await {
            if crate::config::is_builtin_transcript_provider(&provider) {
                run_builtin_audio_llm(app, receiver, model).await;
                return;
            }
        }

        let engine = {
            let guard = TRANSCRIBE_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().cloned()
        };
        let Some(engine) = engine else {
            TauriSink::fatal(&app, "Transcription engine not initialized");
            return;
        };

        if !engine.is_model_loaded().await {
            TauriSink::fatal(&app, "No transcription model is loaded");
            return;
        }

        // Resolved against the loaded model, not passed raw: the picker stores
        // "de" and a model may advertise "de-DE", which transcribe.cpp rejects
        // outright — killing the stream before a single sample decodes.
        let language = engine
            .resolve_language(crate::get_language_preference_internal())
            .await;

        let session = match engine.open_session().await {
            Ok(s) => s,
            Err(e) => {
                TauriSink::fatal(&app, &format!("Failed to open transcription session: {e}"));
                return;
            }
        };

        let streaming = session.model().capabilities().supports_streaming;
        info!(
            "🎙️ Live transcription starting (model {:?}, language {:?}, path {})",
            engine.get_current_model().await,
            language,
            if streaming { "stream" } else { "VAD + batch" }
        );

        // feed()/run()/finalize() are blocking native calls, so the whole loop
        // lives on a blocking thread rather than stalling the async reactor.
        let joined = tokio::task::spawn_blocking(move || {
            let mut session = session;
            let run_options = RunOptions { language, ..Default::default() };
            // One wrap here covers both decode paths below, because both reach
            // the user through this sink. Instrumentation belongs at the
            // composition root for the same reason adapter choice does.
            let sink = BenchSink::new(TauriSink::new(app.clone()));

            if streaming {
                match session.stream(&run_options, &StreamOptions::default()) {
                    // Only this path opts into the lag warning. The segmented
                    // adapter below already warns when its backlog cap starts
                    // dropping audio; the streaming path has no cap, so nothing
                    // otherwise tells a user that the transcript has quietly
                    // drifted half a minute behind them.
                    Ok(stream) => service::run(
                        StreamingTranscriber::new(stream),
                        sink.warn_when_behind(),
                        receiver,
                    ),
                    Err(e) => {
                        TauriSink::fatal(&app, &format!("Failed to begin transcription stream: {e}"))
                    }
                }
            } else {
                match SegmentedTranscriber::new(Decoder::Local { session, run_options }) {
                    Ok(transcriber) => service::run(transcriber, sink, receiver),
                    Err(e) => {
                        TauriSink::fatal(&app, &format!("Failed to start speech detection: {e}"))
                    }
                }
            }
        })
        .await;
        if let Err(e) = joined {
            error!("Transcription task panicked: {e}");
        }
    })
}

/// Read the configured transcript provider and model, if any is stored.
async fn transcript_provider_config<R: Runtime>(app: &AppHandle<R>) -> Option<(String, String)> {
    use tauri::Manager;
    match crate::api::api::api_get_transcript_config(app.clone(), app.state(), None).await {
        Ok(Some(config)) if !config.provider.is_empty() => Some((config.provider, config.model)),
        _ => None,
    }
}

/// Live transcription through an audio-capable LLM in the built-in sidecar.
///
/// Uses the same segmented adapter as a local batch model: an LLM decode per
/// utterance needs exactly the same VAD segmentation, backlog cap and emit
/// path. Only the call that turns samples into a string differs.
async fn run_builtin_audio_llm<R: Runtime>(
    app: AppHandle<R>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<AudioChunk>,
    model: String,
) {
    use crate::config::DEFAULT_BUILTIN_TRANSCRIBE_MODEL;
    use tauri::Manager;

    let model = if model.is_empty() {
        DEFAULT_BUILTIN_TRANSCRIBE_MODEL.to_string()
    } else {
        model
    };

    let app_data_dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(e) => {
            TauriSink::fatal(&app, &format!("Could not resolve the app data directory: {e}"));
            return;
        }
    };

    // Fail once, up front, with something the user can act on — rather than the
    // same "projector missing" warning for every utterance.
    match crate::summary::summary_engine::models::get_mmproj_path(&app_data_dir, &model) {
        Ok(Some(path)) if path.exists() => {}
        Ok(Some(path)) => {
            TauriSink::fatal(
                &app,
                &format!(
                    "{model} is not fully downloaded — its audio projector is missing from {}",
                    path.display()
                ),
            );
            return;
        }
        Ok(None) => {
            TauriSink::fatal(&app, &format!("{model} cannot transcribe audio"));
            return;
        }
        Err(e) => {
            TauriSink::fatal(&app, &e.to_string());
            return;
        }
    }

    info!("🎙️ Live transcription via built-in audio model {model}");

    // The sidecar call is async and the service loop is blocking. A blocking
    // thread is exactly where Handle::block_on is legal, so the decoder carries
    // the handle rather than the whole loop becoming async.
    let handle = tokio::runtime::Handle::current();
    let joined = tokio::task::spawn_blocking(move || {
        let sink = BenchSink::new(TauriSink::new(app.clone()));
        let decoder = Decoder::AudioLlm { handle, app_data_dir, model };
        match SegmentedTranscriber::new(decoder) {
            Ok(transcriber) => service::run(transcriber, sink, receiver),
            Err(e) => TauriSink::fatal(&app, &format!("Failed to start speech detection: {e}")),
        }
    })
    .await;
    if let Err(e) = joined {
        error!("Transcription task panicked: {e}");
    }
}
