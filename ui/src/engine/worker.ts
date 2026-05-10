/// <reference lib="webworker" />
import type { EngineCommand, EngineEvent } from "./protocol";
import {
  attachRingBuffer,
  freeFrames,
  loopEnd as ringLoopEnd,
  loopStart as ringLoopStart,
  resetRingBuffer,
  setLoopRange,
  setProducerEnd,
  setWorkerNextSourceFrame,
  writeFrame as ringWriteFrame,
  writeFrames,
  type RingBufferView,
} from "../audio/ring-buffer";

const ctx = self as unknown as DedicatedWorkerGlobalScope;

// `kool_edit_engine.js` is produced by `wasm-pack build engine --target web
// --out-dir ../ui/src/engine/pkg --features wasm`. Until it has been built
// the dynamic import fails; we surface a clear message rather than a bare
// ModuleNotFound. The path is built dynamically so neither TS nor Vite try
// to statically resolve a file that doesn't exist yet.
type WasmEngine = {
  importWav: (name: string, bytes: Uint8Array, nowIso: string) => string;
  peakSummary: (sourceId: string, columns: number) => Float32Array | undefined;
  peakSummaryRange: (
    sourceId: string,
    startFrame: bigint,
    endFrame: bigint,
    columns: number,
  ) => Float32Array | undefined;
  peakSummaryRangeChannels: (
    sourceId: string,
    startFrame: bigint,
    endFrame: bigint,
    columns: number,
  ) => Float32Array | undefined;
  sourceFrameCount: (sourceId: string) => bigint | undefined;
  sourceSampleRate: (sourceId: string) => number | undefined;
  sourceChannelCount: (sourceId: string) => number | undefined;
  querySamples: (sourceId: string, startFrame: bigint, endFrame: bigint) => Float32Array;
  applyOp: (sourceId: string, opJson: string, nowIso: string) => void;
  undo: (sourceId: string) => boolean;
  redo: (sourceId: string) => boolean;
  // Multitrack
  projectSampleRate: () => number;
  getTempo: () => string;
  setTempo: (bpm: number, beatsPerBar: number, beatUnit: number) => void;
  listSources: () => string;
  addTrack: (name: string) => bigint;
  listTracks: () => string;
  removeTrack: (trackId: bigint) => boolean;
  setTrackGain: (trackId: bigint, gainDb: number) => void;
  setTrackPan: (trackId: bigint, pan: number) => void;
  setTrackMute: (trackId: bigint, mute: boolean) => void;
  setTrackSolo: (trackId: bigint, solo: boolean) => void;
  setTrackName: (trackId: bigint, name: string) => void;
  addClip: (
    trackId: bigint,
    sourceId: string,
    positionFrame: bigint,
    sourceIn: bigint,
    sourceOut: bigint,
  ) => bigint;
  listClips: (trackId: bigint) => string | undefined;
  moveClip: (trackId: bigint, clipId: bigint, newPositionFrame: bigint) => void;
  setClipSourceRange: (
    trackId: bigint,
    clipId: bigint,
    sourceIn: bigint,
    sourceOut: bigint,
  ) => void;
  removeClip: (trackId: bigint, clipId: bigint) => boolean;
  setClipGroup: (trackId: bigint, clipId: bigint, groupId: bigint) => void;
  listGroups: () => string;
  setGroupName: (groupId: bigint, name: string) => void;
  removeGroup: (groupId: bigint) => boolean;
  listPatterns: () => string;
  savePattern: (name: string, gridJson: string) => void;
  loadPattern: (name: string) => string | undefined;
  removePattern: (name: string) => boolean;
  mixdownWav: () => Uint8Array;
  exportKepz: () => Uint8Array;
  importKepz: (bytes: Uint8Array) => void;
  detectPitchContour: (
    sourceId: string,
    startFrame: bigint,
    endFrame: bigint,
    hopSamples: number,
    windowSamples: number,
  ) => Float32Array;
  detectBpm: (sourceId: string) => string;
  duplicateSource: (sourceId: string, nowIso: string) => string;
  renameSource: (sourceId: string, newName: string) => void;
  removeSource: (sourceId: string) => boolean;
  setSourceFolder: (sourceId: string, folder: string | undefined) => void;
  createEmptySource: (
    lengthFrames: bigint,
    channels: number,
    desiredName: string,
    nowIso: string,
  ) => string;
  renderRangeToSource: (
    startFrame: bigint,
    endFrame: bigint,
    desiredName: string,
    nowIso: string,
  ) => string;
  captureNoiseProfile: (
    sourceId: string,
    startFrame: bigint,
    endFrame: bigint,
    name: string,
    profileId: string,
    fftSize: number,
  ) => void;
  listNoiseProfiles: () => string;
  setClipEnvelope: (
    trackId: bigint,
    clipId: bigint,
    parameter: string,
    breakpointsJson: string,
  ) => void;
  spectrogramTile: (
    sourceId: string,
    startFrame: bigint,
    endFrame: bigint,
    fftSize: number,
    hopSize: number,
  ) => Float32Array;
};

