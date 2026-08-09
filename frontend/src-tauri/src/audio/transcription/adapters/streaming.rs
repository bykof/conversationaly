// audio/transcription/adapters/streaming.rs
//
// Driving adapter for models that stream, which is the preferred path: one
// transcribe.cpp Stream stays open for the whole meeting and audio is fed to it
// continuously, so the model keeps its context across pauses.
//
// The split that makes this work is transcribe.cpp's own:
//   - `committed` text is append-only and never rewritten -> a TranscriptChunk,
//     which is exactly what the transcript table assumes.
//   - `tentative` text is the volatile suffix -> shown greyed, never saved.
// Because committed text can only grow, nothing downstream needs revision or
// reconciliation logic.

use crate::audio::transcription::ports::{Transcriber, TranscriptChunk, TranscriptSink};
use anyhow::Result;
use log::warn;

pub struct StreamingTranscriber<'a> {
    stream: transcribe_cpp::Stream<'a>,
    /// Byte offset into `committed` already sent on. Committed text is
    /// append-only, so this only moves forward and the tail past it is exactly
    /// what is new.
    emitted_len: usize,
    /// Audio position (seconds) that `emitted_len` corresponds to.
    emitted_audio_secs: f64,
}

impl<'a> StreamingTranscriber<'a> {
    pub fn new(stream: transcribe_cpp::Stream<'a>) -> Self {
        Self { stream, emitted_len: 0, emitted_audio_secs: 0.0 }
    }

    /// Send on whatever committed text is new since the last call.
    fn emit_committed(&mut self, audio_committed_ms: i64, sink: &mut dyn TranscriptSink) {
        let committed = self.stream.text().committed;
        if committed.len() <= self.emitted_len {
            return;
        }

        // Committed text is documented append-only, so emitted_len is a valid
        // boundary into it. Recover rather than panic if that ever stops
        // holding — a slice panic here takes down a live recording.
        let Some(new_text) = committed.get(self.emitted_len..) else {
            warn!(
                "Committed text is not an extension of what was already emitted \
                 (len {} vs offset {}); re-syncing to the current end",
                committed.len(),
                self.emitted_len
            );
            self.emitted_len = committed.len();
            return;
        };

        let text = new_text.trim().to_string();
        self.emitted_len = committed.len();

        let audio_end = audio_committed_ms as f64 / 1000.0;
        let audio_start = std::mem::replace(&mut self.emitted_audio_secs, audio_end);

        if text.is_empty() {
            return;
        }

        sink.committed(TranscriptChunk {
            text,
            audio_start,
            audio_end,
            // No confidence on this path, deliberately. The only way to get one
            // is `Stream::snapshot()`, which materialises the ENTIRE session —
            // every segment, word and token as an owned String — to average it
            // and throw it away. Doing that per commit, on this thread, over a
            // transcript that grows all meeting, is why the live transcript
            // used to fall further behind the longer a meeting ran. What it
            // bought was a running mean over the whole session, so every line
            // carried the same number anyway.
            confidence: None,
        });
    }
}

impl Transcriber for StreamingTranscriber<'_> {
    fn feed(&mut self, pcm_16k: &[f32], sink: &mut dyn TranscriptSink) -> Result<()> {
        let update = self.stream.feed(pcm_16k)?;

        if update.committed_changed {
            self.emit_committed(update.audio_committed_ms, sink);
        }
        if update.tentative_changed {
            sink.tentative(&self.stream.text().tentative);
        }
        Ok(())
    }

    fn finish(&mut self, sink: &mut dyn TranscriptSink) -> Result<()> {
        let update = self.stream.finalize()?;
        self.emit_committed(update.audio_committed_ms, sink);
        Ok(())
    }
}
