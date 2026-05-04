// Wire protocol between the UI (main thread) and the engine Worker.
// Every command carries a numeric request id; the matching event echoes it
// so the client can correlate replies even when commands are in flight.

import type { RingBufferLayout } from "../audio/ring-buffer";

export type RequestId = number;

export type EngineCommand =
  | { kind: "banner"; req: RequestId }
  | { kind: "import_wav"; req: RequestId; name: string; bytes: Uint8Array; nowIso: string }
  | {
      kind: "peak_summary";
      req: RequestId;
      sourceId: string;
      columns: number;
      // Optional source-frame range. When omitted (both 0) the engine
      // returns a summary of the entire source.
      startFrame: number;
      endFrame: number;
    }
  | {
      kind: "start_playback";
      req: RequestId;
      sourceId: string;
      outputSampleRate: number;
      outputChannels: number;
      loop: boolean;
      // All frame fields are in source-OUTPUT-frames (resampled to
      // outputSampleRate). loopEndFrame is exclusive.
      startFrame: number;
      loopStartFrame: number;
      loopEndFrame: number;
      ring: RingBufferLayout;
    }
  | { kind: "stop_playback"; req: RequestId };

export type EngineEvent =
  | { kind: "ready" }
  | { kind: "fatal"; reason: string }
  | { kind: "banner"; req: RequestId; banner: string }
  | {
      kind: "import_wav_ok";
      req: RequestId;
      sourceId: string;
      frames: number;
      sampleRate: number;
      channelCount: number;
    }
  | { kind: "peak_summary_ok"; req: RequestId; peaks: Float32Array }
  | { kind: "start_playback_ok"; req: RequestId; totalOutputFrames: number }
  | { kind: "stop_playback_ok"; req: RequestId }
  | { kind: "error"; req: RequestId; reason: string };