type EngineModule = {
  default: (input?: unknown) => Promise<unknown>;
  banner: () => string;
  WasmEngine: new (sampleRate: number) => WasmEngine;
};

async function loadWasm(): Promise<EngineModule | { error: string }> {
  // Both URLs use string literals so Vite recognises them as asset references
  // and emits both files in the production bundle. The dynamic `import()` is
  // wrapped in a try/catch because the JS bundle only exists once
  // `wasm-pack build` has run.
  const jsUrl = new URL("./pkg/kool_edit_engine.js", import.meta.url).href;
  const wasmUrl = new URL("./pkg/kool_edit_engine_bg.wasm", import.meta.url).href;
  try {
    const mod = (await import(/* @vite-ignore */ jsUrl)) as EngineModule;
    await mod.default(wasmUrl);
    return mod;
  } catch {
    return {
      error:
        "wasm not built. run `make engine` (or `npm run build:engine` from ui/), then reload.",
    };
  }
}

function send(ev: EngineEvent): void {
  ctx.postMessage(ev);
}

interface PlaybackState {
  ring: RingBufferView;
  rendered: Float32Array; // interleaved at sourceSampleRate × sourceChannels
  sourceFrames: number;
  sourceChannels: number;
  sourceSampleRate: number;
  outputChannels: number; // always 2 for v1
  sourceLength: number; // full source length in output frames
  // Loop bounds live in the SAB header so the main thread can update them
  // live. The worker holds nothing about them except what it reads each tick.
  // workerNextSourceFrame is the source-output-frame the worker will emit
  // next; we mirror the SAB slot here as a JS-side cursor.
  workerNextSourceFrame: number;
  loop: boolean;
  ratio: number; // sourceSampleRate / outputSampleRate
  timer: ReturnType<typeof setTimeout> | null;
}

let playback: PlaybackState | null = null;

