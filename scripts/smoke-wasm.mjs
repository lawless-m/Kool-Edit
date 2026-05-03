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

console.log("OK");
