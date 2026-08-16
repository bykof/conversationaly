# HANDOVER — Latency & responsiveness work package

**Created:** 2026-08-16 · **Source:** FluidVoice teardown (28-agent research pass, 3 repos read in full)
**Narrative report:** https://claude.ai/code/artifact/b9bc100b-1b95-45b5-9df9-4fad84d85aeb

You are picking up a fully-scoped work package. **The research is done. Do not re-do it.** Every task below was
already adversarially verified against this codebase by an agent instructed to refute it — 11 of 14 candidates were
corrected in that pass, and four had their recommended first step inverted. The corrections are baked into the briefs.
If a brief tells you *not* to do something, that is because someone already tried to justify doing it and was wrong.

---

## 0. Read this first (orchestrator)

**Your job:** spawn subagents to execute the task briefs in §4, respecting the wave boundaries and file ownership.

**Why not all at once:** six of these tasks want to edit `audio/recording_commands.rs` and four want
`audio/pipeline.rs`. Naive parallel fan-out produces merge conflicts on the two hottest files in the repo. The waves
below are drawn on **file ownership**, not on topic. Within a wave, no two tasks touch the same file — so those really
can run simultaneously. Across waves they cannot.

**Parallelism available:** 6 agents in Wave 1, 1 in Wave 2, 3 in Wave 3.

**Isolation:** use `isolation: "worktree"` for every subagent. They edit disjoint files, but worktrees make the merge
explicit and let a failed lane be discarded without touching the others.

**Merge order:** Wave 1 lanes merge in any order (disjoint). Wave 2 must merge after W1-B1 (it adds a line to
`lib.rs`, which B1 owns). Wave 3 must merge after Wave 2.

### Environment gotcha — tell every Rust subagent

```bash
export SDKROOT="$(xcrun --show-sdk-path)"
```

Required before any cargo command touching `llama-cpp-sys-2` (`-p llama-helper` or `--workspace`). Without it bindgen
fails with `'stdio.h' file not found`, which looks like a broken dependency and is not.
`cargo check -p conversationaly` alone does not need it.

### Verification each subagent must run before reporting done

| Lane type | Command |
| --- | --- |
| Rust | `cd frontend/src-tauri && cargo check --all-targets` (plus `cargo test` if the brief adds tests) |
| Frontend | `cd frontend && pnpm run build` and `pnpm exec tsc --noEmit` |
| Docs-only | none |

Line numbers in this document were verified on 2026-08-16 but **grep for the quoted code, do not trust the number** —
sibling lanes may have shifted the file.

---

## 1. Context in three paragraphs

FluidVoice (macOS dictation app, 82k lines Swift) feels instant not because of a faster model but because of a
**shorter critical path**: the ASR model is resident from ~3.5s after launch and never unloaded; the Core Audio
plumbing is prepared while idle without starting the hardware; and the UI acknowledges the *click* and confirms the
*microphone* as two separate events, never conflating them. Everything non-essential — media pause, accessibility
capture, LLM prewarm — is deliberately pushed behind the first real audio packet.

Applying that lens to Conversationaly found more in our tree than in theirs. We unload a 716 MB model on every stop
and reload it on every start, **blocking capture** so the reload window is meeting audio we never hear. We have 6.5
seconds of hardcoded sleeps in the stop path, one of which waits for an event no Rust code emits. Our record button
renders no pending state — the spinner exists and its flags are never set. And we persist zero diagnostics on Windows.

**Not in scope, deliberately:** their CoreML/ANE Parakeet path (Apple-only, not portable), their text-injection
waterfall (destructive — see §5), and dictation as a feature (see W3-T3, which builds a measurement, not a feature).

---

## 2. Hard rules — things a fresh agent will otherwise get wrong

These were each proposed, investigated, and refuted during the research pass. Do not let a subagent re-derive them.

1. **Do not prewarm the cpal input stream.** cpal cannot express FluidVoice's prepare-without-start; there is no
   equivalent to registering an IOProc and withholding `AudioDeviceStart`. The win would be 5–80 ms out of a path
   dominated by the model load. Fix W2-T1 instead.
2. **Do not cache `discover_models()`** behind a directory mtime. It buys ~0.15 ms and breaks in-progress download
   status: on APFS, appending to a file does not bump the parent directory's mtime, so a cached scan would freeze a
   downloading model at `Corrupted`/`Missing` — exactly the state the model-manager UI polls.
3. **Do not invert `unload_engine_after_batch`** in `audio/common.rs`. A live `Session` pins the old `Model` via
   `Arc<ModelInner>`; reloading mid-recording doubles resident weights. Its `is_recording()` guard must stay.
4. **Do not use `common.rs::configured_local_model` for any preload.** It substitutes `DEFAULT_TRANSCRIBE_MODEL`
   when the provider is `builtin-ai`, which would load 716 MB for users who never decode with transcribe.cpp. Read
   the provider the way `transcribe_validate_model_ready` does and bail early on builtin-ai.
