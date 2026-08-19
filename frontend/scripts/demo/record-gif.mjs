#!/usr/bin/env node
/**
 * Records the "launch → start recording → first words land" demo GIF against the
 * real Next.js UI, with the Tauri backend replaced by scripts/demo/tauri-mock.js.
 *
 * The point is that nothing about the interface is a mock-up: it is the shipped
 * frontend, clicked by a real pointer, rendering real components. Only the
 * backend beneath it is scripted, which is what makes the shot reproducible on a
 * machine with no microphone, no model and no Tauri host.
 *
 * Prerequisites
 *   1. `pnpm dev` running (http://localhost:3118 by default)
 *   2. Playwright with Chromium available (`npx playwright install chromium`,
 *      or a global `npm i -g playwright`)
 *   3. An ffmpeg with the GIF encoder. Playwright ships a stripped ffmpeg that
 *      cannot write GIFs — pass --ffmpeg <path> if the detected one fails.
 *
 * Usage
 *   node scripts/demo/record-gif.mjs --out ../docs/imgs/recording.gif
 */

import { execFile, execFileSync } from 'node:child_process';
import { mkdtemp, readFile, rm, mkdir, readdir, stat } from 'node:fs/promises';
import { existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { promisify } from 'node:util';

const execFileAsync = promisify(execFile);
const HERE = path.dirname(fileURLToPath(import.meta.url));

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

function parseArgs(argv) {
  const opts = {
    url: 'http://localhost:3118/',
    out: path.resolve(HERE, '../../../docs/imgs/recording.gif'),
    transcript: path.join(HERE, 'transcript.json'),
    width: 1280,
    height: 800,
    /** Output width. Equal to --width means no rescale, which keeps text crisp. */
    gifWidth: 1280,
    fps: 10,
    colors: 160,
    /** Stop feeding segments once the transcript has run this long. */
    maxTranscriptSeconds: 19,
    ffmpeg: null,
    keepVideo: false,
    /** Frame the shot in a window, to match the stills already in docs/imgs. */
    chrome: true,
  };
  for (let i = 0; i < argv.length; i++) {
    const [flag, inline] = argv[i].split('=');
    const value = () => (inline !== undefined ? inline : argv[++i]);
    switch (flag) {
      case '--url': opts.url = value(); break;
      case '--out': opts.out = path.resolve(value()); break;
      case '--transcript': opts.transcript = path.resolve(value()); break;
      case '--width': opts.width = Number(value()); break;
      case '--height': opts.height = Number(value()); break;
      case '--gif-width': opts.gifWidth = Number(value()); break;
      case '--fps': opts.fps = Number(value()); break;
      case '--colors': opts.colors = Number(value()); break;
      case '--max-transcript-seconds': opts.maxTranscriptSeconds = Number(value()); break;
      case '--ffmpeg': opts.ffmpeg = value(); break;
      case '--keep-video': opts.keepVideo = true; break;
      case '--no-chrome': opts.chrome = false; break;
      default:
        throw new Error(`Unknown option: ${flag}`);
    }
  }
  return opts;
}

// ---------------------------------------------------------------------------
// Dependency discovery
// ---------------------------------------------------------------------------

/**
 * Playwright is a tool dependency of this script, not of the app, so it is not
 * in package.json. Resolve a local install first, then a global one.
 */
async function loadPlaywright() {
  // playwright is CommonJS, so a global install imported by path arrives under
  // `default` rather than as named exports.
  const unwrap = (mod) => mod.chromium ?? mod.default?.chromium;

  try {
    const chromium = unwrap(await import('playwright'));
    if (chromium) return { chromium };
  } catch { /* not installed locally */ }

  try {
    const root = execFileSync('npm', ['root', '-g'], { encoding: 'utf8' }).trim();
    const entry = path.join(root, 'playwright', 'index.js');
    if (existsSync(entry)) {
      const chromium = unwrap(await import(pathToFileURL(entry).href));
      if (chromium) return { chromium };
    }
  } catch { /* fall through to the error below */ }

  throw new Error(
    'playwright not found. Install it with `npm i -g playwright` or `pnpm add -D playwright`.'
  );
}

/** An ffmpeg that can actually encode GIF — Playwright's bundled one cannot. */
function findFfmpeg(override) {
  if (override) return override;
  const candidates = [];
  try {
    candidates.push(execFileSync('command', ['-v', 'ffmpeg'], { encoding: 'utf8', shell: true }).trim());
  } catch { /* not on PATH */ }
  try {
    candidates.push(
      execFileSync('python3', ['-c', 'import imageio_ffmpeg;print(imageio_ffmpeg.get_ffmpeg_exe())'], {
        encoding: 'utf8',
      }).trim()
    );
  } catch { /* imageio-ffmpeg not installed */ }
  const found = candidates.find((c) => c && existsSync(c));
  if (!found) {
    throw new Error(
      'No ffmpeg with a GIF encoder found. Install ffmpeg, or `pip install imageio-ffmpeg`, or pass --ffmpeg <path>.'
    );
  }
  return found;
}

// ---------------------------------------------------------------------------
// Transcript input
// ---------------------------------------------------------------------------

/**
 * Reads either the JSON shape in transcript.json or a WebVTT file, so captions
 * pulled off a real recording can be dropped straight in.
 */
async function loadTranscript(file) {
  const raw = await readFile(file, 'utf8');
  if (file.endsWith('.vtt')) return { wordsPerSecond: 4.2, segments: parseVtt(raw) };
  const data = JSON.parse(raw);
  return { wordsPerSecond: data.wordsPerSecond ?? 4.2, segments: data.segments };
}

function parseVtt(raw) {
  const segments = [];
  const speakers = new Map();
  for (const block of raw.replace(/\r/g, '').split('\n\n')) {
    const lines = block.split('\n').filter((l) => l && !/^WEBVTT|^NOTE|^\d+$/.test(l));
    const cue = lines.findIndex((l) => l.includes('-->'));
    if (cue === -1) continue;
    let text = lines.slice(cue + 1).join(' ').trim();
    if (!text) continue;
    // `<v Name>text</v>` is the VTT way of naming who is talking.
    let speaker;
    const voice = text.match(/^<v\s+([^>]+)>/);
    if (voice) {
      const name = voice[1].trim();
      if (!speakers.has(name)) speakers.set(name, name);
      speaker = name;
      text = text.replace(/^<v\s+[^>]+>/, '');
    }
    text = text.replace(/<[^>]+>/g, '').trim();
    if (text) segments.push({ speaker, text });
  }
  return segments;
}

// ---------------------------------------------------------------------------
// Window chrome
// ---------------------------------------------------------------------------

/** Mat, title bar and hairline, picked to sit next to the light palette in globals.css. */
const CHROME = {
  margin: 24,
  titleBar: 36,
  radius: 10,
  mat: '#EBE9E3',
  bar: '#F5F4EF',
  line: '#E0DED7',
};

function chromeGeometry(opts) {
  const videoW = opts.gifWidth || opts.width;
  const videoH = Math.round((opts.height * videoW) / opts.width);
  const { margin, titleBar } = CHROME;
  return {
    videoW,
    videoH,
    x: margin,
    y: margin + titleBar,
    outWidth: videoW + 2 * margin,
    outHeight: videoH + titleBar + 2 * margin,
  };
}

/**
 * Renders the frame as a transparent PNG that ffmpeg lays over the padded video.
 *
 * It is one SVG path with fill-rule="evenodd": the outer rect minus a rounded
 * inner rect, so the app's pixels show through a genuinely rounded window while
 * the mat stays opaque right into the corners. Chromium does the rasterizing
 * because ffmpeg builds generally cannot read SVG.
 */
async function renderChrome(chromium, geo, dir) {
  const { margin, titleBar, radius, mat, bar, line } = CHROME;
  const { videoW, videoH, outWidth, outHeight } = geo;
  const winH = titleBar + videoH;
  const dots = [
    { cx: margin + 18, fill: '#FF5F57' },
    { cx: margin + 38, fill: '#FEBC2E' },
    { cx: margin + 58, fill: '#28C840' },
  ];

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${outWidth}" height="${outHeight}">
    <path fill="${mat}" fill-rule="evenodd" d="
      M0 0 H${outWidth} V${outHeight} H0 Z
      M${margin + radius} ${margin}
      h${videoW - 2 * radius} a${radius} ${radius} 0 0 1 ${radius} ${radius}
      v${winH - 2 * radius} a${radius} ${radius} 0 0 1 -${radius} ${radius}
      h-${videoW - 2 * radius} a${radius} ${radius} 0 0 1 -${radius} -${radius}
      v-${winH - 2 * radius} a${radius} ${radius} 0 0 1 ${radius} -${radius} z" />
    <path fill="${bar}" d="
      M${margin + radius} ${margin}
      h${videoW - 2 * radius} a${radius} ${radius} 0 0 1 ${radius} ${radius}
      v${titleBar - radius} h-${videoW} v-${titleBar - radius}
      a${radius} ${radius} 0 0 1 ${radius} -${radius} z" />
    <rect x="${margin}" y="${margin + titleBar - 1}" width="${videoW}" height="1" fill="${line}" />
    ${dots.map((d) => `<circle cx="${d.cx}" cy="${margin + titleBar / 2}" r="6" fill="${d.fill}" />`).join('\n    ')}
    <rect x="${margin + 0.5}" y="${margin + 0.5}" width="${videoW - 1}" height="${winH - 1}"
          rx="${radius}" fill="none" stroke="${line}" />
  </svg>`;

  const browser = await chromium.launch();
  const page = await browser.newPage({
    viewport: { width: outWidth, height: outHeight },
    deviceScaleFactor: 1,
  });
  await page.setContent(
    `<style>html,body{margin:0;background:transparent}</style>${svg}`
  );
  const file = path.join(dir, 'chrome.png');
  await page.screenshot({ path: file, omitBackground: true });
  await browser.close();
  return file;
}

// ---------------------------------------------------------------------------
// The take
// ---------------------------------------------------------------------------

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function record(opts) {
  const { chromium } = await loadPlaywright();
  const { wordsPerSecond, segments } = await loadTranscript(opts.transcript);
  const msPerWord = Math.round(1000 / wordsPerSecond);

  const videoDir = await mkdtemp(path.join(tmpdir(), 'conversationaly-demo-'));
  const browser = await chromium.launch({
    args: ['--hide-scrollbars', '--force-color-profile=srgb'],
  });
  const context = await browser.newContext({
    viewport: { width: opts.width, height: opts.height },
    deviceScaleFactor: 1,
    colorScheme: 'light',
    locale: 'en-US',
    timezoneId: 'Europe/Berlin',
    recordVideo: { dir: videoDir, size: { width: opts.width, height: opts.height } },
  });

  // Playwright starts the recording when the page opens, so everything before
  // the app is ready has to be trimmed off the front later.
  const recordingStartedAt = Date.now();
  const page = await context.newPage();
  await page.addInitScript({ path: path.join(HERE, 'tauri-mock.js') });
  await page.addInitScript({ path: path.join(HERE, 'cursor.js') });

  const pageErrors = [];
  page.on('pageerror', (e) => pageErrors.push(String(e)));
  page.on('console', (m) => {
    if (m.type() === 'error') pageErrors.push(m.text());
  });

  await page.goto(opts.url, { waitUntil: 'domcontentloaded' });

  // `next dev` paints its own indicator badge over the bottom-left corner of the
  // app, right on top of Settings. Hidden here rather than switched off in
  // next.config.js, so the demo tooling stays out of the app's config.
  await page.addStyleTag({ content: 'nextjs-portal { display: none !important; }' });

  const startButton = page.getByRole('button', { name: /start recording/i });
  await startButton.waitFor({ timeout: 60_000 });
  // Let the fonts settle so the first frame is not mid-swap.
  await page.evaluate(() => document.fonts.ready);
  await sleep(400);

  // Park the pointer somewhere neutral before the first frame, so it is already
  // on screen rather than appearing out of nowhere at the first move.
  await page.mouse.move(opts.width * 0.52, opts.height * 0.66);

  const firstFrameAt = Date.now();

  // 1. The app as it opens: meeting list, idle transport.
  await sleep(1400);

  // 2. Start the recording the way a user does — a visible travel to the
  //    button, then a real press, so the GIF shows the click that causes it.
  const box = await startButton.boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2, { steps: 28 });
  await sleep(320);
  await page.mouse.down();
  await sleep(110);
  await page.mouse.up();

  // 3. Wait for the scripted cold start to arm capture.
  await page.getByRole('button', { name: /stop recording/i }).waitFor({ timeout: 20_000 });
  await sleep(900);

  // 4. Feed the spoken content: a streaming decoder's growing tail, then a
  //    commit. Timing runs inside the page so it is not paced by CDP latency.
  let audioTime = 1.4;
  let sequenceId = 1;
  for (const segment of segments) {
    const words = segment.text.split(/\s+/).length;
    const duration = words / wordsPerSecond;
    if (audioTime + duration > opts.maxTranscriptSeconds) break;

    await page.evaluate(
      async ({ segment, sequenceId, audioTime, duration, msPerWord }) => {
        const words = segment.text.split(/\s+/);
        let acc = '';
        let revision = sequenceId * 1000;
        for (const word of words) {
          acc = acc ? `${acc} ${word}` : word;
          window.__demo.emit('transcript-partial', { text: acc, revision: revision++ });
          await new Promise((r) => setTimeout(r, msPerWord));
        }
        const clock = new Date(Date.UTC(2026, 4, 12, 9, 3, 0) + audioTime * 1000);
        window.__demo.emit('transcript-update', {
          text: segment.text,
          timestamp: clock.toISOString().slice(11, 19),
          source: 'mixed',
          sequence_id: sequenceId,
          chunk_start_time: audioTime,
          is_partial: false,
          confidence: segment.confidence,
          audio_start_time: audioTime,
          audio_end_time: audioTime + duration,
          duration,
          speaker: segment.speaker,
        });
        window.__demo.emit('transcript-partial', { text: '', revision: revision + 1 });
      },
      { segment, sequenceId, audioTime, duration, msPerWord }
    );

    audioTime += duration + 0.35;
    sequenceId += 1;
    await sleep(320);
  }

  // 5. Hold on the finished state so the loop does not snap.
  await sleep(1300);

  const lastFrameAt = Date.now();
  await context.close();
  await browser.close();

  const [videoFile] = (await readdir(videoDir)).filter((f) => f.endsWith('.webm'));
  if (!videoFile) throw new Error('Playwright produced no video file');

  return {
    chromium,
    video: path.join(videoDir, videoFile),
    videoDir,
    pageErrors,
    trimStart: (firstFrameAt - recordingStartedAt) / 1000,
    duration: (lastFrameAt - firstFrameAt) / 1000,
  };
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

async function toGif(ffmpeg, take, opts) {
  await mkdir(path.dirname(opts.out), { recursive: true });

  const rescale =
    opts.gifWidth && opts.gifWidth !== opts.width
      ? `,scale=${opts.gifWidth}:-1:flags=lanczos`
      : '';

  const inputs = ['-i', take.video];
  let stage = `[0:v]fps=${opts.fps}${rescale}`;

  if (opts.chrome) {
    const geo = chromeGeometry(opts);
    const chromePng = await renderChrome(take.chromium, geo, take.videoDir);
    inputs.push('-i', chromePng);
    // The mat is painted twice — once by pad, once by the overlay — so a
    // rounding disagreement between them cannot show as a seam.
    stage +=
      `,pad=${geo.outWidth}:${geo.outHeight}:${geo.x}:${geo.y}:color=${CHROME.mat}[framed];` +
      `[framed][1:v]overlay=0:0`;
  }

  // stats_mode=full, not diff: on a screen this static, diff spends the palette
  // on the handful of pixels that move and leaves everything else — the window
  // controls, the record button, the brand green — visibly off-hue.
  const filter =
    `${stage},split[a][b];` +
    `[a]palettegen=max_colors=${opts.colors}:stats_mode=full[p];` +
    `[b][p]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle`;

  await execFileAsync(ffmpeg, [
    '-y',
    '-ss', take.trimStart.toFixed(3),
    '-t', take.duration.toFixed(3),
    ...inputs,
    '-filter_complex', filter,
    '-loop', '0',
    opts.out,
  ], { maxBuffer: 32 * 1024 * 1024 });

  return opts.out;
}

// ---------------------------------------------------------------------------

const opts = parseArgs(process.argv.slice(2));
const ffmpeg = findFfmpeg(opts.ffmpeg);
console.log(`ffmpeg: ${ffmpeg}`);

const take = await record(opts);
console.log(`take: ${take.duration.toFixed(1)}s (trimmed ${take.trimStart.toFixed(1)}s of startup)`);
if (take.pageErrors.length) {
  console.warn(`page reported ${take.pageErrors.length} error(s):`);
  for (const e of [...new Set(take.pageErrors)].slice(0, 10)) console.warn(`  ${e}`);
}

const gif = await toGif(ffmpeg, take, opts);
const { size } = await stat(gif);
console.log(`wrote ${gif} (${(size / 1e6).toFixed(1)} MB)`);

if (opts.keepVideo) {
  console.log(`video kept at ${take.video}`);
} else {
  await rm(take.videoDir, { recursive: true, force: true });
}
