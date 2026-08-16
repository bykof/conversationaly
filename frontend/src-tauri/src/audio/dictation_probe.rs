// audio/dictation_probe.rs
//
// A measurement, not a feature. This is not dictation and must not grow into
// it: it exists to produce the one number nobody in this repo has, and then to
// be deleted or kept as an instrument.
//
// ## The open question
//
// Decode is the cheap half. At RTF ~0.06 a five-second utterance finalizes in
// ~300ms on an already-loaded model, and Wave 1's `BENCH stream:` line already
// reports that from real meetings. Capture *start* is the unknown. Reading the
// code it looks like 300-800ms — device resolution, cpal stream construction, a
// 50ms mixing window in the pipeline and a 512-sample resampler fill — but that
// is an estimate assembled from reading, not a measurement, and the parts of it
// that dominate (Core Audio handing over a Bluetooth input) are exactly the
// parts source cannot tell you.
//
// ## EXIT CRITERION — written before the body, and before any number existed
//
//   If first-chunk exceeds ~250 ms on a machine with Bluetooth audio paired,
//   dictation requires a prewarmed capture path — FluidVoice spent ~1,750 lines
//   on theirs — and the bet is XXL, not XL.
//
// Stated in advance because that is the only way it decides anything. A
// threshold argued for after seeing the result is not a threshold, it is a
// rationalisation with a number in it, and 250ms is close enough to the
// estimate's floor that either side of it is genuinely reachable.
//
// ## How a human runs it
//
// Only compiled in debug builds (`#[cfg(debug_assertions)]` covers this module,
// the command, and its `generate_handler!` entry), so it cannot ship. Start the
// app with `./clean_run.sh`, load a *streaming* transcription model, then from
// the devtools console (Cmd+Shift+I):
//
//     await __TAURI__.core.invoke('dictation_probe')
//
// and speak a short phrase immediately. The returned string and the
// `BENCH dictation_probe:` line in the log carry the same numbers. Run it once
// with a Bluetooth headset as the default input and once with the built-in mic;
// the gap between those two is the actual finding.
//
// ## What it deliberately does not do
//
// No hotkey, no UI, no text injection, no new dependency, no new model, and no
// prewarming — prewarming is the thing being *costed*, and cpal cannot express
// prepare-without-start anyway. It also does not touch `IS_RECORDING` or the
// global `RECORDING_MANAGER`: it builds its own `RecordingManager`, drops it on
// every exit path, and refuses to run at all while a meeting is recording.
//
// That it can reuse `StreamingTranscriber` and `service::run` unmodified, with a
// throwaway `Vec`-collecting sink, is the hexagon's port claim being cashed
// rather than asserted. Nothing in `audio/transcription/` changed for this.

use crate::audio::recording_commands::is_recording_now;
use crate::audio::transcription::adapters::streaming::StreamingTranscriber;
use crate::audio::transcription::ports::{TranscriptChunk, TranscriptSink};
use crate::audio::transcription::service;
use crate::audio::{default_input_device, AudioChunk, RecordingManager};
use crate::transcribe_engine::TRANSCRIBE_ENGINE;
use log::{info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use transcribe_cpp::{RunOptions, StreamOptions};

/// Longest the microphone may stay open waiting for a first committed word.
///
/// A probe that leaks an open mic is worse than no probe, so the wait is
/// bounded rather than "until something is said". Silence for this long is
/// itself a result: it means first-text never arrived, which the report says.
const PROBE_WINDOW: Duration = Duration::from_secs(10);

/// Grace for the decoder to drain and finalize after capture stops.
const FINALIZE_TIMEOUT: Duration = Duration::from_secs(15);

/// One probe at a time. Two concurrent probes would open two microphones and
/// the second `Stream` would fail with `Error::Busy` anyway — transcribe.cpp
/// allows one in-flight compute per `Model`.
static PROBE_RUNNING: AtomicBool = AtomicBool::new(false);

/// Releases [`PROBE_RUNNING`] however the command exits, panic included.
struct ProbeGuard;

impl ProbeGuard {
    fn acquire() -> Option<Self> {
        PROBE_RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| ProbeGuard)
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        PROBE_RUNNING.store(false, Ordering::SeqCst);
    }
}

