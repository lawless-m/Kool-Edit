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
  | { kind: "get_tempo"; req: RequestId }
  | {
      kind: "set_tempo";
      req: RequestId;
      bpm: number;
      beatsPerBar: number;
      beatUnit: number;
    }
  | { kind: "add_track"; req: RequestId; name: string }
  | { kind: "list_tracks"; req: RequestId }
  | { kind: "remove_track"; req: RequestId; trackId: number }
  | { kind: "set_track_gain"; req: RequestId; trackId: number; gainDb: number }
  | { kind: "set_track_mute"; req: RequestId; trackId: number; mute: boolean }
  | { kind: "set_track_solo"; req: RequestId; trackId: number; solo: boolean }
  | { kind: "set_track_name"; req: RequestId; trackId: number; name: string }
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
  | {
      kind: "set_clip_source_range";
      req: RequestId;
      trackId: number;
      clipId: number;
      sourceIn: number;
      sourceOut: number;
    }
  | { kind: "remove_clip"; req: RequestId; trackId: number; clipId: number }
  | {
      kind: "set_clip_group";
      req: RequestId;
      trackId: number;
      clipId: number;
      // 0 means ungroup; any other value sets that group id.
      groupId: number;
    }
  | { kind: "list_groups"; req: RequestId }
  | { kind: "set_group_name"; req: RequestId; groupId: number; name: string }
  | { kind: "remove_group"; req: RequestId; groupId: number }
  | { kind: "list_patterns"; req: RequestId }
  | { kind: "save_pattern"; req: RequestId; name: string; gridJson: string }
  | { kind: "load_pattern"; req: RequestId; name: string }
  | { kind: "remove_pattern"; req: RequestId; name: string }
  | { kind: "mixdown_wav"; req: RequestId }
  | { kind: "export_kepz"; req: RequestId }
  | { kind: "import_kepz"; req: RequestId; bytes: Uint8Array }
  | {
      kind: "detect_pitch_contour";
      req: RequestId;
      sourceId: string;
      startFrame: number;
      endFrame: number;
      hopSamples: number;
      windowSamples: number;
    }
  | { kind: "detect_bpm"; req: RequestId; sourceId: string }
  | { kind: "duplicate_source"; req: RequestId; sourceId: string; nowIso: string }
  | { kind: "rename_source"; req: RequestId; sourceId: string; newName: string }
  | { kind: "remove_source"; req: RequestId; sourceId: string }
  | { kind: "set_source_folder"; req: RequestId; sourceId: string; folder: string | null }
  | {
      kind: "create_empty_source";
      req: RequestId;
      lengthFrames: number;
      channels: number;
      desiredName: string;
      nowIso: string;
    }
  | {
      kind: "render_range_to_source";
      req: RequestId;
      startFrame: number;
      endFrame: number;
      desiredName: string;
      nowIso: string;
    }
  | {
      kind: "capture_noise_profile";
      req: RequestId;
      sourceId: string;
      startFrame: number;
      endFrame: number;
      name: string;
      profileId: string;
      fftSize: number;
    }
  | { kind: "list_noise_profiles"; req: RequestId }
  | {
      kind: "set_clip_envelope";
      req: RequestId;
      trackId: number;
      clipId: number;
      parameter: "volume" | "pan";
      breakpointsJson: string;
    }
  | {
      kind: "query_samples";
      req: RequestId;
      sourceId: string;
      startFrame: number;
      endFrame: number;
    }
  | {
      kind: "spectrogram_tile";
      req: RequestId;
      sourceId: string;
      startFrame: number;
      endFrame: number;
      fftSize: number;
      hopSize: number;
    };

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
  | { kind: "get_tempo_ok"; req: RequestId; json: string }
  | { kind: "set_tempo_ok"; req: RequestId }
  | { kind: "add_track_ok"; req: RequestId; trackId: number }
  | { kind: "list_tracks_ok"; req: RequestId; json: string }
  | { kind: "remove_track_ok"; req: RequestId; removed: boolean }
  | { kind: "set_track_gain_ok"; req: RequestId }
  | { kind: "set_track_mute_ok"; req: RequestId }
  | { kind: "set_track_solo_ok"; req: RequestId }
  | { kind: "set_track_name_ok"; req: RequestId }
  | { kind: "add_clip_ok"; req: RequestId; clipId: number }
  | { kind: "list_clips_ok"; req: RequestId; json: string }
  | { kind: "move_clip_ok"; req: RequestId }
  | { kind: "set_clip_source_range_ok"; req: RequestId }
  | { kind: "remove_clip_ok"; req: RequestId; removed: boolean }
  | { kind: "set_clip_group_ok"; req: RequestId }
  | { kind: "list_groups_ok"; req: RequestId; json: string }
  | { kind: "set_group_name_ok"; req: RequestId }
  | { kind: "remove_group_ok"; req: RequestId; removed: boolean }
  | { kind: "list_patterns_ok"; req: RequestId; json: string }
  | { kind: "save_pattern_ok"; req: RequestId }
  | { kind: "load_pattern_ok"; req: RequestId; gridJson: string | null }
  | { kind: "remove_pattern_ok"; req: RequestId; removed: boolean }
  | { kind: "mixdown_wav_ok"; req: RequestId; bytes: Uint8Array }
  | { kind: "export_kepz_ok"; req: RequestId; bytes: Uint8Array }
  | { kind: "import_kepz_ok"; req: RequestId }
  | { kind: "detect_pitch_contour_ok"; req: RequestId; contour: Float32Array }
  | { kind: "detect_bpm_ok"; req: RequestId; json: string }
  | { kind: "duplicate_source_ok"; req: RequestId; newSourceId: string }
  | { kind: "rename_source_ok"; req: RequestId }
  | { kind: "remove_source_ok"; req: RequestId; removed: boolean }
  | { kind: "set_source_folder_ok"; req: RequestId }
  | { kind: "create_empty_source_ok"; req: RequestId; newSourceId: string }
  | { kind: "render_range_to_source_ok"; req: RequestId; newSourceId: string }
  | { kind: "capture_noise_profile_ok"; req: RequestId }
  | { kind: "list_noise_profiles_ok"; req: RequestId; json: string }
  | { kind: "set_clip_envelope_ok"; req: RequestId }
  | { kind: "query_samples_ok"; req: RequestId; samples: Float32Array; channels: number }
  | {
      kind: "spectrogram_tile_ok";
      req: RequestId;
      magnitudes: Float32Array;
      frameCount: number;
      binCount: number;
    }
  | { kind: "error"; req: RequestId; reason: string };
