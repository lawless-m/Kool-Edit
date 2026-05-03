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

// DSL emit: build a fresh project, import a small WAV, apply a couple of
// ops, then verify the DSL surface contains the expected lines.
const dslEng = new WasmEngine(96000);
const dslWav = makeWav(new Float32Array(96000).fill(0.5));
const dslId = dslEng.importWav("dsl.wav", dslWav, new Date().toISOString());
dslEng.applyOp(
  dslId,
  JSON.stringify({ Silence: { range: { start: 9600, end: 19200 } } }),
  new Date().toISOString(),
);
dslEng.applyOp(
  dslId,
  JSON.stringify({
    Generate: {
      at: 0,
      length: 4800,
      params: { Tone: { shape: "Sine", frequency_hz: 440.0, amplitude_db: -6.0 } },
    },
  }),
  new Date().toISOString(),
);
const dsl = dslEng.projectDsl();
const expected = [
  "project \"\"",
  "format_version: 1",
  "sample_rate: 96_000",
  `${dslId} "dsl.wav"`,
  "silence",
  "kind:tone shape:sine freq:440 amplitude:-6dB",
];
for (const needle of expected) {
  if (!dsl.includes(needle)) {
    console.error(`FAIL: dsl missing ${JSON.stringify(needle)}`);
    console.error(dsl);
    process.exit(1);
  }
}
console.log("DSL emitter produces expected lines");

// Mixdown: build a project with a single mono clip on one track, route it
// through a Hall reverb insert, and verify the rendered WAV has tail energy
// past the clip end.
const mixEng = new WasmEngine(48000);
const blip = new Float32Array(480);
blip[0] = 1.0; // 10 ms blip with an impulse at the start
const mixWav = makeWav(blip);
const mixSrcId = mixEng.importWav("mix.wav", mixWav, new Date().toISOString());

const proj = JSON.parse(mixEng.projectJson());
proj.tracks.push({
  id: 1,
  name: "T",
  height: 80.0,
  mute: false,
  solo: false,
  arm: false,
  gain_db: 0.0,
  pan: 0.0,
  inserts: [
    {
      id: 1,
      bypass: false,
      params: {
        Reverb: {
          model: "Hall",
          size: 0.7,
          damping: 0.3,
          mix: 1.0,
        },
      },
    },
  ],
  automation: [],
  clips: [
    {
      id: 1,
      source_id: mixSrcId,
      name: "c",
      track_position: { start: 0, end: 24000 }, // half a second
      source_in: 0,
      source_out: 480,
      gain_db: 0.0,
      pan: 0.0,
      fade_in: { duration_samples: 0, shape: "Linear" },
      fade_out: { duration_samples: 0, shape: "Linear" },
      time_stretch: 1.0,
      pitch_shift_cents: 0.0,
      envelopes: [],
      locked: false,
      group: null,
    },
  ],
});
mixEng.loadProjectJson(JSON.stringify(proj));

const wavBytes = mixEng.mixdownWav();
const riff = String.fromCharCode(...wavBytes.slice(0, 4));
const wave = String.fromCharCode(...wavBytes.slice(8, 12));
if (riff !== "RIFF" || wave !== "WAVE") {
  console.error(`FAIL: mixdown WAV header bad: ${riff}/${wave}`);
  process.exit(1);
}
// The "data" chunk's size field follows the four ASCII bytes "data". WAV
// spec lets fmt and fact chunks sit between the header and data, so we
// search rather than assume an offset.
let dataOffset = -1;
for (let i = 12; i < wavBytes.length - 8; i++) {
  if (
    wavBytes[i] === 0x64 && wavBytes[i + 1] === 0x61 &&
    wavBytes[i + 2] === 0x74 && wavBytes[i + 3] === 0x61
  ) {
    dataOffset = i;
    break;
  }
}
if (dataOffset < 0) {
  console.error("FAIL: no `data` chunk in mixdown WAV");
  process.exit(1);
}
const dataChunkSize = new DataView(wavBytes.buffer, wavBytes.byteOffset + dataOffset + 4, 4)
  .getUint32(0, true);
