// Single-producer, single-consumer ring buffer over a SharedArrayBuffer.
// Producer is the engine Worker; consumer is the AudioWorklet. Indices count
// frames written / read since playback began (not modulo capacity); the index
// space is u32, so playback longer than 2^31 / sampleRate seconds (~12 hours
// at 48 kHz) would overflow — outside v1's concerns.
//
// Layout:
//   header: Int32Array, 4 entries
//     [0] writeFrame  — atomic, monotonic, advanced by producer
//     [1] readFrame   — atomic, monotonic, advanced by consumer
//     [2] producerEndFrame — atomic; 0 means "still producing", otherwise the
//         total frame count the producer will ever write. Lets the consumer
//         (and any observer on the main thread) detect end-of-stream.
//     [3] reserved
//   data: Float32Array, capacity * channels samples, interleaved.

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

const HEADER_BYTES = 16;
const HEADER_LEN = 4;
const WRITE_IDX = 0;
const READ_IDX = 1;
const END_IDX = 2;

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