5. **Do not port a growing-prefix preview loop.** FluidVoice re-decodes the entire buffer from t=0 every 0.6 s.
   At meeting length that is ~180× real-time compute. Our append-only committed stream is correct. See W1-B2, which
   locks this in with a test rather than a comment.
6. **Do not add a third `StatusOverlay` for the starting state.** `StatusOverlays` and `RecordingControls` render in
   identical fixed containers; it would draw on top of the record button. DESIGN.md sanctions a spinner *inside* a
   button only.
7. **Do not add `enigo`, `rdev`, or a CGEventTap.** See §5.
8. **Use `Instant`, never wall clock, for any timing.** FluidVoice's own `elapsedMilliseconds` uses
   `Date().timeIntervalSince1970` and is NTP/DST-jumpable. Do not copy that.

---

## 3. Wave plan and file ownership

**No file appears in two lanes of the same wave.** This is the contract that makes parallel execution safe.

### Wave 1 — six lanes, fully parallel

| Lane | Task | Owns (exclusive) |
| --- | --- | --- |
| **W1-A1** | Delete the typewriter reveal | `components/VirtualizedTranscriptView.tsx`, `hooks/useTranscriptStreaming.ts`, `app/_components/TranscriptPanel.tsx`, `components/TranscriptView.tsx` |
| **W1-A2** | Stop-path padding | `hooks/useRecordingStop.ts`, `contexts/TranscriptContext.tsx`, `app/meeting-details/page.tsx`, `services/transcriptService.ts` |
| **W1-A3** | Record-button pending state (TS half) | `hooks/useRecordingStart.ts`, `app/page.tsx`, `components/RecordingControls.tsx` |
| **W1-B1** | Log file + log-volume gates | `src-tauri/src/main.rs`, `src-tauri/src/lib.rs`, `src-tauri/Cargo.toml`, `src-tauri/src/audio/pipeline.rs`, `src-tauri/src/console_utils/`, `components/ConsoleToggle.tsx` |
| **W1-B2** | Transcription-hexagon instrumentation | `src-tauri/src/audio/transcription/**` (all files) |
| **W1-C1** | Injection rule + onboarding arity bug | `CLAUDE.md`, `components/onboarding/steps/PermissionsStep.tsx` |

Frontend paths are relative to `frontend/src/` unless prefixed `src-tauri`.

### Wave 2 — one lane (the shared Rust core)

| Lane | Task | Owns |
| --- | --- | --- |
| **W2-T1** | Model residency, capture-before-load, 50 ms sleep, tray/pending Rust half | `src-tauri/src/audio/recording_commands.rs`, `audio/common.rs`, `audio/recording_manager.rs`, `call_detector.rs`, `transcribe_engine/commands.rs`, `tray.rs`, one line in `lib.rs` |

Three separate findings collapse into one lane because they all edit `recording_commands.rs`. Execute as **three
sequential commits** inside the lane, in the order given in the brief.

### Wave 3 — three lanes, parallel

| Lane | Task | Owns |
| --- | --- | --- |
| **W3-T1** | Capture truth: mic level meter + honest arming | `audio/recording_state.rs`, `audio/pipeline.rs`, `audio/recording_commands.rs`, `components/RecordingControls.tsx`, `components/VirtualizedTranscriptView.tsx`, `contexts/RecordingStateContext.tsx` |
| **W3-T2** | Context memoization + kill the 500 ms poll | `contexts/TranscriptContext.tsx`, `components/Sidebar/SidebarProvider.tsx`, `hooks/useRecordingStateSync.ts` |
| **W3-T3** | Dictation probe (measurement only) | `audio/dictation_probe.rs` (new), one line in `lib.rs` |

---

## 4. Task briefs

Each brief is self-contained. A subagent should not need the artifact or any other document.

---

### W1-A1 · Delete the 800 ms typewriter reveal

**Why.** We take text the decoder has already committed and hide it for up to 795 ms, revealing it at 15 ms per two
characters via a 66 Hz `setInterval`. Every tick re-renders `VirtualizedTranscriptView` and can change the revealing
row's height, driving a ResizeObserver → `measureElement` → relayout of all following rows — roughly 47 renders/sec
sustained, ~90,000 avoided renders per hour of speech. It also ignores `prefers-reduced-motion`, the only motion in
the app that does.

**Hidden correctness bug, fix this first.** `streamingSegment` is never nulled on completion, so the newest
*committed* line renders in `text-ink-muted` — the same token as genuinely uncommitted partial text — for the entire
5–11 s gap until the next commit. The user cannot tell what is settled.

**Do.**
1. **Commit 1 (the correctness half, ships alone):** in `VirtualizedTranscriptView.tsx`, delete the
   `isStreaming && 'text-ink-muted'` branch (~line 115), remove the `isStreaming` prop from `TranscriptSegment`
   (~lines 74, 81) and its two call sites (~351, ~390), and delete the now-wrong comment above it.