// 24000 stereo frames × 2 channels × 4 bytes = 192_000 bytes of audio.
const expectedDataBytes = 24000 * 2 * 4;
if (dataChunkSize !== expectedDataBytes) {
  console.error(`FAIL: data chunk ${dataChunkSize} bytes, expected ${expectedDataBytes}`);
  process.exit(1);
}
// Decode the WAV's tail ourselves and assert it isn't silent — that's the
// reverb insert's tail. Skip headers, read the data section as 32-bit LE
// floats, and look at the last 25% of the buffer.
const dataView = new DataView(
  wavBytes.buffer,
  wavBytes.byteOffset + dataOffset + 8,
  dataChunkSize,
);
const totalSamples = dataChunkSize / 4;
const tailStart = (totalSamples / 4) * 3;
let tailSumSq = 0;
for (let i = tailStart | 0; i < totalSamples; i++) {
  const v = dataView.getFloat32(i * 4, true);
  tailSumSq += v * v;
}
const tailRms = Math.sqrt(tailSumSq / (totalSamples - (tailStart | 0)));
if (tailRms < 1e-4) {
  console.error(`FAIL: mixdown tail too quiet (rms ${tailRms}); reverb insert not running?`);
  process.exit(1);
}
console.log(
  `mixdown with reverb insert: ${wavBytes.length}-byte WAV, ${dataChunkSize} bytes audio, tail rms ${tailRms.toFixed(5)}`,
);

// Compressor end-to-end. Steady -10 dB sine, threshold -20 dB, ratio 4:1
// → expect static gain reduction of ~7.5 dB after the envelope settles.
const fxEng = new WasmEngine(48000);
const sampleCount = 48000;
const amp = Math.pow(10, -10 / 20);
const sine = new Float32Array(sampleCount);
for (let n = 0; n < sampleCount; n++) {
  sine[n] = amp * Math.sin((n / 48) * 2 * Math.PI);
}
const fxSrcId = fxEng.importWav("sine.wav", makeWav(sine), new Date().toISOString());
fxEng.applyOp(
  fxSrcId,
  JSON.stringify({
    Compress: {
      range: { start: 0, end: sampleCount },
      params: {
        threshold_db: -20.0,
        ratio: 4.0,
        attack_ms: 1.0,
        release_ms: 100.0,
        makeup_db: 0.0,
        knee_db: 0.0,
      },
    },
  }),
  new Date().toISOString(),
);
const compressed = fxEng.querySamples(fxSrcId, BigInt(sampleCount / 2), BigInt(sampleCount));
const tailPeak = Math.max(...Array.from(compressed).map(Math.abs));
const tailDb = 20 * Math.log10(tailPeak);
if (Math.abs(tailDb + 17.5) > 1.0) {
  console.error(`FAIL: compressor tail peak ${tailDb.toFixed(2)} dB, expected ~-17.5 dB`);
  process.exit(1);
}
console.log(`compressor settles at ${tailDb.toFixed(2)} dB (target -17.5 dB)`);

// EQ end-to-end: peak band at 1 kHz with +6 dB and Q=1, fed a 1 kHz sine.
// Steady-state ratio should match the band's gain.
const eqEng = new WasmEngine(48000);
const eqLen = 8192;
const eqInput = new Float32Array(eqLen);
const eqAmp = Math.pow(10, -12 / 20);
for (let n = 0; n < eqLen; n++) {
  eqInput[n] = eqAmp * Math.sin((n / 48) * 2 * Math.PI);
}
const eqWavBytes = makeWav(eqInput);
const eqSrcId = eqEng.importWav("eq.wav", eqWavBytes, new Date().toISOString());
eqEng.applyOp(
  eqSrcId,
  JSON.stringify({
    Eq: {
      range: { start: 0, end: eqLen },
      params: {
        bands: [
          {
            kind: "Peak",
            frequency_hz: 1000.0,
            gain_db: 6.0,
            q: 1.0,
            enabled: true,
          },
        ],
      },
    },
  }),
  new Date().toISOString(),
);
const eqOut = eqEng.querySamples(eqSrcId, BigInt(eqLen / 2), BigInt(eqLen));
const rms = (arr) => Math.sqrt(arr.reduce((s, x) => s + x * x, 0) / arr.length);
const inputTail = eqInput.slice(eqLen / 2);
const eqGainDb = 20 * Math.log10(rms(eqOut) / rms(inputTail));
if (Math.abs(eqGainDb - 6.0) > 0.5) {
  console.error(`FAIL: EQ peak band gain ${eqGainDb.toFixed(2)} dB, expected ~+6`);
  process.exit(1);
}
console.log(`EQ peak band measures ${eqGainDb.toFixed(2)} dB at centre (target +6 dB)`);

