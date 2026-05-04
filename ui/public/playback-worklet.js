// Playback worklet. Drains a SharedArrayBuffer ring buffer filled by the
// engine Worker. Underruns (producer can't keep up) emit silence.
//
// Ring layout must match ring-buffer.ts:
//   header Int32Array length 4: [writeFrame, readFrame, producerEnd, _]
//   data Float32Array, capacity * channels samples, interleaved.

const HEADER_BYTES = 16;
const WRITE_IDX = 0;
const READ_IDX = 1;

class PlaybackProcessor extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ring = null;
    this.underruns = 0;
    this.port.onmessage = (ev) => {
      const msg = ev.data;
      if (msg.kind === "attach") {
        const { sab, capacity, channels } = msg;
        this.ring = {
          capacity,
          channels,
          header: new Int32Array(sab, 0, 4),
          data: new Float32Array(sab, HEADER_BYTES, capacity * channels),
        };
        this.underruns = 0;
      } else if (msg.kind === "detach") {
        this.ring = null;
      }
    };
  }

  process(_inputs, outputs) {
    const out = outputs[0];
    if (!out || out.length === 0) return true;
    const frames = out[0].length;

    if (!this.ring) {
      for (let c = 0; c < out.length; c++) out[c].fill(0);
      return true;
    }

    const r = this.ring;
    const writeFrame = Atomics.load(r.header, WRITE_IDX);
    const readFrame = Atomics.load(r.header, READ_IDX);
    const available = writeFrame - readFrame;
    const toRead = Math.min(frames, available);

    for (let i = 0; i < toRead; i++) {
      const src = ((readFrame + i) % r.capacity) * r.channels;
      for (let c = 0; c < out.length; c++) {
        // If the output has more channels than the ring (or vice versa),
        // map within the smaller dimension and zero the rest.
        out[c][i] = c < r.channels ? r.data[src + c] : 0;
      }
    }
    for (let i = toRead; i < frames; i++) {
      for (let c = 0; c < out.length; c++) out[c][i] = 0;
    }
    if (toRead > 0) {
      Atomics.store(r.header, READ_IDX, readFrame + toRead);
    } else {
      this.underruns++;
    }

    return true;
  }
}

registerProcessor("kool-edit-playback", PlaybackProcessor);
