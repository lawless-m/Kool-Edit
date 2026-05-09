// Canvas2D spectrogram painter. Inputs are linear STFT magnitudes laid out
// row-major as `frameCount × binCount` (positive bins only). The painter
// converts to dB, applies a perceptual ramp, and writes pixel data via
// `putImageData` so a fresh draw is one upload.
//
// Architecture doc §"Spectrogram renderer" describes a tile-based WebGL2
// path for the longer-term renderer; this module is the simpler v0 used by
// the destructive editor's spectral view.

interface DrawOptions {
  /** dB threshold mapped to the darkest colour. */
  dbFloor?: number;
  /** dB threshold mapped to the brightest colour. */
  dbCeiling?: number;
}

export function drawSpectrogram(
  canvas: HTMLCanvasElement,
  magnitudes: Float32Array,
  frameCount: number,
  binCount: number,
  options: DrawOptions = {},
): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  if (w === 0 || h === 0 || frameCount === 0 || binCount === 0) {
    ctx.clearRect(0, 0, w, h);
    return;
  }
  const dbFloor = options.dbFloor ?? -100;
  const dbCeiling = options.dbCeiling ?? 0;
  const dbSpan = Math.max(1e-3, dbCeiling - dbFloor);

  const img = ctx.createImageData(w, h);

  // Pre-compute the magnitude→colour-index lookup column-wise so adjacent
  // pixels in a column reuse the same frame slice.
  for (let x = 0; x < w; x++) {
    const fIdx = Math.min(frameCount - 1, Math.floor((x / w) * frameCount));
    const frameOffset = fIdx * binCount;
    for (let y = 0; y < h; y++) {
      // y=0 is the top of the canvas; we want high frequency at the top, so
      // bin index runs in the opposite direction.
      const binIdx = Math.min(
        binCount - 1,
        Math.floor(((h - 1 - y) / h) * binCount),
      );
      const mag = magnitudes[frameOffset + binIdx];
      const db = mag > 1e-12 ? 20 * Math.log10(mag) : dbFloor;
      const t = Math.max(0, Math.min(1, (db - dbFloor) / dbSpan));
      const off = (y * w + x) * 4;
      const [r, g, b] = sampleRamp(t);
      img.data[off] = r;
      img.data[off + 1] = g;
      img.data[off + 2] = b;
      img.data[off + 3] = 255;
    }
  }
  ctx.putImageData(img, 0, 0);
}

// Five-stop inferno-style ramp: black → purple → red → orange → near-white.
// Values are eyeballed not fit-to-published-data, but close enough that
// dynamic range reads naturally on a black UI.
const RAMP: Array<readonly [number, readonly [number, number, number]]> = [
  [0.0, [0, 0, 4]],
  [0.25, [50, 10, 90]],
  [0.5, [180, 50, 60]],
  [0.75, [240, 130, 40]],
  [1.0, [255, 255, 200]],
];

function sampleRamp(t: number): [number, number, number] {
  for (let i = 1; i < RAMP.length; i++) {
    const [pa, ca] = RAMP[i - 1];
    const [pb, cb] = RAMP[i];
    if (t <= pb) {
      const f = (t - pa) / (pb - pa);
      return [
        Math.round(ca[0] + f * (cb[0] - ca[0])),
        Math.round(ca[1] + f * (cb[1] - ca[1])),
        Math.round(ca[2] + f * (cb[2] - ca[2])),
      ];
    }
  }
  const last = RAMP[RAMP.length - 1][1];
  return [last[0], last[1], last[2]];
}