// Reverb end-to-end: feed an impulse, verify the wet output decays over the
// course of a second. A real-world reverb tail is loud near the impulse and
// progressively quieter — measuring two windows is enough to see the curve.
const revEng = new WasmEngine(48000);
const revLen = 48000;
const impulse = new Float32Array(revLen);
impulse[0] = 1.0;
const revSrcId = revEng.importWav("rev.wav", makeWav(impulse), new Date().toISOString());
revEng.applyOp(
  revSrcId,
  JSON.stringify({
    Reverb: {
      range: { start: 0, end: revLen },
      params: { model: "Hall", size: 0.5, damping: 0.5, mix: 1.0 },
    },
  }),
  new Date().toISOString(),
);
const revOut = revEng.querySamples(revSrcId, 0n, BigInt(revLen));
const earlyWindow = revOut.slice(2400, 4800); // 50 ms starting at 50 ms
const lateWindow = revOut.slice(38400, 40800); // 50 ms starting at 800 ms
const earlyRms = rms(earlyWindow);
const lateRms = rms(lateWindow);
if (lateRms >= earlyRms) {
  console.error(`FAIL: reverb late RMS ${lateRms} not less than early ${earlyRms}`);
  process.exit(1);
}
const ratio = earlyRms / Math.max(lateRms, 1e-9);
console.log(`reverb tail decays ${ratio.toFixed(1)}× from early to late window`);

// Noise reduction end-to-end: source contains pure noise (first 0.25 s) +
// signal-plus-noise (next 0.5 s). Capture profile from the noise region,
// apply NR, then verify the formerly-noisy first quarter is now quiet.
const nrEng = new WasmEngine(48000);
const nrFs = 48000;
const noiseLen = nrFs / 4;
const sigLen = nrFs / 2;
const total = noiseLen + sigLen;
const data = new Float32Array(total);
function rand(seed) {
  let s = seed | 1;
  return () => {
    s ^= s << 13;
    s ^= s >>> 17;
    s ^= s << 5;
    return ((s >>> 0) / 0xffffffff) * 2 - 1;
  };
}
const r1 = rand(0xdead);
for (let n = 0; n < noiseLen; n++) data[n] = 0.1 * r1();
const r2 = rand(0xbeef);
for (let n = 0; n < sigLen; n++) {
  data[noiseLen + n] = 0.5 * Math.sin((n / 48) * 2 * Math.PI) + 0.1 * r2();
}
const nrSrcId = nrEng.importWav("noisy.wav", makeWav(data), new Date().toISOString());
nrEng.captureNoiseProfile(nrSrcId, 0n, BigInt(noiseLen), "AC", "np_001", 512);
nrEng.applyOp(
  nrSrcId,
  JSON.stringify({
    NoiseReduce: {
      range: { start: 0, end: total },
      profile: "np_001",
      params: {
        amount_db: 24.0,
        floor_db: -30.0,
        oversubtraction: 1.5,
        attack_ms: 5.0,
        release_ms: 50.0,
        freq_smoothing: 0.0,
        fft_size: 512,
      },
    },
  }),
  new Date().toISOString(),
);
const beforeNoise = data.slice(0, noiseLen);
const beforeRms = rms(beforeNoise);
const afterNoise = nrEng.querySamples(nrSrcId, 0n, BigInt(noiseLen));
const afterRms = rms(afterNoise);
const reductionDb = 20 * Math.log10(afterRms / beforeRms);
if (reductionDb > -3.0) {
  console.error(`FAIL: NR reduction ${reductionDb.toFixed(2)} dB, expected < -3`);
  process.exit(1);
}
console.log(`noise reduction lowers floor by ${(-reductionDb).toFixed(2)} dB`);

// .kepz round-trip: export the NR engine's project, import into a fresh
// engine, verify the same query returns the same samples.
const before = nrEng.querySamples(nrSrcId, 0n, 1024n);
const archive = nrEng.exportKepz();
const restored = new WasmEngine(48000);
restored.importKepz(archive);
const after = restored.querySamples(nrSrcId, 0n, 1024n);
let mismatch = 0;
for (let i = 0; i < before.length; i++) {
  if (Math.abs(before[i] - after[i]) > 1e-6) mismatch++;
}
if (mismatch > 0) {
  console.error(`FAIL: kepz round-trip differs at ${mismatch} samples`);
  process.exit(1);
}
console.log(
  `kepz archive round-trip: ${archive.length}-byte zip preserves source samples`,
);

console.log("OK");