2. **Commit 2:** delete `hooks/useTranscriptStreaming.ts` entirely. Strip its import, its call, and the
   `getDisplayText`/`streamingSegmentId` usage from `VirtualizedTranscriptView.tsx`. Remove the now-unused
   `enableStreaming` prop and drop `enableStreaming={isRecording}` from `app/_components/TranscriptPanel.tsx`.
3. If `components/TranscriptView.tsx` is dead (verify with a repo-wide grep for imports before deleting) it carries a
   duplicated ~85-line copy of the same effect — remove it too.

**Verify.** `pnpm run build` and `pnpm exec tsc --noEmit`. Manually: record 30 s, confirm committed lines appear
immediately at full opacity and only genuine partial text renders muted.

**Expected diff:** roughly −360 lines.

---

### W1-A2 · Delete the 6.5 s of hardcoded padding in the stop path

**Why.** Three unconditional sleeps sit between "decode drained" and "meeting readable": 4000 ms
(`useRecordingStop.ts:206`), 500 ms (`:229`), 2000 ms (`:336-342`).

**Two facts that justify the deletions — verified, put them in the commit message.**
- The 4000 ms waits for a `transcription-complete` event. Grepping all of `src-tauri/src` returns only
  `retranscription-complete`. **Nothing emits it.** The listener at `:156` and its wrapper in
  `services/transcriptService.ts:100` are dead code.
