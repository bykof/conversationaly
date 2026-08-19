/**
 * A stand-in for the Tauri IPC bridge, injected into a plain Chromium page
 * before any app module runs.
 *
 * Why this exists: the demo GIF has to show the real UI — the real sidebar, the
 * real transport, the real virtualized transcript view — but a headless Linux
 * container has no audio device, no GGUF model and no Tauri host. Everything the
 * frontend knows about the backend arrives through `window.__TAURI_INTERNALS__`
 * (see node_modules/@tauri-apps/api/core.js), so replacing that one object is
 * enough to run the whole frontend against a scripted backend.
 *
 * Nothing in src/ is aware of this file. If a command or event name here drifts
 * from the Rust side the GIF stops matching the app, not the other way round —
 * so keep the payload shapes in step with transcribe/audio commands.
 */
(() => {
  const callbacks = new Map();
  let nextCallbackId = 1;

  /** event name -> [{ rid, handlerId }] */
  const listeners = new Map();
  let nextRid = 1;

  const state = {
    recording: false,
    paused: false,
    startedAt: null,
    micFrames: 0,
    meetingName: null,
  };

  /** Commands invoked but not modelled here — read back over CDP while iterating. */
  const unhandled = [];

  const MIC = 'MacBook Pro Microphone';
  const SYSTEM = 'BlackHole 2ch';
  const TRANSCRIBE_MODEL = 'nemotron-3.5-asr-streaming-0.6b-q8';

  /**
   * Stored preferences. The demo depicts someone with a meeting history, so the
   * inform-your-participants consent toast has already been acknowledged — left
   * unset it fires on every take and parks itself over the transport.
   * Keys absent from here read back as "never set".
   */
  const STORE = {
    show_recording_notification: false,
  };

  const MEETINGS = [
    { id: 'm-1', title: 'Weekly product sync' },
    { id: 'm-2', title: 'Design review — transcript pane' },
    { id: 'm-3', title: 'Customer interview: Ravi (Northwind)' },
    { id: 'm-4', title: 'Q3 roadmap planning' },
  ];

  const elapsed = () =>
    state.startedAt === null ? null : (Date.now() - state.startedAt) / 1000;

  /** How long the scripted cold start spends on each leg. */
  const MODEL_LOAD_MS = 900;
  const CAPTURE_ARMED_MS = 1150;

  /** Deliver a backend event to every live listener, in Tauri's payload shape. */
  function emitEvent(event, payload) {
    for (const { rid, handlerId } of listeners.get(event) ?? []) {
      const entry = callbacks.get(handlerId);
      if (!entry) continue;
      if (entry.once) callbacks.delete(handlerId);
      entry.callback({ event, id: rid, payload });
    }
  }

  /**
   * The 10Hz `mic-level` stream the transport's meter is driven by. A slow
   * envelope times with the transcript being fed in, so the needle moves with
   * the words rather than jittering independently of them.
   */
  let micTicker = null;
  let micTick = 0;
  function startMicTicker() {
    stopMicTicker();
    micTick = 0;
    micTicker = setInterval(() => {
      micTick += 1;
      const envelope = 0.5 + 0.5 * Math.sin(micTick / 11);
      const syllable = 0.55 + 0.45 * Math.sin(micTick / 1.7);
      const rms = state.paused ? 0 : 0.02 + 0.3 * envelope * syllable;
      emitEvent('mic-level', { rms, armed: true });
    }, 100);
  }
  function stopMicTicker() {
    if (micTicker) clearInterval(micTicker);
    micTicker = null;
  }

  const handlers = {
    // --- app / onboarding ---------------------------------------------------
    get_onboarding_status: () => ({
      version: '1',
      completed: true,
      current_step: 5,
      model_status: {
        parakeet: 'downloaded',
        summary: 'downloaded',
        selected_summary_model: 'gemma4:e2b',
      },
      last_updated: '2026-05-12T09:00:00Z',
    }),
    check_first_launch: () => false,
    get_app_version: () => '1.3.0',

    // --- devices & permissions ---------------------------------------------
    get_audio_devices: () => [
      { name: MIC, device_type: 'Input' },
      { name: SYSTEM, device_type: 'Output' },
    ],
    start_audio_level_monitoring: () => null,
    stop_audio_level_monitoring: () => null,
    trigger_microphone_permission: () => null,
    trigger_system_audio_permission_command: () => null,

    // --- recording lifecycle ------------------------------------------------
    is_recording: () => state.recording,
    get_recording_state: () => ({
      is_recording: state.recording,
      is_paused: state.paused,
      is_active: state.recording && !state.paused,
      recording_duration: elapsed(),
      active_duration: elapsed(),
      mic_frames: state.micFrames,
    }),
    get_recording_meeting_name: () => state.meetingName,
    get_recording_preferences: () => ({
      preferred_mic_device: MIC,
      preferred_system_device: SYSTEM,
    }),
    get_meeting_folder_path: () =>
      '/Users/you/Documents/Conversationaly/Weekly product sync',
    get_default_recordings_folder_path: () =>
      '/Users/you/Documents/Conversationaly',
    // Mirrors the real cold-start shape so the GIF shows what a user sees:
    // the transport goes to "Loading model…", then capture arms, then
    // `recording-started` flips the whole tree into its recording state.
    start_recording_with_devices_and_meeting: ({ meeting_name }) => {
      state.recording = true;
      state.paused = false;
      state.startedAt = Date.now();
      state.meetingName = meeting_name ?? null;
      emitEvent('model-loading-started', { model: TRANSCRIBE_MODEL });
      return new Promise((resolve) => {
        setTimeout(() => {
          emitEvent('model-loading-completed', { model: TRANSCRIBE_MODEL });
          resolve(null);
        }, MODEL_LOAD_MS);
        setTimeout(() => {
          state.micFrames = 1;
          emitEvent('recording-started', {});
          startMicTicker();
        }, CAPTURE_ARMED_MS);
      });
    },
    pause_recording: () => {
      state.paused = true;
      return null;
    },
    resume_recording: () => {
      state.paused = false;
      return null;
    },
    stop_recording: () => {
      const meetingName = state.meetingName;
      state.recording = false;
      state.paused = false;
      state.startedAt = null;
      state.micFrames = 0;
      stopMicTicker();
      emitEvent('mic-level', { rms: 0, armed: false });
      emitEvent('recording-stopped', {
        message: 'Recording saved',
        folder_path: '/Users/you/Documents/Conversationaly',
        meeting_name: meetingName,
      });
      emitEvent('recording-stop-complete', {});
      return null;
    },

    // --- transcription engine ----------------------------------------------
    transcribe_init: () => null,
    transcribe_has_available_models: () => true,
    transcribe_is_model_loaded: () => true,
    transcribe_get_current_model: () => TRANSCRIBE_MODEL,
    transcribe_validate_model_ready: () => true,
    transcribe_load_model: () => null,
    transcribe_model_languages: () => ['en', 'de', 'fr', 'es'],
    transcribe_get_models_directory: () =>
      '/Users/you/Library/Application Support/Conversationaly/models',
    transcribe_get_available_models: () => [
      {
        id: TRANSCRIBE_MODEL,
        name: TRANSCRIBE_MODEL,
        status: 'Downloaded',
        streaming: true,
        size_bytes: 651000000,
      },
    ],
    transcribe_builtin_models: () => [],
    get_transcript_history: () => [],
    get_transcription_status: () => ({ is_transcribing: state.recording }),

    // --- config -------------------------------------------------------------
    api_get_transcript_config: () => ({
      provider: 'local',
      model: TRANSCRIBE_MODEL,
      apiKey: null,
    }),
    api_get_model_config: () => null,
    api_get_api_key: () => null,
    api_get_custom_openai_config: () => null,
    api_get_meetings: () => MEETINGS,
    api_get_summary: () => null,
    get_ollama_models: () => [],
    builtin_ai_list_models: () => [],
    builtin_ai_is_model_ready: () => true,
    builtin_ai_get_recommended_model: () => 'gemma4:e2b',
    builtin_ai_get_model_info: () => null,
    set_language_preference: () => null,
    show_console: () => null,
  };

  function dispatch(cmd, args) {
    if (cmd === 'plugin:event|listen') {
      const rid = nextRid++;
      const forEvent = listeners.get(args.event) ?? [];
      forEvent.push({ rid, handlerId: args.handler });
      listeners.set(args.event, forEvent);
      return rid;
    }
    if (cmd === 'plugin:event|unlisten') {
      const forEvent = listeners.get(args.event) ?? [];
      listeners.set(
        args.event,
        forEvent.filter((l) => l.rid !== args.eventId)
      );
      return null;
    }
    // plugin-store's `get` destructures a [value, exists] tuple, so a bare null
    // throws inside the plugin rather than in app code.
    if (cmd === 'plugin:store|get') {
      const value = STORE[args.key];
      return value === undefined ? [null, false] : [value, true];
    }
    if (cmd === 'plugin:store|load' || cmd === 'plugin:store|create') return 1;

    // Frontend -> backend emits and every plugin the demo does not exercise
    // (updater, process) are inert rather than rejected: a rejection surfaces as
    // an error toast on top of the shot.
    if (cmd.startsWith('plugin:')) return null;

    const handler = handlers[cmd];
    if (handler) return handler(args ?? {});

    unhandled.push(cmd);
    return null;
  }

  window.__TAURI_INTERNALS__ = {
    invoke: (cmd, args) => Promise.resolve().then(() => dispatch(cmd, args)),
    transformCallback: (callback, once) => {
      const id = nextCallbackId++;
      callbacks.set(id, { callback, once });
      return id;
    },
    unregisterCallback: (id) => callbacks.delete(id),
    metadata: { currentWindow: { label: 'main' }, currentWebview: { label: 'main' } },
    convertFileSrc: (path) => path,
  };

  // event.js's _unlisten() calls this directly, not over invoke.
  window.__TAURI_EVENT_PLUGIN_INTERNALS__ = {
    unregisterListener(event, eventId) {
      const forEvent = listeners.get(event) ?? [];
      listeners.set(event, forEvent.filter((l) => l.rid !== eventId));
    },
  };

  window.__TAURI_OS_PLUGIN_INTERNALS__ = {
    platform: 'macos',
    family: 'unix',
    os_type: 'macos',
    version: '15.3.0',
    arch: 'aarch64',
    eol: '\n',
    exe_extension: '',
  };

  /** The driver's handle on the scripted backend. */
  window.__demo = {
    emit: emitEvent,
    /** Mark the mic as delivering frames, which is what un-gates the pane. */
    armCapture() {
      state.micFrames = 1;
    },
    state,
    unhandled,
  };
})();
