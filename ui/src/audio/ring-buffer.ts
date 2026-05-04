// Single-producer, single-consumer ring buffer over a SharedArrayBuffer.
// Producer is the engine Worker; consumer is the AudioWorklet. Indices count
// frames written / read since playback began (not modulo capacity); the index
// space is u32, so playback longer than 2^31 / sampleRate seconds (~12 hours
// at 48 kHz) would overflow — outside v1's concerns.
//
// Layout:
//   header: Int32Array, 8 entries
//     [0] writeFrame  — atomic, monotonic, advanced by producer
//     [1] readFrame   — atomic, monotonic, advanced by consumer
//     [2] producerEndFrame — atomic; 0 means "still producing", otherwise the
//         total frame count the producer will ever write. Lets the consumer
//         (and any observer on the main thread) detect end-of-stream.
//     [3] reserved
//     [4] loopStartFrame — source-output-frame; the producer reads this each
//         fill tick, the main thread writes it for live trim.
//     [5] loopEndFrame — source-output-frame; same. Loop region is half-open
//         [loopStart, loopEnd). The worker wraps (loop) or ends (non-loop)
//         when its next emit reaches loopEnd.
//     [6] workerNextSourceFrame — source-output-frame the producer will emit
//         next. The main thread reads it and subtracts the current buffer
//         fill (writeFrame - readFrame) to compute the consumer's playhead.
//     [7] reserved
//   data: Float32Array, capacity * channels samples, interleaved.
//
// IMPORTANT: the AudioWorklet processor only reads slots [0] and [1], but its
// data offset must match HEADER_BYTES below. Keep playback-worklet.js's
// constant in sync.

export interface RingBufferLayout {
  sab: SharedArrayBuffer;
  capacity: number;
  channels: number;
}

export interface RingBufferView {
  capacity: number;
  channels: number;
  header: Int32Array;
  data: Float32Array;
}

const HEADER_BYTES = 32;
const HEADER_LEN = 8;
const WRITE_IDX = 0;
const READ_IDX = 1;
const END_IDX = 2;
const LOOP_START_IDX = 4;
const LOOP_END_IDX = 5;
const WORKER_NEXT_SRC_IDX = 6;

export function createRingBuffer(capacity: number, channels: number): RingBufferLayout {
  const sab = new SharedArrayBuffer(HEADER_BYTES + capacity * channels * 4);
  return { sab, capacity, channels };
}

export function attachRingBuffer(layout: RingBufferLayout): RingBufferView {
  return {
    capacity: layout.capacity,
    channels: layout.channels,
    header: new Int32Array(layout.sab, 0, HEADER_LEN),
    data: new Float32Array(layout.sab, HEADER_BYTES, layout.capacity * layout.channels),
  };
}

export function resetRingBuffer(view: RingBufferView): void {
  Atomics.store(view.header, WRITE_IDX, 0);
  Atomics.store(view.header, READ_IDX, 0);
  Atomics.store(view.header, END_IDX, 0);
}

/** Producer: write up to `samples.length / channels` frames. Returns frames written. */
export function writeFrames(view: RingBufferView, samples: Float32Array): number {
  const framesIn = (samples.length / view.channels) | 0;
  const writeFrame = Atomics.load(view.header, WRITE_IDX);
  const readFrame = Atomics.load(view.header, READ_IDX);
  const free = view.capacity - (writeFrame - readFrame);
  const toWrite = Math.min(framesIn, free);
  if (toWrite <= 0) return 0;
  for (let i = 0; i < toWrite; i++) {
    const dst = ((writeFrame + i) % view.capacity) * view.channels;
    const src = i * view.channels;
    for (let c = 0; c < view.channels; c++) {
      view.data[dst + c] = samples[src + c];
    }
  }
  Atomics.store(view.header, WRITE_IDX, writeFrame + toWrite);
  return toWrite;
}

/** Consumer: read up to `frames` frames into the per-channel output arrays.
 *  `output` length must equal view.channels. Underruns are filled with zeros. */
export function readFramesInto(
  view: RingBufferView,
  output: Float32Array[],
  frames: number,
): { read: number; underrun: number } {
  const writeFrame = Atomics.load(view.header, WRITE_IDX);
  const readFrame = Atomics.load(view.header, READ_IDX);
  const available = writeFrame - readFrame;
  const toRead = Math.min(frames, available);
  for (let i = 0; i < toRead; i++) {
    const src = ((readFrame + i) % view.capacity) * view.channels;
    for (let c = 0; c < output.length; c++) {
      output[c][i] = view.data[src + c];
    }
  }
  for (let i = toRead; i < frames; i++) {
    for (let c = 0; c < output.length; c++) {
      output[c][i] = 0;
    }
  }
  if (toRead > 0) {
    Atomics.store(view.header, READ_IDX, readFrame + toRead);
  }
  return { read: toRead, underrun: frames - toRead };
}

export function freeFrames(view: RingBufferView): number {
  const writeFrame = Atomics.load(view.header, WRITE_IDX);
  const readFrame = Atomics.load(view.header, READ_IDX);
  return view.capacity - (writeFrame - readFrame);
}

export function readFrame(view: RingBufferView): number {
  return Atomics.load(view.header, READ_IDX);
}

export function writeFrame(view: RingBufferView): number {
  return Atomics.load(view.header, WRITE_IDX);
}

export function setProducerEnd(view: RingBufferView, totalFrames: number): void {
  Atomics.store(view.header, END_IDX, totalFrames);
}

export function producerEnd(view: RingBufferView): number {
  return Atomics.load(view.header, END_IDX);
}

export function setLoopRange(
  view: RingBufferView,
  loopStartFrame: number,
  loopEndFrame: number,
): void {
  Atomics.store(view.header, LOOP_START_IDX, Math.max(0, Math.floor(loopStartFrame)));
  Atomics.store(view.header, LOOP_END_IDX, Math.max(1, Math.floor(loopEndFrame)));
}

export function loopStart(view: RingBufferView): number {
  return Atomics.load(view.header, LOOP_START_IDX);
}

export function loopEnd(view: RingBufferView): number {
  return Atomics.load(view.header, LOOP_END_IDX);
}

export function setWorkerNextSourceFrame(view: RingBufferView, frame: number): void {
  Atomics.store(view.header, WORKER_NEXT_SRC_IDX, Math.max(0, Math.floor(frame)));
}

export function workerNextSourceFrame(view: RingBufferView): number {
  return Atomics.load(view.header, WORKER_NEXT_SRC_IDX);
}