(async () => {
  const loaded = await loadWasm();
  if ("error" in loaded) {
    send({ kind: "fatal", reason: loaded.error });
    return;
  }

  const wasm = loaded;
  const engine = new wasm.WasmEngine(96_000);

  ctx.onmessage = (ev: MessageEvent<EngineCommand>) => {
    const cmd = ev.data;
    try {
      switch (cmd.kind) {
        case "banner":
          send({ kind: "banner", req: cmd.req, banner: wasm.banner() });
          return;

        case "import_wav": {
          const sourceId = engine.importWav(cmd.name, cmd.bytes, cmd.nowIso);
          const framesBig = engine.sourceFrameCount(sourceId);
          const frames = framesBig === undefined ? 0 : Number(framesBig);
          const sampleRate = engine.sourceSampleRate(sourceId) ?? 0;
          const channelCount = engine.sourceChannelCount(sourceId) ?? 1;
          send({
            kind: "import_wav_ok",
            req: cmd.req,
            sourceId,
            frames,
            sampleRate,
            channelCount,
          });
          return;
        }

        case "peak_summary": {
          const useRange = cmd.startFrame > 0 || cmd.endFrame > 0;
          const peaks = useRange
            ? engine.peakSummaryRange(
                cmd.sourceId,
                BigInt(Math.floor(cmd.startFrame)),
                BigInt(Math.floor(cmd.endFrame)),
                cmd.columns,
              )
            : engine.peakSummary(cmd.sourceId, cmd.columns);
          if (!peaks) {
            send({ kind: "error", req: cmd.req, reason: `unknown source ${cmd.sourceId}` });
            return;
          }
          send({ kind: "peak_summary_ok", req: cmd.req, peaks });
          return;
        }

        case "peak_summary_channels": {
          const peaks = engine.peakSummaryRangeChannels(
            cmd.sourceId,
            BigInt(Math.floor(cmd.startFrame)),
            BigInt(Math.floor(cmd.endFrame)),
            cmd.columns,
          );
          if (!peaks) {
            send({ kind: "error", req: cmd.req, reason: `unknown source ${cmd.sourceId}` });
            return;
          }
          send({ kind: "peak_summary_channels_ok", req: cmd.req, peaks });
          return;
        }

        case "start_playback": {
          stopPlayback();
          const framesBig = engine.sourceFrameCount(cmd.sourceId);
          const sourceSr = engine.sourceSampleRate(cmd.sourceId);
          const sourceCh = engine.sourceChannelCount(cmd.sourceId);
          if (framesBig === undefined || sourceSr === undefined || sourceCh === undefined) {
            send({ kind: "error", req: cmd.req, reason: `unknown source ${cmd.sourceId}` });
            return;
          }
          const sourceFrames = Number(framesBig);
          const rendered = engine.querySamples(cmd.sourceId, 0n, framesBig);

          const ring = attachRingBuffer(cmd.ring);
          resetRingBuffer(ring);

          const ratio = sourceSr / cmd.outputSampleRate;
          const sourceLength = Math.max(0, Math.floor(sourceFrames / ratio));
          // Clamp the loop region to [0, sourceLength], with end > start.
          const loopStart = Math.min(
            Math.max(0, Math.floor(cmd.loopStartFrame)),
            Math.max(0, sourceLength),
          );
          const loopEnd = Math.min(
            Math.max(loopStart + 1, Math.floor(cmd.loopEndFrame)),
            sourceLength,
          );
          // Clamp session start into the loop region.
          const sessionStart = Math.min(
            Math.max(loopStart, Math.floor(cmd.startFrame)),
            Math.max(loopStart, loopEnd - 1),
          );
          // Publish the initial loop bounds and worker cursor to the SAB so
          // the main thread can both observe them and update them live.
          setLoopRange(ring, loopStart, loopEnd);
          setWorkerNextSourceFrame(ring, sessionStart);

          playback = {
            ring,
            rendered,
            sourceFrames,
            sourceChannels: sourceCh,
            sourceSampleRate: sourceSr,
            outputChannels: cmd.outputChannels,
            sourceLength,
            workerNextSourceFrame: sessionStart,
            loop: cmd.loop,
            ratio,
            timer: null,
          };
          send({ kind: "start_playback_ok", req: cmd.req, totalOutputFrames: sourceLength });
          scheduleFill();
          return;
        }

        case "stop_playback": {
          stopPlayback();
          send({ kind: "stop_playback_ok", req: cmd.req });
          return;
        }

        case "apply_op": {
          engine.applyOp(cmd.sourceId, cmd.opJson, cmd.nowIso);
          send({ kind: "apply_op_ok", req: cmd.req });
          return;
        }

        case "undo": {
          const didUndo = engine.undo(cmd.sourceId);
          send({ kind: "undo_ok", req: cmd.req, didUndo });
          return;
        }

        case "redo": {
          const didRedo = engine.redo(cmd.sourceId);
          send({ kind: "redo_ok", req: cmd.req, didRedo });
          return;
        }

        case "list_sources": {
          send({ kind: "list_sources_ok", req: cmd.req, json: engine.listSources() });
          return;
        }

        case "project_sample_rate": {
          send({
            kind: "project_sample_rate_ok",
            req: cmd.req,
            sampleRate: engine.projectSampleRate(),
          });
          return;
        }

        case "get_tempo": {
          send({ kind: "get_tempo_ok", req: cmd.req, json: engine.getTempo() });
          return;
        }

        case "set_tempo": {
          engine.setTempo(cmd.bpm, cmd.beatsPerBar, cmd.beatUnit);
          send({ kind: "set_tempo_ok", req: cmd.req });
          return;
        }

        case "add_track": {
          const trackId = Number(engine.addTrack(cmd.name));
          send({ kind: "add_track_ok", req: cmd.req, trackId });
          return;
        }

        case "list_tracks": {
          send({ kind: "list_tracks_ok", req: cmd.req, json: engine.listTracks() });
          return;
        }

        case "remove_track": {
          const removed = engine.removeTrack(BigInt(cmd.trackId));
          send({ kind: "remove_track_ok", req: cmd.req, removed });
          return;
        }

        case "set_track_gain": {
          engine.setTrackGain(BigInt(cmd.trackId), cmd.gainDb);
          send({ kind: "set_track_gain_ok", req: cmd.req });
          return;
        }
        case "set_track_pan": {
          engine.setTrackPan(BigInt(cmd.trackId), cmd.pan);
          send({ kind: "set_track_pan_ok", req: cmd.req });
          return;
        }

        case "set_track_mute": {
          engine.setTrackMute(BigInt(cmd.trackId), cmd.mute);
          send({ kind: "set_track_mute_ok", req: cmd.req });
          return;
        }

        case "set_track_solo": {
          engine.setTrackSolo(BigInt(cmd.trackId), cmd.solo);
          send({ kind: "set_track_solo_ok", req: cmd.req });
          return;
        }

        case "set_track_name": {
          engine.setTrackName(BigInt(cmd.trackId), cmd.name);
          send({ kind: "set_track_name_ok", req: cmd.req });
          return;
        }

        case "add_clip": {
          const clipId = Number(
            engine.addClip(
              BigInt(cmd.trackId),
              cmd.sourceId,
              BigInt(Math.floor(cmd.positionFrame)),
              BigInt(Math.floor(cmd.sourceIn)),
              BigInt(Math.floor(cmd.sourceOut)),
            ),
          );
          send({ kind: "add_clip_ok", req: cmd.req, clipId });
          return;
        }

        case "list_clips": {
          const json = engine.listClips(BigInt(cmd.trackId));
          if (json === undefined) {
            send({ kind: "error", req: cmd.req, reason: `unknown track ${cmd.trackId}` });
            return;
          }
          send({ kind: "list_clips_ok", req: cmd.req, json });
          return;
        }

        case "move_clip": {
          engine.moveClip(
            BigInt(cmd.trackId),
            BigInt(cmd.clipId),
            BigInt(Math.floor(cmd.newPositionFrame)),
          );
          send({ kind: "move_clip_ok", req: cmd.req });
          return;
        }

        case "set_clip_source_range": {
          engine.setClipSourceRange(
            BigInt(cmd.trackId),
            BigInt(cmd.clipId),
            BigInt(Math.floor(cmd.sourceIn)),
            BigInt(Math.floor(cmd.sourceOut)),
          );
          send({ kind: "set_clip_source_range_ok", req: cmd.req });
          return;
        }

        case "remove_clip": {
          const removed = engine.removeClip(BigInt(cmd.trackId), BigInt(cmd.clipId));
          send({ kind: "remove_clip_ok", req: cmd.req, removed });
          return;
        }

        case "set_clip_group": {
          engine.setClipGroup(
            BigInt(cmd.trackId),
            BigInt(cmd.clipId),
            BigInt(cmd.groupId),
          );
          send({ kind: "set_clip_group_ok", req: cmd.req });
          return;
        }

        case "list_groups": {
          const json = engine.listGroups();
          send({ kind: "list_groups_ok", req: cmd.req, json });
          return;
        }

        case "set_group_name": {
          engine.setGroupName(BigInt(cmd.groupId), cmd.name);
          send({ kind: "set_group_name_ok", req: cmd.req });
          return;
        }

        case "remove_group": {
          const removed = engine.removeGroup(BigInt(cmd.groupId));
          send({ kind: "remove_group_ok", req: cmd.req, removed });
          return;
        }

        case "list_patterns": {
          const json = engine.listPatterns();
          send({ kind: "list_patterns_ok", req: cmd.req, json });
          return;
        }

        case "save_pattern": {
          engine.savePattern(cmd.name, cmd.gridJson);
          send({ kind: "save_pattern_ok", req: cmd.req });
          return;
        }

        case "load_pattern": {
          const gridJson = engine.loadPattern(cmd.name) ?? null;
          send({ kind: "load_pattern_ok", req: cmd.req, gridJson });
          return;
        }

        case "remove_pattern": {
          const removed = engine.removePattern(cmd.name);
          send({ kind: "remove_pattern_ok", req: cmd.req, removed });
          return;
        }

        case "mixdown_wav": {
          const bytes = engine.mixdownWav();
          send({ kind: "mixdown_wav_ok", req: cmd.req, bytes });
          return;
        }

        case "export_kepz": {
          const bytes = engine.exportKepz();
          send({ kind: "export_kepz_ok", req: cmd.req, bytes });
          return;
        }

        case "import_kepz": {
          engine.importKepz(cmd.bytes);
          send({ kind: "import_kepz_ok", req: cmd.req });
          return;
        }
        case "detect_pitch_contour": {
          const contour = engine.detectPitchContour(
            cmd.sourceId,
            BigInt(cmd.startFrame),
            BigInt(cmd.endFrame),
            cmd.hopSamples,
            cmd.windowSamples,
          );
          send({ kind: "detect_pitch_contour_ok", req: cmd.req, contour });
          return;
        }
        case "detect_bpm": {
          const json = engine.detectBpm(cmd.sourceId);
          send({ kind: "detect_bpm_ok", req: cmd.req, json });
          return;
        }

        case "duplicate_source": {
          const newSourceId = engine.duplicateSource(cmd.sourceId, cmd.nowIso);
          send({ kind: "duplicate_source_ok", req: cmd.req, newSourceId });
          return;
        }

        case "rename_source": {
          engine.renameSource(cmd.sourceId, cmd.newName);
          send({ kind: "rename_source_ok", req: cmd.req });
          return;
        }

        case "remove_source": {
          const removed = engine.removeSource(cmd.sourceId);
          send({ kind: "remove_source_ok", req: cmd.req, removed });
          return;
        }

        case "set_source_folder": {
          engine.setSourceFolder(cmd.sourceId, cmd.folder ?? undefined);
          send({ kind: "set_source_folder_ok", req: cmd.req });
          return;
        }

        case "create_empty_source": {
          const newSourceId = engine.createEmptySource(
            BigInt(Math.floor(cmd.lengthFrames)),
            cmd.channels,
            cmd.desiredName,
            cmd.nowIso,
          );
          send({ kind: "create_empty_source_ok", req: cmd.req, newSourceId });
          return;
        }

        case "render_range_to_source": {
          const newSourceId = engine.renderRangeToSource(
            BigInt(Math.floor(cmd.startFrame)),
            BigInt(Math.floor(cmd.endFrame)),
            cmd.desiredName,
            cmd.nowIso,
          );
          send({ kind: "render_range_to_source_ok", req: cmd.req, newSourceId });
          return;
        }

        case "capture_noise_profile": {
          engine.captureNoiseProfile(
            cmd.sourceId,
            BigInt(Math.floor(cmd.startFrame)),
            BigInt(Math.floor(cmd.endFrame)),
            cmd.name,
            cmd.profileId,
            cmd.fftSize,
          );
          send({ kind: "capture_noise_profile_ok", req: cmd.req });
          return;
        }

        case "list_noise_profiles": {
          send({
            kind: "list_noise_profiles_ok",
            req: cmd.req,
            json: engine.listNoiseProfiles(),
          });
          return;
        }

        case "set_clip_envelope": {
          engine.setClipEnvelope(
            BigInt(cmd.trackId),
            BigInt(cmd.clipId),
            cmd.parameter,
            cmd.breakpointsJson,
          );
          send({ kind: "set_clip_envelope_ok", req: cmd.req });
          return;
        }

        case "query_samples": {
          const ch = engine.sourceChannelCount(cmd.sourceId);
          if (ch === undefined) {
            send({ kind: "error", req: cmd.req, reason: `unknown source ${cmd.sourceId}` });
            return;
          }
          const samples = engine.querySamples(
            cmd.sourceId,
            BigInt(Math.floor(cmd.startFrame)),
            BigInt(Math.floor(cmd.endFrame)),
          );
          send({ kind: "query_samples_ok", req: cmd.req, samples, channels: ch });
          return;
        }

        case "spectrogram_tile": {
          const start = BigInt(Math.floor(cmd.startFrame));
          const end = BigInt(Math.floor(cmd.endFrame));
          const magnitudes = engine.spectrogramTile(
            cmd.sourceId,
            start,
            end,
            cmd.fftSize,
            cmd.hopSize,
          );
          const binCount = (cmd.fftSize >>> 1) + 1;
          const frameCount = magnitudes.length / binCount;
          send({
            kind: "spectrogram_tile_ok",
            req: cmd.req,
            magnitudes,
            frameCount,
            binCount,
          });
          return;
        }
      }
    } catch (e) {
      send({ kind: "error", req: cmd.req, reason: String(e) });
    }
  };

  send({ kind: "ready" });
})();

