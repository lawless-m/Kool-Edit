import type { EngineCommand, EngineEvent, RequestId } from "./protocol";
import type { RingBufferLayout } from "../audio/ring-buffer";

type Pending = (ev: EngineEvent) => void;

export class EngineUnavailable extends Error {}

export interface SourceInfo {
  id: string;
  name: string;
  frames: number;
  sampleRate: number;
  channels: number;
  folder: string | null;
}

export interface TrackInfo {
  id: number;
  name: string;
  mute: boolean;
  solo: boolean;
  gainDb: number;
  pan: number;
  clipCount: number;
}

export interface Breakpoint {
  time: number;
  value: number;
  curve: "Linear" | "Exponential" | "Logarithmic" | "Hold" | "SCurve";
}

export interface ClipInfo {
  id: number;
  sourceId: string;
  name: string;
  position: number;
  endPosition: number;
  sourceIn: number;
  sourceOut: number;
  gainDb: number;
  pan: number;
  volumeEnvelope: Breakpoint[];
  panEnvelope: Breakpoint[];
  // 0 means ungrouped. Any other value identifies a clip group whose
  // members move and select together in the arranger.
  group: number;
}

export interface GroupInfo {
  id: number;
  name: string;
}

export interface PatternInfo {
  name: string;
  gridJson: string;
}

/** Result of `detectBpm`. `candidates` is up to three `[bpm, normScore]`
 *  pairs ordered best-first; the top score is always 1.0. `confidence` is
 *  the top score divided by the sum of all returned candidate scores
 *  (~1.0 = clear winner; ~0.33 = three roughly equal candidates). */
export interface TempoEstimate {
  bpm: number;
  confidence: number;
  candidates: Array<[number, number]>;
}

export interface NoiseProfileInfo {
  id: string;
  name: string;
  sourceId: string;
  start: number;
  end: number;
  magnitudeBins: number;
}

/**
 * Promise-returning wrapper around the engine Worker. A single client owns
 * one worker; concurrent calls are correlated by request id.
 */
export class EngineClient {
  private worker: Worker;
  private nextReq: RequestId = 1;
  private pending = new Map<RequestId, Pending>();

  private constructor(worker: Worker) {
    this.worker = worker;
    worker.onmessage = (ev: MessageEvent<EngineEvent>) => this.dispatch(ev.data);
  }

  /** Boot a new engine worker, resolving once it has signalled `ready`. */
  static async boot(): Promise<EngineClient> {
    const worker = new Worker(new URL("./worker.ts", import.meta.url), {
      type: "module",
      name: "kool-edit-engine",
    });

    return new Promise<EngineClient>((resolve, reject) => {
      const onReady = (ev: MessageEvent<EngineEvent>) => {
        const msg = ev.data;
        if (msg.kind === "ready") {
          worker.removeEventListener("message", onReady);
          worker.removeEventListener("error", onError);
          resolve(new EngineClient(worker));
        } else if (msg.kind === "fatal") {
          worker.removeEventListener("message", onReady);
          worker.removeEventListener("error", onError);
          worker.terminate();
          reject(new EngineUnavailable(msg.reason));
        }
      };
      const onError = (e: ErrorEvent) => {
        worker.removeEventListener("message", onReady);
        worker.removeEventListener("error", onError);
        worker.terminate();
        reject(new EngineUnavailable(e.message || "worker error"));
      };
      worker.addEventListener("message", onReady);
      worker.addEventListener("error", onError);
    });
  }

  async banner(): Promise<string> {
    const ev = await this.request<EngineCommand & { kind: "banner" }>(
      (req) => ({ kind: "banner", req }),
    );
    if (ev.kind !== "banner") throw new Error("unexpected event for banner");
    return ev.banner;
  }

  async importWav(
    name: string,
    bytes: Uint8Array,
  ): Promise<{
    sourceId: string;
    frames: number;
    sampleRate: number;
    channelCount: number;
  }> {
    const nowIso = new Date().toISOString();
    const ev = await this.request<EngineCommand & { kind: "import_wav" }>(
      (req) => ({ kind: "import_wav", req, name, bytes, nowIso }),
    );
    if (ev.kind !== "import_wav_ok") throw new Error("unexpected event");
    return {
      sourceId: ev.sourceId,
      frames: ev.frames,
      sampleRate: ev.sampleRate,
      channelCount: ev.channelCount,
    };
  }

