// The outside of the live-transcription hexagon.
//
// `streaming` and `segmented` are driving adapters: they own a decoding
// backend and satisfy `ports::Transcriber`. `tauri_sink` is the driven adapter:
// it satisfies `ports::TranscriptSink` by emitting Tauri events.
//
// Nothing in `service` or `ports` imports from here — the dependency only ever
// points inward, which is what makes the use case testable with fakes.

pub mod segmented;
pub mod streaming;
pub mod tauri_sink;