function stopPlayback(): void {
  if (!playback) return;
  if (playback.timer !== null) clearTimeout(playback.timer);
  playback = null;
}

function scheduleFill(): void {
  if (!playback) return;
  // Wait long enough that we sleep when the buffer's full, but short enough
  // to refill before underrun. ~10 ms is well under the 200 ms ring capacity.
  playback.timer = setTimeout(fillTick, 10);
}

function fillTick(): void {
  if (!playback) return;
  const p = playback;
  const free = freeFrames(p.ring);
  if (free <= 0) {
    scheduleFill();
    return;
  }
  // Read live loop bounds from the SAB. The main thread updates them on
  // selection drag/trim/numerical input.
  const loopStartF = ringLoopStart(p.ring);
  const loopEndF = ringLoopEnd(p.ring);
  if (loopEndF <= loopStartF) {
    // Bounds collapsed (shouldn't happen with main-thread guards, but defend).
    scheduleFill();
    return;
  }

  // If the cursor sits outside the current loop region — either because the
  // user shrank the bounds past it, or this is a fresh wrap — snap into the
  // region (or end the session if non-loop).
  if (p.workerNextSourceFrame >= loopEndF) {
    if (!p.loop) {
      // Mark the producer as done at the current ring write head; the
      // consumer drains and the main thread's end-watcher fires.
      setProducerEnd(p.ring, ringWriteFrame(p.ring));
      stopPlayback();
      return;
    }
    p.workerNextSourceFrame = loopStartF;
  } else if (p.workerNextSourceFrame < loopStartF) {
    p.workerNextSourceFrame = loopStartF;
  }

  const sourceOutputFrame = p.workerNextSourceFrame;
  const remaining = loopEndF - sourceOutputFrame;
  const chunkFrames = Math.min(free, remaining, 4096);
  const out = renderChunk(p, sourceOutputFrame, chunkFrames);
  const written = writeFrames(p.ring, out);
  p.workerNextSourceFrame = sourceOutputFrame + written;
  setWorkerNextSourceFrame(p.ring, p.workerNextSourceFrame);
  scheduleFill();
}