  async peakSummary(
    sourceId: string,
    columns: number,
    startFrame = 0,
    endFrame = 0,
  ): Promise<Float32Array> {
    const ev = await this.request<EngineCommand & { kind: "peak_summary" }>(
      (req) => ({ kind: "peak_summary", req, sourceId, columns, startFrame, endFrame }),
    );
    if (ev.kind !== "peak_summary_ok") throw new Error("unexpected event");
    return ev.peaks;
  }

  async startPlayback(
    sourceId: string,
    outputSampleRate: number,
    outputChannels: number,
    loop: boolean,
    startFrame: number,
    loopStartFrame: number,
    loopEndFrame: number,
    ring: RingBufferLayout,
  ): Promise<{ totalOutputFrames: number }> {
    const ev = await this.request<EngineCommand & { kind: "start_playback" }>(
      (req) => ({
        kind: "start_playback",
        req,
        sourceId,
        outputSampleRate,
        outputChannels,
        loop,
        startFrame,
        loopStartFrame,
        loopEndFrame,
        ring,
      }),
    );
    if (ev.kind !== "start_playback_ok") throw new Error("unexpected event");
    return { totalOutputFrames: ev.totalOutputFrames };
  }

  async stopPlayback(): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "stop_playback" }>(
      (req) => ({ kind: "stop_playback", req }),
    );
    if (ev.kind !== "stop_playback_ok") throw new Error("unexpected event");
  }

  async applyOp(sourceId: string, opJson: string): Promise<void> {
    const nowIso = new Date().toISOString();
    const ev = await this.request<EngineCommand & { kind: "apply_op" }>(
      (req) => ({ kind: "apply_op", req, sourceId, opJson, nowIso }),
    );
    if (ev.kind !== "apply_op_ok") throw new Error("unexpected event");
  }

  async undo(sourceId: string): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "undo" }>(
      (req) => ({ kind: "undo", req, sourceId }),
    );
    if (ev.kind !== "undo_ok") throw new Error("unexpected event");
    return ev.didUndo;
  }

  async redo(sourceId: string): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "redo" }>(
      (req) => ({ kind: "redo", req, sourceId }),
    );
    if (ev.kind !== "redo_ok") throw new Error("unexpected event");
    return ev.didRedo;
  }

  // ---- multitrack ------------------------------------------------------

  async projectSampleRate(): Promise<number> {
    const ev = await this.request<EngineCommand & { kind: "project_sample_rate" }>(
      (req) => ({ kind: "project_sample_rate", req }),
    );
    if (ev.kind !== "project_sample_rate_ok") throw new Error("unexpected event");
    return ev.sampleRate;
  }

  async getTempo(): Promise<{ bpm: number; beatsPerBar: number; beatUnit: number }> {
    const ev = await this.request<EngineCommand & { kind: "get_tempo" }>(
      (req) => ({ kind: "get_tempo", req }),
    );
    if (ev.kind !== "get_tempo_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as { bpm: number; beatsPerBar: number; beatUnit: number };
  }

  async setTempo(bpm: number, beatsPerBar: number, beatUnit: number): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_tempo" }>(
      (req) => ({ kind: "set_tempo", req, bpm, beatsPerBar, beatUnit }),
    );
    if (ev.kind !== "set_tempo_ok") throw new Error("unexpected event");
  }

  async listSources(): Promise<SourceInfo[]> {
    const ev = await this.request<EngineCommand & { kind: "list_sources" }>(
      (req) => ({ kind: "list_sources", req }),
    );
    if (ev.kind !== "list_sources_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as SourceInfo[];
  }

  async addTrack(name: string): Promise<number> {
    const ev = await this.request<EngineCommand & { kind: "add_track" }>(
      (req) => ({ kind: "add_track", req, name }),
    );
    if (ev.kind !== "add_track_ok") throw new Error("unexpected event");
    return ev.trackId;
  }

  async listTracks(): Promise<TrackInfo[]> {
    const ev = await this.request<EngineCommand & { kind: "list_tracks" }>(
      (req) => ({ kind: "list_tracks", req }),
    );
    if (ev.kind !== "list_tracks_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as TrackInfo[];
  }

  async removeTrack(trackId: number): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "remove_track" }>(
      (req) => ({ kind: "remove_track", req, trackId }),
    );
    if (ev.kind !== "remove_track_ok") throw new Error("unexpected event");
    return ev.removed;
  }

  async setTrackGain(trackId: number, gainDb: number): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_track_gain" }>(
      (req) => ({ kind: "set_track_gain", req, trackId, gainDb }),
    );
    if (ev.kind !== "set_track_gain_ok") throw new Error("unexpected event");
  }

  /** Set the static stereo pan for a track. `pan` is clamped to
   *  [-1, +1] (L → R) by the engine. */
  async setTrackPan(trackId: number, pan: number): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_track_pan" }>(
      (req) => ({ kind: "set_track_pan", req, trackId, pan }),
    );
    if (ev.kind !== "set_track_pan_ok") throw new Error("unexpected event");
  }

  async setTrackMute(trackId: number, mute: boolean): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_track_mute" }>(
      (req) => ({ kind: "set_track_mute", req, trackId, mute }),
    );
    if (ev.kind !== "set_track_mute_ok") throw new Error("unexpected event");
  }

  async setTrackSolo(trackId: number, solo: boolean): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_track_solo" }>(
      (req) => ({ kind: "set_track_solo", req, trackId, solo }),
    );
    if (ev.kind !== "set_track_solo_ok") throw new Error("unexpected event");
  }

  async setTrackName(trackId: number, name: string): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_track_name" }>(
      (req) => ({ kind: "set_track_name", req, trackId, name }),
    );
    if (ev.kind !== "set_track_name_ok") throw new Error("unexpected event");
  }

  async addClip(
    trackId: number,
    sourceId: string,
    positionFrame: number,
    sourceIn: number,
    sourceOut: number,
  ): Promise<number> {
    const ev = await this.request<EngineCommand & { kind: "add_clip" }>(
      (req) => ({
        kind: "add_clip",
        req,
        trackId,
        sourceId,
        positionFrame,
        sourceIn,
        sourceOut,
      }),
    );
    if (ev.kind !== "add_clip_ok") throw new Error("unexpected event");
    return ev.clipId;
  }

  async listClips(trackId: number): Promise<ClipInfo[]> {
    const ev = await this.request<EngineCommand & { kind: "list_clips" }>(
      (req) => ({ kind: "list_clips", req, trackId }),
    );
    if (ev.kind !== "list_clips_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as ClipInfo[];
  }

  async moveClip(trackId: number, clipId: number, newPositionFrame: number): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "move_clip" }>(
      (req) => ({ kind: "move_clip", req, trackId, clipId, newPositionFrame }),
    );
    if (ev.kind !== "move_clip_ok") throw new Error("unexpected event");
  }

  async setClipSourceRange(
    trackId: number,
    clipId: number,
    sourceIn: number,
    sourceOut: number,
  ): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_clip_source_range" }>(
      (req) => ({ kind: "set_clip_source_range", req, trackId, clipId, sourceIn, sourceOut }),
    );
    if (ev.kind !== "set_clip_source_range_ok") throw new Error("unexpected event");
  }

  async removeClip(trackId: number, clipId: number): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "remove_clip" }>(
      (req) => ({ kind: "remove_clip", req, trackId, clipId }),
    );
    if (ev.kind !== "remove_clip_ok") throw new Error("unexpected event");
    return ev.removed;
  }

  /** Assign a clip to a group. `groupId === 0` removes any existing group. */
  async setClipGroup(trackId: number, clipId: number, groupId: number): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_clip_group" }>(
      (req) => ({ kind: "set_clip_group", req, trackId, clipId, groupId }),
    );
    if (ev.kind !== "set_clip_group_ok") throw new Error("unexpected event");
  }

  /** Return the named groups stored in the project. Clips can reference a
   *  group id that has no name entry — UIs should fall back to "Group N". */
  async listGroups(): Promise<GroupInfo[]> {
    const ev = await this.request<EngineCommand & { kind: "list_groups" }>(
      (req) => ({ kind: "list_groups", req }),
    );
    if (ev.kind !== "list_groups_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as GroupInfo[];
  }

  async setGroupName(groupId: number, name: string): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_group_name" }>(
      (req) => ({ kind: "set_group_name", req, groupId, name }),
    );
    if (ev.kind !== "set_group_name_ok") throw new Error("unexpected event");
  }

  async removeGroup(groupId: number): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "remove_group" }>(
      (req) => ({ kind: "remove_group", req, groupId }),
    );
    if (ev.kind !== "remove_group_ok") throw new Error("unexpected event");
    return ev.removed;
  }

  async listPatterns(): Promise<PatternInfo[]> {
    const ev = await this.request<EngineCommand & { kind: "list_patterns" }>(
      (req) => ({ kind: "list_patterns", req }),
    );
    if (ev.kind !== "list_patterns_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as PatternInfo[];
  }

  async savePattern(name: string, gridJson: string): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "save_pattern" }>(
      (req) => ({ kind: "save_pattern", req, name, gridJson }),
    );
    if (ev.kind !== "save_pattern_ok") throw new Error("unexpected event");
  }

  /** Returns the saved gridJson for `name`, or null if no such pattern. */
  async loadPattern(name: string): Promise<string | null> {
    const ev = await this.request<EngineCommand & { kind: "load_pattern" }>(
      (req) => ({ kind: "load_pattern", req, name }),
    );
    if (ev.kind !== "load_pattern_ok") throw new Error("unexpected event");
    return ev.gridJson;
  }

  async removePattern(name: string): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "remove_pattern" }>(
      (req) => ({ kind: "remove_pattern", req, name }),
    );
    if (ev.kind !== "remove_pattern_ok") throw new Error("unexpected event");
    return ev.removed;
  }

  async mixdownWav(): Promise<Uint8Array> {
    const ev = await this.request<EngineCommand & { kind: "mixdown_wav" }>(
      (req) => ({ kind: "mixdown_wav", req }),
    );
    if (ev.kind !== "mixdown_wav_ok") throw new Error("unexpected event");
    return ev.bytes;
  }

  async exportKepz(): Promise<Uint8Array> {
    const ev = await this.request<EngineCommand & { kind: "export_kepz" }>(
      (req) => ({ kind: "export_kepz", req }),
    );
    if (ev.kind !== "export_kepz_ok") throw new Error("unexpected event");
    return ev.bytes;
  }

  async importKepz(bytes: Uint8Array): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "import_kepz" }>(
      (req) => ({ kind: "import_kepz", req, bytes }),
    );
    if (ev.kind !== "import_kepz_ok") throw new Error("unexpected event");
  }

  async detectPitchContour(
    sourceId: string,
    startFrame: number,
    endFrame: number,
    hopSamples: number,
    windowSamples: number,
  ): Promise<Float32Array> {
    const ev = await this.request<EngineCommand & { kind: "detect_pitch_contour" }>(
      (req) => ({
        kind: "detect_pitch_contour",
        req,
        sourceId,
        startFrame,
        endFrame,
        hopSamples,
        windowSamples,
      }),
    );
    if (ev.kind !== "detect_pitch_contour_ok") throw new Error("unexpected event");
    return ev.contour;
  }

  async detectBpm(sourceId: string): Promise<TempoEstimate> {
    const ev = await this.request<EngineCommand & { kind: "detect_bpm" }>(
      (req) => ({ kind: "detect_bpm", req, sourceId }),
    );
    if (ev.kind !== "detect_bpm_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as TempoEstimate;
  }

  async duplicateSource(sourceId: string): Promise<string> {
    const nowIso = new Date().toISOString();
    const ev = await this.request<EngineCommand & { kind: "duplicate_source" }>(
      (req) => ({ kind: "duplicate_source", req, sourceId, nowIso }),
    );
    if (ev.kind !== "duplicate_source_ok") throw new Error("unexpected event");
    return ev.newSourceId;
  }

  async createEmptySource(
    lengthFrames: number,
    channels: number,
    desiredName: string,
  ): Promise<string> {
    const nowIso = new Date().toISOString();
    const ev = await this.request<EngineCommand & { kind: "create_empty_source" }>(
      (req) => ({
        kind: "create_empty_source",
        req,
        lengthFrames,
        channels,
        desiredName,
        nowIso,
      }),
    );
    if (ev.kind !== "create_empty_source_ok") throw new Error("unexpected event");
    return ev.newSourceId;
  }

  async renameSource(sourceId: string, newName: string): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "rename_source" }>(
      (req) => ({ kind: "rename_source", req, sourceId, newName }),
    );
    if (ev.kind !== "rename_source_ok") throw new Error("unexpected event");
  }

  async removeSource(sourceId: string): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "remove_source" }>(
      (req) => ({ kind: "remove_source", req, sourceId }),
    );
    if (ev.kind !== "remove_source_ok") throw new Error("unexpected event");
    return ev.removed;
  }

  async setSourceFolder(sourceId: string, folder: string | null): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_source_folder" }>(
      (req) => ({ kind: "set_source_folder", req, sourceId, folder }),
    );
    if (ev.kind !== "set_source_folder_ok") throw new Error("unexpected event");
  }

  async renderRangeToSource(
    startFrame: number,
    endFrame: number,
    desiredName: string,
  ): Promise<string> {
    const nowIso = new Date().toISOString();
    const ev = await this.request<EngineCommand & { kind: "render_range_to_source" }>(
      (req) => ({
        kind: "render_range_to_source",
        req,
        startFrame,
        endFrame,
        desiredName,
        nowIso,
      }),
    );
    if (ev.kind !== "render_range_to_source_ok") throw new Error("unexpected event");
    return ev.newSourceId;
  }

  async captureNoiseProfile(
    sourceId: string,
    startFrame: number,
    endFrame: number,
    name: string,
    profileId: string,
    fftSize: number,
  ): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "capture_noise_profile" }>(
      (req) => ({
        kind: "capture_noise_profile",
        req,
        sourceId,
        startFrame,
        endFrame,
        name,
        profileId,
        fftSize,
      }),
    );
    if (ev.kind !== "capture_noise_profile_ok") throw new Error("unexpected event");
  }

  async setClipEnvelope(
    trackId: number,
    clipId: number,
    parameter: "volume" | "pan",
    breakpoints: Breakpoint[],
  ): Promise<void> {
    const ev = await this.request<EngineCommand & { kind: "set_clip_envelope" }>(
      (req) => ({
        kind: "set_clip_envelope",
        req,
        trackId,
        clipId,
        parameter,
        breakpointsJson: JSON.stringify(breakpoints),
      }),
    );
    if (ev.kind !== "set_clip_envelope_ok") throw new Error("unexpected event");
  }

  async listNoiseProfiles(): Promise<NoiseProfileInfo[]> {
    const ev = await this.request<EngineCommand & { kind: "list_noise_profiles" }>(
      (req) => ({ kind: "list_noise_profiles", req }),
    );
    if (ev.kind !== "list_noise_profiles_ok") throw new Error("unexpected event");
    return JSON.parse(ev.json) as NoiseProfileInfo[];
  }

  /** Pull raw interleaved samples for a source range. Used by the drum
   *  sequencer to populate per-pad AudioBuffers for low-latency live
   *  preview without round-tripping through mixdownWav. */
  async querySamples(
    sourceId: string,
    startFrame: number,
    endFrame: number,
  ): Promise<{ samples: Float32Array; channels: number }> {
    const ev = await this.request<EngineCommand & { kind: "query_samples" }>(
      (req) => ({ kind: "query_samples", req, sourceId, startFrame, endFrame }),
    );
    if (ev.kind !== "query_samples_ok") throw new Error("unexpected event");
    return { samples: ev.samples, channels: ev.channels };
  }

  /** Compute STFT magnitudes for a source range. Returns row-major
   *  `frameCount × binCount` linear magnitudes; the caller maps to dB and
   *  paints. */
  async spectrogramTile(
    sourceId: string,
    startFrame: number,
    endFrame: number,
    fftSize: number,
    hopSize: number,
  ): Promise<{ magnitudes: Float32Array; frameCount: number; binCount: number }> {
    const ev = await this.request<EngineCommand & { kind: "spectrogram_tile" }>(
      (req) => ({
        kind: "spectrogram_tile",
        req,
        sourceId,
        startFrame,
        endFrame,
        fftSize,
        hopSize,
      }),
    );
    if (ev.kind !== "spectrogram_tile_ok") throw new Error("unexpected event");
    return {
      magnitudes: ev.magnitudes,
      frameCount: ev.frameCount,
      binCount: ev.binCount,
    };
  }

  terminate(): void {
    this.worker.terminate();
    this.pending.clear();
  }

  private request<C extends EngineCommand>(build: (req: RequestId) => C): Promise<EngineEvent> {
    const req = this.nextReq++;
    const cmd = build(req);
    return new Promise<EngineEvent>((resolve, reject) => {
      this.pending.set(req, (ev) => {
        if (ev.kind === "error") reject(new Error(ev.reason));
        else resolve(ev);
      });
      this.worker.postMessage(cmd);
    });
  }

  private dispatch(msg: EngineEvent): void {
    if (msg.kind === "ready" || msg.kind === "fatal") return; // boot-time only
    const handler = this.pending.get(msg.req);
    if (!handler) {
      console.warn("engine: orphan reply", msg);
      return;
    }
    this.pending.delete(msg.req);
    handler(msg);
  }
}
