/**
 * Default model for transcription.
 * IMPORTANT: Keep in sync with DEFAULT_TRANSCRIBE_MODEL in src-tauri/src/config.rs
 *
 * Picked on WER (1.94% vs nemotron-3.5's 3.06%) at the same download size. It is
 * batch-only, so live text arrives in ~8s segments with no partials — see the
 * Rust constant for the full trade.
 */
export const DEFAULT_TRANSCRIBE_MODEL = 'parakeet-tdt-0.6b-v3-q8';

/** The single local transcription provider. Was localWhisper / whisper / parakeet. */
export const LOCAL_PROVIDER = 'local';