- The poll loop beside it does no work either: the registered command is a hardcoded stub in `lib.rs` returning
  `{chunks_in_queue: 0, is_processing: false, ...}`, so the loop breaks on iteration one and the 8 s escape at `:175`
  is unreachable. (A second, real-ish implementation exists in `recording_commands.rs` but is shadowed and never
  registered — leave it alone, it belongs to W2-T1's file.)
- The real guarantee is that `stop_recording` awaits the transcription `JoinHandle` and unlistens the transcript
  listener before it returns, and all three call sites await that return.

**Do, in this order — (B) is a prerequisite for deleting the 500 ms.**

- **(A)** Delete lines ~204-206 (the 4000 ms) and replace with a single macrotask yield
  (`await new Promise(r => setTimeout(r, 0))`) placed immediately before the `flushBuffer()` call at ~:217. Delete the
  dead `transcription-complete` listener at ~:156-159 and ~:197-198, and delete
  `transcriptService.listenForTranscriptionComplete`. Do not leave a listener for an event nothing emits.
- **(B)** In `contexts/TranscriptContext.tsx`: `flushBuffer` currently returns `void`, so `await flushBuffer()` is a
  no-op and the 500 ms is load-bearing by accident. Change `processBufferedTranscripts` to compute the merged array
  against `transcriptsRef.current` rather than the `prev` argument, assign `transcriptsRef.current = combined`
  synchronously, and make `flushBuffer` **return the flushed array**. Keep the existing commit-time effect as a
  backstop for the other writers. Then delete the 500 ms at `:229` and change `:239` to consume the returned array.
- **(C)** Replace the `setTimeout(..., 2000)` navigate delay at `:336-342` with a direct `router.push(...)`, keeping
  `clearTranscripts()` and `setStatus(IDLE)` after the push. Then in `app/meeting-details/page.tsx`, split the
  spinner gate (~:354): drop the summary-fetch `isLoading` term so the page renders as soon as `metadata` has
  populated `meetingDetails`, and let the content component render its own summary placeholder from
  `summaryData === null`. A transcript that is fully in hand must not wait on an LLM.

**Do not** attempt to render the in-memory transcript on the destination page — `clearTranscripts()` has already run
and `usePaginatedTranscripts` owns that data. Metadata plus the paginated first page is one source of truth.

**Verify.** Record 30 s, stop, confirm the last spoken sentence is present in the saved meeting and that the
meeting-details page paints its transcript before the summary resolves.

**Smallest shippable slice:** (A) alone — ~10 lines, delivers 4 of the 6.5 s, and the 500 ms still covers the React
commit until (B) lands.

---

### W1-A3 · Wire the record button's pending state

**Why — this is a live bug, not cosmetics.** `RecordingControls.tsx:47` and `:53` declare `isStarting` and
`isValidatingModel`; neither is ever set to `true`. So `busy` (`:319`) is false during the multi-second start, the
guard at `:71` is dead, and the spinner branch at `:372` is unreachable. A second click therefore fires a second
`start_recording_with_devices_and_meeting`, which blocks on the lifecycle lock, fails the `IS_RECORDING` check, and
paints a red **"Recording Failed — check your audio device settings"** card while the recording is running fine.

We already do this correctly in the tray (`tray.rs` sets `RecordingState::Starting` on click and renders a disabled
"🔄 Starting Recording…"). The window is behind the tray. That is the argument to use.

**Do.**
1. **Own the flag in the hook, not the button.** `hooks/useRecordingStart.ts` has *three* start paths: the button
   (`handleRecordingStart`), the `autoStartRecording` sessionStorage effect, and the `start-recording-from-sidebar`
   listener (fired by `SidebarProvider` and by the tray via `layout.tsx`). The latter two already set an internal
   `isAutoStarting` that `app/page.tsx` destructures away and never passes down. Promote it to a general `isStarting`,
   set it as the **first statement** of all three starters — before the `checkParakeetReady()` await, which is itself
   an await — wrap each body in `try/finally { setIsStarting(false) }`, and return it from the hook.
2. Destructure `isStarting` in `app/page.tsx` and pass it to `<RecordingControls>`. In `RecordingControls.tsx`,
   delete the local `isStarting` state and the never-used `isValidatingModel`, use the prop, keep `busy`, and reuse
   the existing branch at `:372-376` relabelled "Starting…". The guard at `:71` becomes live.
3. **Two-phase label.** Add a listener for `model-loading-started` / `model-loading-completed` / `model-loading-failed`
   and swap the label to "Loading model…" between them, falling back to "Starting…". These three events already exist
   and are documented as a stable contract in `transcribe_engine/commands.rs`; they currently have zero listeners.
   **W2-T1 makes `transcribe_validate_model_ready` emit them** — until that lands your listener simply never fires,
   which is a safe no-op. Do not invent a new event name.
4. If the invoke is parked waiting on a `stop_recording` drain, label it distinctly ("Waiting for the previous
   recording to finish…") rather than showing an indefinite spinner.

**Accessibility (PRODUCT.md / DESIGN.md):** the pending button keeps an accessible name (`aria-busy`, visible text),
and reduced motion must not remove information — swap the spinning `Loader2` for a static state word under
`prefers-reduced-motion`, do not just freeze it.

**Do not** add a `StatusOverlay` — see hard rule 6.

**Verify.** Click record and confirm the button is labelled and disabled within one frame on all three entry points
(button, sidebar, tray). Double-click and confirm no "Recording Failed" card appears.

---

### W1-B1 · Ship a log file, and stop the logs drowning it

**Why.** Persisted diagnostics retrievable from a user's machine today: **0 bytes on Windows release** (no console,
stderr discarded); on macOS, only what the user captures by running `log stream` themselves. Model-load timing,
capture health, mix-rate drift and the "transcribing slower than you are speaking" warning are all already emitted at
`info!` and all already thrown away. Every other measurement task in this package depends on this one.

**Separate bug, fix in the same commit.** `main.rs:10` does `std::env::set_var("RUST_LOG", "info")` unconditionally
before `env_logger::init()`, overwriting the export in `clean_run.sh`. `./clean_run.sh debug` — documented in
CLAUDE.md — has never produced debug output.

**Do.**
1. Move `tauri-plugin-log` out of the `cfg(target_os = "macos")` dependency block in `src-tauri/Cargo.toml` into the
   shared `[dependencies]`.
2. Delete the `set_var` and `env_logger::init()` from `main.rs`; register the plugin in `lib.rs::run()` alongside the
   other plugins with `TargetKind::LogDir { file_name: Some("conversationaly") }` + `Stdout`, `max_file_size` 4 MB,
   `RotationStrategy::KeepOne`, level from `RUST_LOG` defaulting to info. 4 MB rather than FluidVoice's 1 MB because a
   meeting is 30–120 minutes, not a 3-second dictation. Drop `env_logger` from `Cargo.toml` and from `console_utils`.
3. **Mandatory in the same commit, or the file is 95% mixer telemetry:** `pipeline.rs:248`'s `if self.windows % 8 == 0`
   emits ~2.5 lines/s ≈ 9,000 lines and ~1.8 MB per meeting-hour. Convert it, and `pipeline.rs:562`'s
   `callbacks % 200`, to 30-second time gates (`last_log.elapsed() >= Duration::from_secs(30)`). Keep the counter
   arithmetic in the callback — it is three atomics — and move only the `info!` behind the gate.
4. Add `get_log_file_path()` in `console_utils`, backed by `app.path().app_log_dir()`, register it in the
   `invoke_handler`, and add a "Reveal log file" button to `components/ConsoleToggle.tsx` beside the existing toggle
   — whose copy already promises log access it does not deliver.

No `tauri.conf.json` capability entry is needed as long as the frontend never calls the plugin's JS API.

**Verify.** Release build on macOS, record 60 s, confirm a log file exists in the app log dir containing the model-load
line and at most ~2 mixer lines per minute. Confirm `./clean_run.sh debug` now produces debug output.

---

### W1-B2 · Instrument the transcription hexagon

**Why.** We have no idea how far behind the speaker we are. The number that matters is
`lag_ms = wall_clock_since_session_start − chunk.audio_end` at each commit, and nothing computes it. Separately, the
streaming path drains an **unbounded** channel with no cap and no warning — unlike the segmented path, which caps
backlog at 30 s and warns the user. A slower-than-realtime streaming model grows that queue for the whole meeting,
silently.

**Do — three commits, all inside `audio/transcription/`.**

1. **`BenchSink`, a decorator, not a macro.** New `adapters/bench_sink.rs` implementing `TranscriptSink` by wrapping
   an inner sink: count commits, compute `lag_ms` per commit, and emit one `info!` line prefixed `BENCH ` on the first
   commit and then at most every 15 s (`n`, `t`, `audio_end`, `lag_ms`, `chars`). Never log `tentative` — it fires per
   feed. Wrap it at the **two composition points in `transcription/mod.rs`** (the main path and the builtin-audio-LLM
   path) and all three decode backends are covered with zero adapter changes. Export from `adapters/mod.rs`. Unit-test
   it against the existing `FakeSink` in `service.rs`.
2. **Stream lag counters.** `Stream::feed` already returns `StreamUpdate.buffered_ms` — computed upstream as
   `input_received_us − audio_committed_us`, i.e. exactly the live lag — and `StreamingTranscriber::feed` currently
   **discards it**. Add `peak_buffered_ms`, `feed_wall_us`, `audio_fed_us`, `commits` to `StreamingTranscriber`, wrap
   the `self.stream.feed(...)` call in an `Instant`, and emit one summary `info!` from `finish()`: peak/median
   buffered_ms, commit count, and measured feed RTF. **No behaviour change.** This produces the number that decides
   whether `att_context_right` can be lowered (see §6).
3. **Lock in the append-only invariant with a test.** Extract `emitted_len` / `emitted_audio_secs` from
   `StreamingTranscriber` into a pure `CommittedCursor::advance(&mut self, committed: &str, audio_end_secs: f64) ->
   Option<TranscriptChunk>`, reduce `emit_committed` to a call into it, and add two tests:
   `only_the_new_tail_is_emitted` and `emitting_is_proportional_to_the_delta_not_the_session`. This is what stops a
   future refactor from reintroducing the growing-prefix loop (hard rule 5). A comment would not.
4. Once `lag_ms` exists, add a one-shot `sink.warn()` when it crosses ~20 s on the streaming path, reusing the
   `backlog_warned` latch pattern already in `adapters/segmented.rs`, so users get the same "pick a faster model"
   advice on both paths.

**Do not** change any decode behaviour, chunk size, or model option in this lane. Measurement only.

**Verify.** `cargo check --all-targets`, `cargo test -p conversationaly`. Then record 2 minutes and confirm `BENCH `
lines appear with plausible `lag_ms`.

---

### W1-C1 · Write down the injection rule; fix the onboarding arity bug

Two unrelated small things, bundled because nothing else touches these files.

**1. The injection rule (docs only, ~20 lines).** Append item 8 to the numbered "Important Constraints and Gotchas"
list in `CLAUDE.md` (currently ends at item 7, "Audio Permissions"), titled **"No text injection into other
applications without a verified caret anchor."** State plainly that no injection code exists today, so this is a
greenfield rule rather than a description of current behaviour. Content:

- Never write a whole field. FluidVoice's Accessibility path has rungs that replace the entire target's `kAXValue`,
  and the element is sometimes chosen by a hierarchy walk rather than by focus — so the destroyed field need not be
  the one the user is typing in.
- Those destructive rungs are reached precisely when `kAXValue`/`kAXSelectedTextRange` are unreadable — Electron apps,
  web views and terminals. That is the majority case for a desktop dictation tool, not the tail.
- One occurrence is unbounded loss of someone else's document, with no undo entry from us.
- A verified caret anchor (read back what was inserted, where) is a **prerequisite that gates the feature**, not a
  follow-up.
- No `enigo`, no `rdev`, no CGEventTap. See §5 of `HANDOVER.md`.

**2. The onboarding arity bug.** `components/onboarding/steps/PermissionsStep.tsx` invokes `open_system_settings`
with no argument in two places, while `src-tauri/src/utils.rs` requires `preference_pane: String`. Both onboarding
permission buttons currently throw and fall through to an `alert()`. Copy the working call shape from
`components/PermissionWarning.tsx`. Verify by walking onboarding and clicking both buttons.

---

### W2-T1 · The Rust capture/engine critical path

**Single lane, three sequential commits.** All three touch `recording_commands.rs`, which is why they cannot be
parallelised. This is the highest-impact task in the package.

#### Commit 1 — stop unloading the model; add an idle unloader

`stop_recording` unconditionally calls `unload_model()` (grep `unload_model` in `recording_commands.rs`, ~line 662,
inside a block that also emits a `"stage": "unloading_model"` progress event ~line 641). So **every** recording
re-reads 716 MB. And `transcribe.cpp` does not mmap — it streams the file through `std::ifstream` and copies each
tensor via `ggml_backend_tensor_set`, so a load is a full read *plus* a full copy, and residency is 716 MB
non-purgeable. Because the load sits ahead of capture start, that entire window is meeting audio we never hear.

Do:
- Delete the unload block. Keep the progress emit but relabel the stage honestly ("finalizing") — it will no longer be
  unloading. Replace the deleted block with a call to a new `super::common::touch_engine_idle()`.
- In `audio/common.rs`, beside the existing `ENGINE_LIFECYCLE_LOCK`: add `ENGINE_LAST_USE` (an `Instant` behind a
  lock), `pub(crate) async fn touch_engine_idle()`, and `pub(crate) fn spawn_engine_idle_unloader()`.
- **Model the unloader directly on `summary/summary_engine/sidecar.rs::start_idle_check_loop`** — same
  `DEFAULT_IDLE_TIMEOUT_SECS` (300), 60 s tick with `MissedTickBehavior::Skip`. Do not invent a second timeout number.
  Each tick: skip if `is_recording_now()`, skip if an import or retranscription is in progress; else if
  `last_use.elapsed() > timeout`, take `acquire_engine_lifecycle_lock()`, **re-check `is_recording_now()` under the
  lock**, then unload. Make the timeout overridable via a `TRANSCRIBE_IDLE_TIMEOUT` env var.
- Because we are not CoreML, this idle timer is **mandatory, not optional** — the delete alone leaks 716 MB for the
  life of the process, so the pair is the smallest honest commit.
- Also call `touch_engine_idle()` from the import and retranscription engine-init paths so a batch job resets the clock.
- Leave `unload_engine_after_batch` exactly as it is (hard rule 3).
- Spawn the unloader in `lib.rs` next to the existing `transcribe_init()` spawn.
- Tests, beside the existing `test_engine_lifecycle_lock_serializes_acquirers` in `common.rs`: does not unload while
  recording; does unload after the timeout when idle; `touch_engine_idle` pushes the deadline out. All three are
  infrastructure-free — no model, no Tauri.

#### Commit 2 — warm on intent, and stop losing the head of the meeting

- In `call_detector.rs::spawn`, after the trigger produces a headline and after the `is_recording()` check but
  **before** the nudge-enabled gate (intent exists even with the nudge disabled), spawn a detached warm task. It must:
  read provider+model the way `transcribe_validate_model_ready` does (**not** via `configured_local_model` — hard rule
  4); return immediately for builtin-ai providers; take the lifecycle lock, re-check `is_recording_now()`,
  short-circuit if the configured model is already loaded, else load it; **log-and-drop every error** so a failed
  preload never surfaces UI; call `touch_engine_idle()` on success.
- This placement means the app never loads 716 MB for a user who opened it to read yesterday's summary, and the load
  overlaps the seconds the nudge is already on screen. Windows/Linux use a process-launch edge that fires earlier than
  the macOS mic-busy edge — strictly better for warming.
- **Belt and braces:** in both `start_recording_with_meeting_name` and `start_recording_with_devices_and_meeting`,
  start `manager.start_recording(...)` **before** awaiting the model load, then spawn the transcription task. The
  transcription channel is an `UnboundedSender`, so chunks buffer and RTF ~0.06 catches up. Keep the pre-flight
  *existence* check — a missing file is the error users actually hit — but move `load_model` off the capture path.

#### Commit 3 — the 50 ms sleep, the stuck tray, and the phase events

- Delete `recording_manager.rs:122` (`tokio::time::sleep(...50ms)`) and its comment. Provably safe: `pipeline.rs`
  creates an unbounded channel and calls `state.set_audio_sender` **synchronously inside `start()`** before it
  returns, so the "Audio pipeline not ready" branch is unreachable once `start()` has returned.
- Add `Instant`-based `info!` stage timings: around `transcribe_validate_model_ready`, around
  `pipeline_manager.start()`, around `stream_manager.start_streams()`, and a total, emitted as one line of the form
  `record_start validate_ms=… pipeline_ms=… mic_stream_ms=… sys_stream_ms=… total_ms=…`. Ship at `info!` so it lands
  in W1-B1's log file on both platforms. **Every remaining latency decision in this package branches on these numbers.**
- Fix the stuck tray: the early error returns in `start_recording_with_devices_and_meeting` skip
  `crate::tray::update_tray_menu(&app)`, stranding the tray in a disabled "🔄 Starting Recording…" until the app
  restarts. Call it before every early return.
- Emit the existing `model-loading-started` / `-completed` / `-failed` events from `transcribe_validate_model_ready`
  around its `engine.load_model` call (it already returns early for builtin-ai and for an already-loaded model, so the
  events fire only when a load actually happens). The `AppHandle` is already available; no signature change. This is
  what makes W1-A3's "Loading model…" label real.

**Do not** in this lane: touch `audio/transcription/`, change any decode option, or add a discovery cache
(hard rule 2).

**Verify.** `cargo check --all-targets` and `cargo test -p conversationaly`. Then: start a recording, stop, start
again — confirm from the log that the second start has no model load and that `total_ms` drops accordingly. Leave the
app idle >5 min and confirm the unload fires. Start a recording while a stop is draining and confirm the tray recovers.

---

### W3-T1 · Capture truth: level meter and honest arming

Two findings merged — they share `recording_state.rs`, `pipeline.rs` and `recording_commands.rs`.

**Why.** PRODUCT.md defines the user's context as *peripheral* during a 30–120 minute call. Over that span they glance
15–30 times to answer one question: *is it still capturing?* Today that is answered only by an elapsed timer that
keeps ticking whether or not the headset disconnected, the OS switched inputs, or another app grabbed exclusive
WASAPI. The startup gap is not the payoff; **converting a silent-failure class into an at-a-glance one is.**

**Critical detail — tap the raw signal.** Read RMS from the **mic branch of `process_audio_data`, right after the mono
downmix and before the resampler and RNNoise**. A normalized or post-processed tap keeps showing signal after the mic
has died, which is the exact failure this exists to catch.

**Do.**
- `audio/recording_state.rs`: add `mic_rms_milli: AtomicU32` with a `set_mic_level`/`mic_level` pair, and
  `mic_frames: AtomicU64` with `note_frames`/`frames_captured`.
- `audio/pipeline.rs`: write the RMS from the mic branch after the downmix; bump `mic_frames` next to the existing
  `raw_frames.fetch_add`.
- `audio/recording_commands.rs`: spawn a 100 ms emitter for a new `mic-level` event once recording starts; expose
  `mic_frames` in `get_recording_state` via a delegating `RecordingManager::frames_captured`.
- Frontend: listen for `mic-level` in `RecordingControls.tsx` and render the existing `AudioLevelMeter` component
  (it exists; its only call site is behind a commented-out "Test Mic" button in `DeviceSelection`). Map `mic_frames`
  to a `captureArmed` flag in `RecordingStateContext`, and make the recording empty state in
  `VirtualizedTranscriptView.tsx` show a static "Waiting for audio" until `captureArmed` flips, then "Listening".

**Honest scope — say this in the PR.** The arming gate catches exactly one failure mode: mic opened but delivering
nothing. It does *not* catch a muted-but-live mic (that is what the level meter is for) and it does not cover
system-audio-only capture. In the healthy case the gate is 50–300 ms on a wired mic and 1–3 s on Bluetooth; the
justification is the broken case, not the healthy one.

**Acceptance test that decides whether the rest is worth building:** start a recording and speak — the bar tracks the
voice. Then mute the mic in System Settings — **the bar must drop to zero while the elapsed timer keeps advancing.**
That second half is the entire product claim.

---

### W3-T2 · Memoize the context values; delete the 500 ms poll

**Why.** Three polling loops (500 ms recording state, 1 s `is_recording`, 5 s summary status) each `setState` a fresh
object, and both `TranscriptContext` and `SidebarProvider` rebuild their value objects on every render — so the entire
consumer tree re-renders at 2–3 Hz with nothing changed. The 500 ms poll also takes the `RECORDING_MANAGER` mutex from
an async command twice a second. That is 7,200 spurious context invalidations per hour → 0.

**Do.** `useMemo` the context value objects in `TranscriptContext.tsx` and `Sidebar/SidebarProvider.tsx` with correct
dependency arrays. Remove the 500 ms recording-state poll in favour of the events that already exist
(`recording-started` / `recording-stopped`), keeping the 1 s `is_recording` sync as the reconciliation backstop.

**Do not** implement partial-event throttling. It was in the original proposal and is worthless: on the default model
(`nemotron-3.5-asr-streaming-0.6b-q8`) `tentative` is provably always empty, so there are **zero** partial events to
throttle. Only `moonshine-streaming-*` emits any, at ≤4/sec.

**Sequencing:** must land after W1-A2, which restructures `TranscriptContext.tsx`.

---

### W3-T3 · Dictation probe — a measurement, not a feature

**Do not build dictation.** Build the thing that decides whether dictation is affordable, then stop and report.

**The open question.** Decode is the cheap half — at RTF ~0.06 a five-second utterance finalizes in ~300 ms on an
already-loaded model. Capture start is the unknown, and from code it looks like 300–800 ms: a full
`host.input_devices()` walk with a `default_input_config()` per device, the (now deleted) 50 ms sleep, cpal stream
construction, a 50 ms mixing window and a 512-sample resampler fill. **Nobody has the number.**

**Do.** New `frontend/src-tauri/src/audio/dictation_probe.rs`: one `#[tauri::command] pub async fn dictation_probe`,
registered in the `generate_handler!` list, gated behind `#[cfg(debug_assertions)]` so it never ships. It must **not**
touch `IS_RECORDING` or the global `RECORDING_MANAGER` — construct its own `RecordingManager::new()` and drop it at the
end. Body: mic only, no system audio, no saver; take the returned 16 kHz `UnboundedReceiver<AudioChunk>`; pipe it
through the existing `StreamingTranscriber` and `service::run` into a `Vec`-collecting `ProbeSink` implementing
`TranscriptSink`. Log five `Instant` deltas: command entry → streams up → first audio chunk → first committed text →
finalize. Roughly 80 lines, no new dependency, no new model, no hotkey, no UI. It proves the hexagon's port claim for
free.

**Write the exit criterion into the PR description before running it:** if first-chunk exceeds ~250 ms on a machine
with Bluetooth audio paired, dictation requires a prewarmed capture path — FluidVoice spent ~1,750 lines on theirs —
and the bet is XXL, not XL. Stating that in advance is what stops the result being rationalized afterwards.

**Then stop.** Report the numbers. Everything past this is a product decision (§6).

---

## 5. Do not build these

| Thing | Why not |
| --- | --- |
| Text-injection waterfall | Two of FluidVoice's four Accessibility rungs replace the entire target field, and the element can be chosen by hierarchy walk rather than focus. Reached exactly on Electron/web views/terminals. Unbounded loss of someone else's document, no undo. W1-C1 writes the rule down. |
| Session-wide `CGEventTap` | The only macOS API that binds a bare modifier and fires on key-down — and a component that wakes on every keystroke in the session. For a product whose pitch is that nothing leaves the machine, that is the wrong trade. If a chord hotkey proves insufficient UX, that is an argument for killing dictation, not for installing the tap. |
| `enigo` / `rdev` | Not needed. If injection is ever approved, `cidre` is already a dependency and only needs its `cg`/`ax` features plus two `extern "C"` declarations. |
| A local HTTP API | Theirs listens on `127.0.0.1:47733` exposing history, dictionary, transcribe and post-process with **no authentication at all**. If we ever add a local surface it needs a token from day one. |
| Growing-prefix preview loop | Hard rule 5. |
| cpal stream prewarm | Hard rule 1. |

---

## 6. Decisions that need the human, not an agent

1. **`att_context_right` 13 → 6.** Our default model emits committed text in 1120 ms blocks. R=6 halves that to
   560 ms — the single largest real-latency lever we have, worth ~560 ms off the tail. Measured cost on the English
   sibling: WER 1.68% → 1.70% (inside the CI), decode 231 → 436 ms per 7 s utterance. Still ~9× real-time headroom on
   Apple silicon; **unquantified and potentially a hard regression on CPU-only Windows**, where the streaming path has
   no backlog cap. Plan: ship the extension plumbing with the constant still at 13 (a no-op that exercises the code
   path, with an `InvalidArgument` fallback to `StreamOptions::default()`), collect feed RTF from W1-B2 across
   Metal/CUDA/CPU, then flip only if CPU-only feed RTF stays under ~0.5 — and make R conditional on `Model::backend()`
   rather than exposing a user setting. Before merging at R=6, run one German and one English meeting and diff the
   saved transcript for stray `<ll-RR>` locale tags; our porting notes observed them mid-utterance at lower R on this
   exact checkpoint.
2. **Concurrent stream start.** `start_streams` awaits the microphone stream to completion before beginning the system
   stream, and the system path pays `create_process_tap` + `create_aggregate_device` (routinely 100 ms+). A
   `tokio::try_join!` would likely recover more than several tasks in this package combined. Not scheduled because it
   needs the W2-T1 commit-3 numbers first to size it.
3. **Whether dictation happens at all.** See W3-T3. Dictation is a different job at a different moment from
   PRODUCT.md's peripheral 30–120 minute call. The ingredients are closer than they look — we are already a
   background-resident tray app, and `call_detector.rs` already builds a transparent, non-focus-stealing, always-on-top
   overlay on an isolated route that bypasses the provider tree, proven on all three platforms. What is entirely
   absent: any global-hotkey capability, any clipboard writer, any injection surface. **If it proceeds:** clipboard-only,
   a chord hotkey with toggle semantics, reuse the already-loaded model, and refuse dictation while a recording is in
   progress with a message saying so — that sidesteps the one-in-flight-compute constraint honestly rather than paying
   a second 716 MB load. And fix `call_detector.rs` in the same change: while dictation holds the mic, `mic_in_use()`
   is already true, so a real call joined during that window produces **no idle→busy edge and the nudge is silently
   missed**. The suppression flag has to be consulted *inside* `Trigger::poll`, not after it.

---

## 7. Suggested orchestrator prompt

> Read `HANDOVER.md` at the repo root in full, then execute Wave 1 by spawning six subagents in a single message —
> one per lane W1-A1, W1-A2, W1-A3, W1-B1, W1-B2, W1-C1 — each with `isolation: "worktree"`. Give each subagent its
> own brief from §4 verbatim, plus §0 (environment and verification) and §2 (hard rules). Do not let any subagent edit
> a file outside its lane's ownership row in §3. When all six report done and verified, merge them, run the full
> verification suite, then run Wave 2 as a single agent, then Wave 3 as three parallel agents. Report the §6 decisions
> to the human rather than deciding them.

---

## 8. Housekeeping

- Branch per lane off `main`; `main` is the release branch, do not commit directly to it.
- This file is scaffolding. Delete it once Wave 3 has landed — the durable outputs are the code, the tests in
  `common.rs` and `adapters/`, and the CLAUDE.md rule from W1-C1.
- Line numbers were verified 2026-08-16 against commit `fae4d8f`. **Grep for the quoted code, not the number.**