/// The instants the probe exists to collect. Monotonic, never wall clock: a
/// latency read across an NTP step is a fiction, and this one is load-bearing.
#[derive(Default)]
struct Marks {
    /// When the first audio chunk reached us — the number the exit criterion
    /// is about.
    first_chunk: Mutex<Option<Instant>>,
    /// When the decoder first committed text.
    first_commit: Mutex<Option<Instant>>,
    text: Mutex<Vec<String>>,
}

impl Marks {
    fn mark(slot: &Mutex<Option<Instant>>) {
        let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert_with(Instant::now);
    }

    fn since(slot: &Mutex<Option<Instant>>, t0: Instant) -> Option<u128> {
        let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.map(|at| at.duration_since(t0).as_millis())
    }
}

/// Collects transcript text into a `Vec` and timestamps the first commit.
///
/// The whole driven side of the hexagon, for a throwaway measurement, in nine
/// lines — which is the claim `ports.rs` makes about itself, tested here by
/// something that is not a test.
struct ProbeSink(Arc<Marks>);

impl TranscriptSink for ProbeSink {
    fn committed(&mut self, chunk: TranscriptChunk) {
        Marks::mark(&self.0.first_commit);
        self.0.text.lock().unwrap_or_else(|e| e.into_inner()).push(chunk.text);
    }

    // Deliberately dropped. Tentative text lands earlier than committed text and
    // would flatter the measurement; dictation pastes final words, so final
    // words are what gets timed.
    fn tentative(&mut self, _text: &str) {}

    fn warn(&mut self, message: &str) {
        warn!("dictation probe: {message}");
    }
}

/// Measure how long the capture path takes to produce first audio and first text.
///
/// Returns a one-line summary; the same numbers go to the log at `info!`.
#[tauri::command]
pub async fn dictation_probe() -> Result<String, String> {
    let t0 = Instant::now();

    // Refuse rather than disturb a meeting: a second mic stream and a second
    // `Stream` on the same `Model` are both real damage, and a probe that can
    // do that to a recording is not a probe you can leave in the build.
    if is_recording_now() {
        return Err("A recording is in progress — the probe would open a second microphone \
                    and a second stream on the same model. Stop the recording first."
            .to_string());
    }
    let _probe_guard = ProbeGuard::acquire()
        .ok_or_else(|| "A dictation probe is already running".to_string())?;

    // Checked before anything is opened, so a missing model costs no microphone.
    let engine = {
        let guard = TRANSCRIBE_ENGINE.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().cloned()
    }
    .ok_or_else(|| "Transcription engine not initialized".to_string())?;
    if !engine.is_model_loaded().await {
        return Err("No transcription model is loaded — the probe measures capture start \
                    against a warm model, so load one first."
            .to_string());
    }
    let language = engine
        .resolve_language(crate::get_language_preference_internal())
        .await;

    // Its own manager, never the global one. Everything below runs between here
    // and the unconditional teardown that follows.
    let mut manager = RecordingManager::new();
    let outcome = measure(&mut manager, &engine, language, t0).await;

    // Every exit path, including the error ones and the no-audio-ever one.
    // `stop_streams_only` is idempotent (`measure` already called it on the
    // happy path), and dropping the manager clears the recording state again.
    if let Err(e) = manager.stop_streams_only().await {
        warn!("dictation probe: teardown reported {e}");
    }
    drop(manager);

    outcome
}

