/// <reference lib="webworker" />
import type { EngineCommand, EngineEvent } from "./protocol";
import {
  attachRingBuffer,
  freeFrames,
  resetRingBuffer,
  setProducerEnd,
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
  sourceFrameCount: (sourceId: string) => bigint | undefined;
  sourceSampleRate: (sourceId: string) => number | undefined;
  sourceChannelCount: (sourceId: string) => number | undefined;
  querySamples: (sourceId: string, startFrame: bigint, endFrame: bigint) => Float32Array;
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
  sessionStartFrame: number; // source-output-frame where this session began
  loopStartFrame: number; // loop region in (source-output-frames, inclusive)
  loopEndFrame: number; // loop region out (source-output-frames, exclusive)
  framesEmitted: number; // frames pushed since session start
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
          const firstIterFrames = loopEnd - sessionStart;
          // Non-loop: producer emits firstIterFrames then stops. Loop: producer
          // never ends; main thread tracks loop bounds separately for the
          // playhead modulo.
          if (!cmd.loop) setProducerEnd(ring, firstIterFrames);

          playback = {
            ring,
            rendered,
            sourceFrames,
            sourceChannels: sourceCh,
            sourceSampleRate: sourceSr,
            outputChannels: cmd.outputChannels,
            sourceLength,
            sessionStartFrame: sessionStart,
            loopStartFrame: loopStart,
            loopEndFrame: loopEnd,
            framesEmitted: 0,
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
  const firstIter = p.loopEndFrame - p.sessionStartFrame;
  const loopLen = p.loopEndFrame - p.loopStartFrame;
  if (p.framesEmitted >= firstIter && !p.loop) {
    // Reached the end of a non-loop session; let the consumer drain.
    stopPlayback();
    return;
  }
  // Map ring-frames-emitted to a source-output-frame and the contiguous run
  // remaining in this segment (first iteration runs from sessionStart..end;
  // subsequent loop iterations run from loopStart..end).
  let sourceOutputFrame: number;
  let remaining: number;
  if (p.framesEmitted < firstIter) {
    sourceOutputFrame = p.sessionStartFrame + p.framesEmitted;
    remaining = firstIter - p.framesEmitted;
  } else {
    sourceOutputFrame = p.loopStartFrame + ((p.framesEmitted - firstIter) % loopLen);
    remaining = p.loopEndFrame - sourceOutputFrame;
  }
  const chunkFrames = Math.min(free, remaining, 4096);
  const out = renderChunk(p, sourceOutputFrame, chunkFrames);
  const written = writeFrames(p.ring, out);
  p.framesEmitted += written;
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
