// Main-thread playback controller. Owns the AudioContext, the
// AudioWorkletNode, and the SAB ring buffer; commands the engine Worker to
// fill it. Each `play()` call rebuilds a fresh ring buffer from scratch.

import {
  attachRingBuffer,
  createRingBuffer,
  producerEnd,
  readFrame as ringReadFrame,
  type RingBufferView,
} from "./ring-buffer";
import type { EngineClient } from "../engine/client";

const RING_CAPACITY_FRAMES = 9600; // ~200 ms at 48 kHz
const OUTPUT_CHANNELS = 2;

export class Playback {
  private client: EngineClient;
  private audioCtx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private ringView: RingBufferView | null = null;
  private endWatcher: ReturnType<typeof setInterval> | null = null;
  private playing = false;
  private paused = false;
  private loop = false;
  private sourceLength = 0; // total source length in output frames
  private sessionStartFrame = 0; // source-output-frame this session began at
  private loopStartFrame = 0; // active loop region in (source-output-frames)
  private loopEndFrame = 0; // active loop region out (source-output-frames)

  constructor(client: EngineClient) {
    this.client = client;
  }

  isPlaying(): boolean {
    return this.playing;
  }

  isPaused(): boolean {
    return this.paused;
  }

  isLooping(): boolean {
    return this.playing && this.loop;
  }

  async pause(): Promise<void> {
    if (!this.playing || this.paused || !this.audioCtx) return;
    await this.audioCtx.suspend();
    this.paused = true;
  }

  async resume(): Promise<void> {
    if (!this.playing || !this.paused || !this.audioCtx) return;
    await this.audioCtx.resume();
    this.paused = false;
  }

  /** Current source-output-frame the consumer is reading, plus the source
   *  length. Null when no session is active. */
  position(): { sourceFrame: number; sourceLength: number } | null {
    if (!this.ringView || this.sourceLength <= 0) return null;
    const readF = ringReadFrame(this.ringView);
    const firstIter = this.loopEndFrame - this.sessionStartFrame;
    const loopLen = this.loopEndFrame - this.loopStartFrame;
    const sourceFrame =
      readF < firstIter || loopLen <= 0
        ? this.sessionStartFrame + readF
        : this.loopStartFrame + ((readF - firstIter) % loopLen);
    return { sourceFrame, sourceLength: this.sourceLength };
  }

  /** Output sample rate of the active AudioContext, or null if not yet
   *  created. The selection UI uses this to convert source-frames to
   *  source-output-frames at play time. */
  outputSampleRate(): number | null {
    return this.audioCtx?.sampleRate ?? null;
  }

  setLoop(b: boolean): void {
    this.loop = b;
  }

  /** Play `sourceId`. All frame arguments are in **source-native frames**;
   *  Playback converts to source-output-frames internally using the
   *  AudioContext's sample rate. `loopEndSourceFrame === 0` means "whole
   *  source". Resolves once playback has started; the promise does NOT wait
   *  for end-of-stream — use `onEnded` for that. */
  async play(
    sourceId: string,
    onEnded: () => void,
    opts: {
      startSourceFrame?: number;
      loopStartSourceFrame?: number;
      loopEndSourceFrame?: number;
      sourceSampleRate: number;
    },
  ): Promise<void> {
    await this.stop();

    if (!this.audioCtx) {
      this.audioCtx = new AudioContext();
      // Worklet lives in ui/public/ so Vite serves it verbatim — referencing
      // it via `new URL(..., import.meta.url)` from src/ doesn't trigger
      // Vite's asset emit, so we'd lose it in the production bundle.
      await this.audioCtx.audioWorklet.addModule("/playback-worklet.js");
    }
    if (this.audioCtx.state === "suspended") {
      await this.audioCtx.resume();
    }

    const ring = createRingBuffer(RING_CAPACITY_FRAMES, OUTPUT_CHANNELS);
    const view = attachRingBuffer(ring);

    const node = new AudioWorkletNode(this.audioCtx, "kool-edit-playback", {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [OUTPUT_CHANNELS],
    });
    node.port.postMessage({
      kind: "attach",
      sab: ring.sab,
      capacity: ring.capacity,
      channels: ring.channels,
    });
    node.connect(this.audioCtx.destination);

    this.ringView = view;
    this.node = node;

    // Convert source-native frames → source-output-frames using the active
    // AudioContext's sample rate. The worker speaks output-frames everywhere
    // (so that the ring counters match the worklet's playback rate).
    const ratio = this.audioCtx.sampleRate / opts.sourceSampleRate;
    const startOut = Math.max(0, Math.floor((opts.startSourceFrame ?? 0) * ratio));
    const loopStartOut = Math.max(
      0,
      Math.floor((opts.loopStartSourceFrame ?? 0) * ratio),
    );
    // 0 means "whole source"; pass a large sentinel and let the worker clamp
    // to its computed sourceLength.
    const loopEndSrc = opts.loopEndSourceFrame ?? 0;
    const loopEndOut =
      loopEndSrc > 0 ? Math.ceil(loopEndSrc * ratio) : Number.MAX_SAFE_INTEGER;

    const { totalOutputFrames } = await this.client.startPlayback(
      sourceId,
      this.audioCtx.sampleRate,
      OUTPUT_CHANNELS,
      this.loop,
      startOut,
      loopStartOut,
      loopEndOut,
      ring,
    );
    this.sourceLength = totalOutputFrames;
    this.loopStartFrame = Math.min(loopStartOut, Math.max(0, totalOutputFrames));
    this.loopEndFrame = Math.min(loopEndOut, totalOutputFrames);
    this.sessionStartFrame = Math.min(
      Math.max(this.loopStartFrame, startOut),
      Math.max(this.loopStartFrame, this.loopEndFrame - 1),
    );

    this.playing = true;
    this.endWatcher = setInterval(() => {
      if (!this.ringView) return;
      // Worker sets producerEnd to firstIter for non-loop sessions. Loop
      // sessions leave it at 0 so this never fires.
      const total = producerEnd(this.ringView);
      if (total > 0 && ringReadFrame(this.ringView) >= total) {
        this.stop().then(onEnded);
      }
    }, 50);
  }

  async stop(): Promise<void> {
    if (this.endWatcher !== null) {
      clearInterval(this.endWatcher);
      this.endWatcher = null;
    }
    if (this.playing) {
      try {
        await this.client.stopPlayback();
      } catch {
        // Worker may already have ended; ignore.
      }
    }
    if (this.node) {
      this.node.port.postMessage({ kind: "detach" });
      this.node.disconnect();
      this.node = null;
    }
    this.ringView = null;
    this.playing = false;
    this.paused = false;
    this.sourceLength = 0;
    this.sessionStartFrame = 0;
    this.loopStartFrame = 0;
    this.loopEndFrame = 0;
  }
}