/// The measured part, with the microphone open. Its caller guarantees teardown.
async fn measure(
    manager: &mut RecordingManager,
    engine: &Arc<crate::transcribe_engine::TranscribeEngine>,
    language: Option<String>,
    t0: Instant,
) -> Result<String, String> {
    // The default input device, not the macOS built-in override the meeting
    // path applies. Dictation would use whatever the user is actually wearing,
    // and a Bluetooth headset is the case the exit criterion names.
    let mic = default_input_device().map_err(|e| format!("No microphone available: {e}"))?;
    let mic_name = mic.name.clone();

    // Mic only, no system audio, no saver. `auto_save: false` with no meeting
    // name set means `RecordingSaver` writes nothing and creates no folder.
    let capture = manager
        .start_recording(Some(Arc::new(mic)), None, false)
        .await
        .map_err(|e| format!("Failed to start capture: {e}"))?;
    let streams_up_ms = t0.elapsed().as_millis();

    let marks = Arc::new(Marks::default());

    // Interposed between capture and `service::run` purely to timestamp the
    // first chunk — the service consumes the receiver, and the sink sees text,
    // not audio, so there is nowhere else this instant can be taken.
    let (tx, rx) = mpsc::unbounded_channel::<AudioChunk>();
    let forwarder_marks = marks.clone();
    let forwarder = tokio::spawn(async move {
        let mut capture = capture;
        while let Some(chunk) = capture.recv().await {
            Marks::mark(&forwarder_marks.first_chunk);
            if tx.send(chunk).is_err() {
                break;
            }
        }
    });

    let session = engine
        .open_session()
        .await
        .map_err(|e| format!("Failed to open transcription session: {e}"))?;
    if !session.model().capabilities().supports_streaming {
        return Err("The loaded model does not stream. The probe times first *committed* text, \
                    and the VAD + batch path's latency floor is its 8s segment length, which \
                    would measure the segmenter rather than the capture path."
            .to_string());
    }

    // Real adapters, unmodified. Blocking, so a blocking thread.
    let sink_marks = marks.clone();
    let decoding = tokio::task::spawn_blocking(move || {
        let mut session = session;
        let run_options = RunOptions { language, ..Default::default() };
        // Semicolon load-bearing: without it the match is the closure's tail
        // expression, so its temporary `Result<Stream<'_>, _>` outlives
        // `session` — which the stream borrows.
        match session.stream(&run_options, &StreamOptions::default()) {
            Ok(stream) => service::run(StreamingTranscriber::new(stream), ProbeSink(sink_marks), rx),
            Err(e) => warn!("dictation probe: failed to begin stream: {e}"),
        };
    });

    // Wait for first text or the window, whichever comes first.
    let deadline = Instant::now() + PROBE_WINDOW;
    while Marks::since(&marks.first_commit, t0).is_none() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Closes the capture channel, which ends the forwarder, which ends
    // `service::run`, which finalizes the stream.
    manager
        .stop_streams_only()
        .await
        .map_err(|e| format!("Failed to stop capture: {e}"))?;
    let _ = tokio::time::timeout(FINALIZE_TIMEOUT, forwarder).await;
    if tokio::time::timeout(FINALIZE_TIMEOUT, decoding).await.is_err() {
        warn!("dictation probe: decoder did not finalize within {FINALIZE_TIMEOUT:?}");
    }
    let finalize_ms = t0.elapsed().as_millis();

    let first_chunk_ms = Marks::since(&marks.first_chunk, t0);
    let first_commit_ms = Marks::since(&marks.first_commit, t0);
    let ms = |v: Option<u128>| v.map_or_else(|| "never".to_string(), |v| format!("{v}ms"));

    let report = format!(
        "BENCH dictation_probe: mic '{mic_name}'; entry->streams up {streams_up_ms}ms, \
         ->first audio {}, ->first text {}, ->finalize {finalize_ms}ms \
         (threshold: first audio over ~250ms means dictation needs a prewarmed capture path). \
         Text: {:?}",
        ms(first_chunk_ms),
        ms(first_commit_ms),
        marks.text.lock().unwrap_or_else(|e| e.into_inner()),
    );
    info!("{report}");
    Ok(report)
}
