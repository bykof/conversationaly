// audio/transcription/service.rs
//
// The live-transcription use case, and the only place that knows the shape of a
// recording: audio arrives until it stops, then the decoder is drained, then
// the volatile text on screen is cleared.
//
// It compiles without Tauri, without transcribe.cpp and without an audio
// device, which is the point — every bug this module used to hide was in code
// that could only be exercised by holding a real meeting.

use super::ports::{Transcriber, TranscriptSink};
use crate::audio::AudioChunk;
use log::info;
use tokio::sync::mpsc::UnboundedReceiver;

/// Drive audio through a transcriber into a sink until the audio stops.
///
/// Blocking on purpose: every decoder behind [`Transcriber`] is a blocking
/// native call, so this runs on a blocking thread rather than holding an async
/// worker hostage.
pub fn run(
    mut transcriber: impl Transcriber,
    mut sink: impl TranscriptSink,
    mut audio: UnboundedReceiver<AudioChunk>,
) {
    let mut chunks = 0u64;

    while let Some(chunk) = audio.blocking_recv() {
        chunks += 1;
        // A failed chunk costs that audio, not the meeting's transcript. The
        // decoder is usually fine on the next one, and ending a recording's
        // transcript over one bad buffer is the worse failure by far.
        if let Err(e) = transcriber.feed(&chunk.data, &mut sink) {
            sink.warn(&e.to_string());
        }
    }

    // Input ended: whatever the decoder is still holding is still owed to us.
    if let Err(e) = transcriber.finish(&mut sink) {
        sink.warn(&e.to_string());
    }

    // Live text does not survive the stream that produced it. Clear it, or the
    // last half-formed phrase sits on screen looking like part of the meeting.
    sink.tentative("");

    info!("🎙️ Live transcription ended after {chunks} audio chunks");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::recording_state::DeviceType;
    use crate::audio::transcription::ports::TranscriptChunk;
    use anyhow::Result;

    /// What a sink was told. Shared, because `run` takes the sink by value.
    #[derive(Default)]
    struct Recorded {
        committed: Vec<String>,
        tentative: Vec<String>,
        warnings: Vec<String>,
    }

    #[derive(Default, Clone)]
    struct FakeSink(std::sync::Arc<std::sync::Mutex<Recorded>>);

    impl TranscriptSink for FakeSink {
        fn committed(&mut self, chunk: TranscriptChunk) {
            self.0.lock().unwrap().committed.push(chunk.text);
        }
        fn tentative(&mut self, text: &str) {
            self.0.lock().unwrap().tentative.push(text.to_string());
        }
        fn warn(&mut self, message: &str) {
            self.0.lock().unwrap().warnings.push(message.to_string());
        }
    }

    /// Emits one chunk per feed, optionally failing on a chosen call.
    struct FakeTranscriber {
        fed: usize,
        fail_on: Option<usize>,
        finished: bool,
    }

    impl FakeTranscriber {
        fn new(fail_on: Option<usize>) -> Self {
            Self { fed: 0, fail_on, finished: false }
        }
    }

    impl Transcriber for FakeTranscriber {
        fn feed(&mut self, _pcm: &[f32], sink: &mut dyn TranscriptSink) -> Result<()> {
            self.fed += 1;
            if self.fail_on == Some(self.fed) {
                anyhow::bail!("decoder exploded");
            }
            sink.committed(TranscriptChunk {
                text: format!("chunk {}", self.fed),
                audio_start: 0.0,
                audio_end: 1.0,
                confidence: None,
            });
            Ok(())
        }

        fn finish(&mut self, sink: &mut dyn TranscriptSink) -> Result<()> {
            self.finished = true;
            sink.committed(TranscriptChunk {
                text: "tail".to_string(),
                audio_start: 1.0,
                audio_end: 2.0,
                confidence: None,
            });
            Ok(())
        }
    }

    fn audio() -> (
        tokio::sync::mpsc::UnboundedSender<AudioChunk>,
        UnboundedReceiver<AudioChunk>,
    ) {
        tokio::sync::mpsc::unbounded_channel()
    }

    fn chunk() -> AudioChunk {
        AudioChunk {
            data: vec![0.0; 800],
            sample_rate: 16_000,
            timestamp: 0.0,
            chunk_id: 0,
            device_type: DeviceType::Microphone,
        }
    }

    /// Feed `n` chunks, then close the channel, and report what the sink saw.
    fn transcribe(n: usize, fail_on: Option<usize>) -> Recorded {
        let (tx, rx) = audio();
        for _ in 0..n {
            tx.send(chunk()).unwrap();
        }
        drop(tx);

        let sink = FakeSink::default();
        let recorded = sink.0.clone();
        run(FakeTranscriber::new(fail_on), sink, rx);

        let log = std::mem::take(&mut *recorded.lock().unwrap());
        log
    }

    #[test]
    fn every_chunk_is_transcribed_and_the_tail_is_drained() {
        assert_eq!(
            transcribe(3, None).committed,
            vec!["chunk 1", "chunk 2", "chunk 3", "tail"],
            "the decoder's tail must be drained after input ends"
        );
    }

    #[test]
    fn a_failed_chunk_warns_but_does_not_end_the_transcript() {
        let log = transcribe(3, Some(2));

        assert_eq!(log.warnings, vec!["decoder exploded"]);
        assert_eq!(
            log.committed,
            vec!["chunk 1", "chunk 3", "tail"],
            "chunk 2 failed; 3 and the tail must still arrive"
        );
    }

    #[test]
    fn the_live_text_is_cleared_when_the_stream_ends() {
        assert_eq!(
            transcribe(1, None).tentative.last().map(String::as_str),
            Some(""),
            "a stale partial must not outlive the recording"
        );
    }
}
