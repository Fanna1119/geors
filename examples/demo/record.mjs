// Record the demo page to video, then convert it to MP4 and GIF.
//
// Usage (from examples/demo, geors serving on the API URL):
//   npm install                  # playwright only; uses your installed Chrome
//   node record.mjs [API_URL]    # default http://127.0.0.1:2322
//
// Writes demo.webm (raw), demo.mp4 (whole demo) and demo.gif (one part, as
// GIFs get large) next to this script; the last two need ffmpeg.
//
// Env: WIDTH / HEIGHT (default 1600 x 900), MP4_WIDTH (default 1280),
// MP4_CRF (default 28; lower = sharper and larger), GIF_FROM (scene to start
// the GIF at: autocomplete, tour, reverse-na, reverse-eu, reverse-as,
// search-burst; default reverse-na), GIF_SECONDS (default 10), GIF_WIDTH
// (default 720), GIF_FPS (default 10), HEADLESS=1 to hide the browser
// window (the map then renders without the GPU and loads tiles slower).
import { chromium } from 'playwright';
import { execFileSync } from 'node:child_process';
import { existsSync, readdirSync, renameSync, rmSync, mkdirSync, statSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const api = process.argv[2] || 'http://127.0.0.1:2322';
const width = Number(process.env.WIDTH || 1600);
const height = Number(process.env.HEIGHT || 900);
const mp4Width = Number(process.env.MP4_WIDTH || 1280);
const mp4Crf = String(process.env.MP4_CRF || 28);
const gifFrom = process.env.GIF_FROM || 'reverse-na';
const gifSeconds = Number(process.env.GIF_SECONDS || 10);
const gifWidth = Number(process.env.GIF_WIDTH || 720);
const gifFps = Number(process.env.GIF_FPS || 10);

if (!existsSync(join(here, 'demo-data.js'))) {
  console.error('demo-data.js missing: run ./make-data.py ../../data first');
  process.exit(1);
}
const status = await fetch(api + '/status').catch(() => null);
if (!status?.ok) {
  console.error(`geors is not reachable at ${api}; start it first (geors serve)`);
  process.exit(1);
}

const videoDir = join(here, '.video');
rmSync(videoDir, { recursive: true, force: true });
mkdirSync(videoDir);

const browser = await chromium.launch({
  channel: 'chrome',
  headless: process.env.HEADLESS === '1',
  args: [`--window-size=${width},${height + 100}`],
});
const context = await browser.newContext({
  viewport: { width, height },
  deviceScaleFactor: 1,
  recordVideo: { dir: videoDir, size: { width, height } },
});
const page = await context.newPage();
const videoStart = Date.now(); // the video starts with the page
page.on('console', m => m.type() === 'error' && console.error('page:', m.text()));
const url = pathToFileURL(join(here, 'geors-demo.html')).href + `?api=${encodeURIComponent(api)}`;
console.log('recording', url);
const t0 = Date.now();
await page.goto(url);
await page.waitForFunction(() => window.DEMO_DONE === true, null, { timeout: 5 * 60_000, polling: 500 });
// Scene marks are seconds since page load; shift them to video time.
const { marks, origin } = await page.evaluate(() => ({ marks: window.DEMO_MARKS, origin: performance.timeOrigin }));
const offset = (origin - videoStart) / 1000;
const at = name => (marks[name] ?? 0) + offset;
await context.close(); // finishes the video file
await browser.close();
console.log(`recorded ${((Date.now() - t0) / 1000).toFixed(0)} s`);

const webm = join(here, 'demo.webm');
renameSync(join(videoDir, readdirSync(videoDir)[0]), webm);
rmSync(videoDir, { recursive: true, force: true });
console.log('wrote', webm);

const ffmpeg = (args) => execFileSync('ffmpeg', ['-y', '-loglevel', 'error', ...args], { stdio: 'inherit' });
try {
  // From the intro (after the warm-up) to the end of the outro.
  const cut = ['-ss', at('intro').toFixed(2), '-to', at('end').toFixed(2), '-i', webm];
  ffmpeg([...cut, '-vf', `scale=${mp4Width}:-2:flags=lanczos`, '-c:v', 'libx264', '-preset', 'slow',
    '-pix_fmt', 'yuv420p', '-crf', mp4Crf, '-movflags', '+faststart', join(here, 'demo.mp4')]);
  // Two-pass GIF of one part, with a palette made from that part.
  const part = ['-ss', at(gifFrom).toFixed(2), '-t', String(gifSeconds), '-i', webm];
  const filters = `fps=${gifFps},scale=${gifWidth}:-1:flags=lanczos`;
  const palette = join(here, '.palette.png');
  ffmpeg([...part, '-vf', `${filters},palettegen=stats_mode=diff`, palette]);
  ffmpeg([...part, '-i', palette, '-lavfi', `${filters}[x];[x][1:v]paletteuse=dither=sierra2_4a`, join(here, 'demo.gif')]);
  rmSync(palette);
  const mb = f => (statSync(join(here, f)).size / 1e6).toFixed(1) + ' MB';
  console.log(`wrote demo.mp4 (${mb('demo.mp4')}, ${(at('end') - at('intro')).toFixed(0)} s) and demo.gif (${mb('demo.gif')}, from ${gifFrom})`);
} catch (e) {
  console.error('ffmpeg failed or missing; demo.webm is still there:', e.message);
}
