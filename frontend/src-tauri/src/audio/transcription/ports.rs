// audio/transcription/ports.rs
//
// The two boundaries of the live-transcription hexagon.
//
// Inside: `service::run`, which knows only these traits and the chunk type
// below. Outside: transcribe.cpp, Silero, the llama-helper sidecar, Tauri
// events — none of which appear in the use case, and all of which are
// substitutable in a test.
//
// Both ports are deliberately tiny. A port is a promise the domain makes about
// what it needs, so every method here is something the use case actually calls;
// there is no "interface for its own sake" surface to keep in sync.

use anyhow::Result;

/// A piece of transcript the decoder considers final.
///
/// Final is the important word. Everything that reaches a sink through
/// [`TranscriptSink::committed`] is persisted, so a decoder must not send text
/// it might revise. Text that is still moving goes to
/// [`TranscriptSink::tentative`] instead, and is never saved.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptChunk {
    pub text: String,
    /// Seconds from the start of the recording.
    pub audio_start: f64,
    pub audio_end: f64,
    /// Mean per-token probability, when the decoder reports one.
    ///
    /// `None` is not "zero confidence" — it means this decoder has nothing to
    /// report, which is the honest answer for a chat completion, since it
    /// carries no token probabilities. The UI omits the badge rather than
    /// painting a made-up number on real text.
    pub confidence: Option<f32>,
    pub speaker: Option<String>,
}

impl TranscriptChunk {
    pub fn duration(&self) -> f64 {
        (self.audio_end - self.audio_start).max(0.0)
    }
}

/// Where transcript text goes, and how the user hears about trouble.
///
/// The driven side of the hexagon: the app implements this with Tauri events,
/// tests implement it with a Vec.
pub trait TranscriptSink: Send {
    /// Final text. Will be rendered *and* persisted.
    fn committed(&mut self, chunk: TranscriptChunk);

    /// Volatile text to show and never save. An empty string clears it, which
    /// is how the end of a stream is signalled.
    fn tentative(&mut self, text: &str);

    /// Something went wrong but transcription continues.
    fn warn(&mut self, message: &str);
}

/// Anything that can turn a live 16kHz mono stream into transcript text.
///
/// One trait covers all three backends because the differences between them —
/// streaming versus segmented, local weights versus a sidecar process — are
/// entirely about *how* text is produced, and the use case only cares *that* it
/// is. Adapters emit through the sink rather than returning text so that a
/// backend which produces several chunks, or a warning and no chunks, needs no
/// special shape.
pub trait Transcriber: Send {
    /// Take the next slice of audio. Must not block longer than it takes to
    /// decode: the caller is the only thing draining the audio channel.
    fn feed(&mut self, pcm_16k: &[f32], sink: &mut dyn TranscriptSink) -> Result<()>;

    /// Input has ended. Emit whatever is still held back.
    fn finish(&mut self, sink: &mut dyn TranscriptSink) -> Result<()>;

    fn note_levels(&mut self, _start_s: f64, _mic_rms: f32, _sys_rms: f32) {}
}
