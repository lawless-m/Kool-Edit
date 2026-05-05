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

  async removeClip(trackId: number, clipId: number): Promise<boolean> {
    const ev = await this.request<EngineCommand & { kind: "remove_clip" }>(
      (req) => ({ kind: "remove_clip", req, trackId, clipId }),
    );
    if (ev.kind !== "remove_clip_ok") throw new Error("unexpected event");
    return ev.removed;
  }

  async mixdownWav(): Promise<Uint8Array> {
    const ev = await this.request<EngineCommand & { kind: "mixdown_wav" }>(
      (req) => ({ kind: "mixdown_wav", req }),
    );
    if (ev.kind !== "mixdown_wav_ok") throw new Error("unexpected event");
    return ev.bytes;
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