/** Linear-interpolation resample + mono→stereo upmix. */
function renderChunk(p: PlaybackState, outStartFrame: number, outFrames: number): Float32Array {
  const out = new Float32Array(outFrames * p.outputChannels);
  const sc = p.sourceChannels;
  const src = p.rendered;
  const lastInputFrame = p.sourceFrames - 1;
  for (let i = 0; i < outFrames; i++) {
    const pos = (outStartFrame + i) * p.ratio;
    const i0 = Math.floor(pos);
    const frac = pos - i0;
    const i1 = Math.min(i0 + 1, lastInputFrame);
    if (sc === 1) {
      const a = src[i0] ?? 0;
      const b = src[i1] ?? 0;
      const s = a + (b - a) * frac;
      out[i * 2] = s;
      out[i * 2 + 1] = s;
    } else {
      // Stereo source: interleave at i0*sc, i0*sc+1; same for i1.
      const aL = src[i0 * sc] ?? 0;
      const aR = src[i0 * sc + 1] ?? 0;
      const bL = src[i1 * sc] ?? 0;
      const bR = src[i1 * sc + 1] ?? 0;
      out[i * 2] = aL + (bL - aL) * frac;
      out[i * 2 + 1] = aR + (bR - aR) * frac;
    }
  }
  return out;
}
