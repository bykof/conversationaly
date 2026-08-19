# Demo recording

Produces `docs/imgs/live-recording.gif` — launch, start a recording, first words
land — by driving the real Next.js UI in Chromium with a scripted stand-in for
the Tauri backend.

```bash
pnpm dev                                   # in one shell
node scripts/demo/record-gif.mjs           # in another
```

Everything on screen is the shipped frontend: the sidebar, the transport, the
virtualized transcript view, clicked by a real pointer. Only what sits *below*
the UI is faked, which is what makes the shot reproducible on a machine with no
microphone, no model and no Tauri host — CI included.

| file | what it is |
| --- | --- |
| `record-gif.mjs` | the driver: choreographs the take, then encodes the GIF |
| `tauri-mock.js` | stands in for `window.__TAURI_INTERNALS__` — commands and events |
| `cursor.js` | paints a pointer, since video capture omits the OS cursor |
| `transcript.json` | the spoken content |

## Changing what is said

Edit `transcript.json`, or point the driver at a WebVTT file — captions lifted
off a real call go straight in, `<v Name>` and all:

```bash
node scripts/demo/record-gif.mjs --transcript ~/Downloads/call.vtt
```

## Options

`--url`, `--out`, `--transcript`, `--width`, `--height`, `--gif-width`, `--fps`,
`--colors`, `--max-transcript-seconds`, `--ffmpeg`, `--keep-video`,
`--no-chrome`.

Needs Playwright with Chromium, and an ffmpeg that can encode GIF — the one
Playwright bundles cannot, so pass `--ffmpeg` if the detected binary fails.

## When the GIF stops matching the app

The mock is a copy of a contract, so it can drift. If a command in
`tauri-mock.js` or an event name it emits is renamed on the Rust side, the take
either fails outright (an unmodelled command lands in `window.__demo.unhandled`)
or quietly records the wrong thing. Fix the mock, not the GIF.
