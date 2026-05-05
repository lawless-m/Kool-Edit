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
  | { kind: "stop_playback"; req: RequestId }
  | {
      kind: "apply_op";
      req: RequestId;
      sourceId: string;
      opJson: string;
      nowIso: string;
    }
  | { kind: "undo"; req: RequestId; sourceId: string }
  | { kind: "redo"; req: RequestId; sourceId: string }
  | { kind: "list_sources"; req: RequestId }
  | { kind: "project_sample_rate"; req: RequestId }
  | { kind: "add_track"; req: RequestId; name: string }
  | { kind: "list_tracks"; req: RequestId }
  | { kind: "remove_track"; req: RequestId; trackId: number }
  | { kind: "set_track_gain"; req: RequestId; trackId: number; gainDb: number }
  | { kind: "set_track_mute"; req: RequestId; trackId: number; mute: boolean }
  | { kind: "set_track_solo"; req: RequestId; trackId: number; solo: boolean }
  | {
      kind: "add_clip";
      req: RequestId;
      trackId: number;
      sourceId: string;
      positionFrame: number;
      sourceIn: number;
      sourceOut: number;
    }
  | { kind: "list_clips"; req: RequestId; trackId: number }
  | {
      kind: "move_clip";
      req: RequestId;
      trackId: number;
      clipId: number;
      newPositionFrame: number;
    }
  | { kind: "remove_clip"; req: RequestId; trackId: number; clipId: number }
  | { kind: "mixdown_wav"; req: RequestId };

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
  | { kind: "apply_op_ok"; req: RequestId }
  | { kind: "undo_ok"; req: RequestId; didUndo: boolean }
  | { kind: "redo_ok"; req: RequestId; didRedo: boolean }
  | { kind: "list_sources_ok"; req: RequestId; json: string }
  | { kind: "project_sample_rate_ok"; req: RequestId; sampleRate: number }
  | { kind: "add_track_ok"; req: RequestId; trackId: number }
  | { kind: "list_tracks_ok"; req: RequestId; json: string }
  | { kind: "remove_track_ok"; req: RequestId; removed: boolean }
  | { kind: "set_track_gain_ok"; req: RequestId }
  | { kind: "set_track_mute_ok"; req: RequestId }
  | { kind: "set_track_solo_ok"; req: RequestId }
  | { kind: "add_clip_ok"; req: RequestId; clipId: number }
  | { kind: "list_clips_ok"; req: RequestId; json: string }
  | { kind: "move_clip_ok"; req: RequestId }
  | { kind: "remove_clip_ok"; req: RequestId; removed: boolean }
  | { kind: "mixdown_wav_ok"; req: RequestId; bytes: Uint8Array }
  | { kind: "error"; req: RequestId; reason: string };
