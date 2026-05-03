// End-to-end smoke test for the wasm-bindgen surface.
//
// Builds a synthetic 8-cycle sine WAV in memory, hands it to WasmEngine via
// the same API the browser worker uses, and checks that the peak summary
// captures the signal's [-1, 1] range. Run with:
//
//   make smoke-wasm
//
// Requires `wasm-pack` and the `wasm32-unknown-unknown` target.

import { createRequire } from "node:module";
import { execSync } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const require = createRequire(import.meta.url);

const outDir = mkdtempSync(join(tmpdir(), "kool-smoke-"));
execSync(
  `wasm-pack build engine --target nodejs --out-dir ${outDir} --features wasm`,
  { stdio: "inherit" },
);

const { banner, format_version, WasmEngine } = require(
  join(outDir, "kool_edit_engine.js"),
);

console.log("banner:", banner());
console.log("format_version:", format_version());

function makeWav(samples) {
  const sampleRate = 48000;
  const channels = 1;
  const bitsPerSample = 32;
  const bytesPerSample = bitsPerSample / 8;
  const dataBytes = samples.length * bytesPerSample;
  const buffer = new ArrayBuffer(44 + dataBytes);
  const view = new DataView(buffer);
  const write = (off, s) => {
    for (let i = 0; i < s.length; i++) view.setUint8(off + i, s.charCodeAt(i));
  };
  write(0, "RIFF");
  view.setUint32(4, 36 + dataBytes, true);
  write(8, "WAVE");
  write(12, "fmt ");
  view.setUint32(16, 16, true);
  view.setUint16(20, 3, true);
  view.setUint16(22, channels, true);
  view.setUint32(24, sampleRate, true);
  view.setUint32(28, sampleRate * channels * bytesPerSample, true);
  view.setUint16(32, channels * bytesPerSample, true);
  view.setUint16(34, bitsPerSample, true);
  write(36, "data");
  view.setUint32(40, dataBytes, true);
  for (let i = 0; i < samples.length; i++) {
    view.setFloat32(44 + i * 4, samples[i], true);
  }
  return new Uint8Array(buffer);
}

const samples = new Float32Array(2048);
for (let i = 0; i < samples.length; i++) {
  samples[i] = Math.sin((i / 2048) * Math.PI * 2 * 8);
}
const wav = makeWav(samples);

const eng = new WasmEngine(96000);
const id = eng.importWav("sine.wav", wav, new Date().toISOString());
console.log("source id:", id);
console.log("frame count:", eng.sourceFrameCount(id));

const peaks = eng.peakSummary(id, 16);
const min = Math.min(...peaks);
const max = Math.max(...peaks);
console.log(`global min/max: ${min.toFixed(3)} / ${max.toFixed(3)}`);

if (Math.abs(max - 1) > 0.05 || Math.abs(min + 1) > 0.05) {
  console.error("FAIL: peaks did not span [-1, 1]");
  process.exit(1);
}

// Apply a destructive silence on the middle half, then query and check.
const silenceOp = JSON.stringify({
  Silence: { range: { start: 512, end: 1536 } },
});
eng.applyOp(id, silenceOp, new Date().toISOString());
const middle = eng.querySamples(id, 800n, 1200n);
const middleMax = Math.max(...middle.map(Math.abs));
if (middleMax !== 0) {
  console.error(`FAIL: silenced range still has signal (max abs ${middleMax})`);
  process.exit(1);
}
console.log("apply_op + query_samples: silenced range is zero");

// Undo restores the original middle.
eng.undo(id);
const middleAfterUndo = eng.querySamples(id, 800n, 1200n);
const undoMax = Math.max(...middleAfterUndo.map(Math.abs));
if (undoMax < 0.5) {
  console.error(`FAIL: undo did not restore signal (max abs ${undoMax})`);
  process.exit(1);
}
console.log("undo restores pre-op samples");

// Flatten then verify edit list is empty in the saved project JSON.
eng.applyOp(id, silenceOp, new Date().toISOString());
eng.flatten(id, new Date().toISOString());
const json = JSON.parse(eng.projectJson());
const editList = json.sources[id].edits.ops ?? [];
if (editList.length !== 0) {
  console.error(`FAIL: flatten left ${editList.length} ops in the journal`);
  process.exit(1);
}
console.log("flatten clears the edit journal");

// Reload the same JSON into a fresh engine and verify the source comes back.
const eng2 = new WasmEngine(96000);
eng2.loadProjectJson(eng.projectJson());
const reload = JSON.parse(eng2.projectJson());
if (!reload.sources[id]) {
  console.error("FAIL: reloaded project missing source");
  process.exit(1);
}
console.log("project json round-trip preserves sources");

// Length-changing op: Cut. Re-import a fresh source so we don't fight the
// previous flatten.
const cutSrcWav = makeWav(new Float32Array(100).fill(0.5));
const cutSrcId = eng.importWav("cut.wav", cutSrcWav, new Date().toISOString());
const beforeFrames = Number(eng.sourceFrameCount(cutSrcId));
eng.applyOp(
  cutSrcId,
  JSON.stringify({ Cut: { range: { start: 20, end: 80 } } }),
  new Date().toISOString(),
);
const cutAll = eng.querySamples(cutSrcId, 0n, BigInt(beforeFrames - 60));
if (cutAll.length !== beforeFrames - 60) {
  console.error(`FAIL: expected ${beforeFrames - 60} frames after cut, got ${cutAll.length}`);
  process.exit(1);
}
console.log(`cut shortens buffer (${beforeFrames} -> ${cutAll.length})`);

// Generate: insert a half-second of sine.
const genSrcWav = makeWav(new Float32Array(10).fill(0.0));
const genSrcId = eng.importWav("gen.wav", genSrcWav, new Date().toISOString());
eng.applyOp(
  genSrcId,
  JSON.stringify({
    Generate: {
      at: 5,
      length: 100,
      params: { Tone: { shape: "Sine", frequency_hz: 440.0, amplitude_db: 0.0 } },
    },
  }),
  new Date().toISOString(),
);
const generated = eng.querySamples(genSrcId, 0n, 110n);
if (generated.length !== 110) {
  console.error(`FAIL: generate did not extend buffer (length=${generated.length})`);
  process.exit(1);
}
const generatedPeak = Math.max(...Array.from(generated.slice(5, 105)).map(Math.abs));
if (Math.abs(generatedPeak - 1.0) > 0.05) {
  console.error(`FAIL: generated tone peak ${generatedPeak}, expected ~1.0`);
  process.exit(1);
}
console.log(`generate inserts samples (length 10 -> ${generated.length}, peak ${generatedPeak.toFixed(3)})`);

// Normalize-peak to -6 dB.
const normSrcWav = makeWav(new Float32Array(50).fill(0.1));
const normSrcId = eng.importWav("norm.wav", normSrcWav, new Date().toISOString());
eng.applyOp(
  normSrcId,
  JSON.stringify({
    Normalize: { range: { start: 0, end: 50 }, target: "Peak", value_db: -6.0206 },
  }),
  new Date().toISOString(),
);
const normalized = eng.querySamples(normSrcId, 0n, 50n);
const normPeak = Math.max(...Array.from(normalized).map(Math.abs));
if (Math.abs(normPeak - 0.5) > 1e-2) {
  console.error(`FAIL: normalize peak got ${normPeak}, expected ~0.5`);
  process.exit(1);
}
console.log(`normalize-peak scales to target (peak ${normPeak.toFixed(3)})`);

console.log("OK");
