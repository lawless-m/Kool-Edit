/**
 * Draw a flat min/max peak buffer (alternating min,max,min,max,...) into a
 * canvas. One vertical bar per pair, in the spec's near-black-on-green palette.
 */
export function drawWaveform(canvas: HTMLCanvasElement, peaks: Float32Array): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const width = canvas.width;
  const height = canvas.height;
  const mid = height / 2;
  const columns = peaks.length / 2;

  ctx.fillStyle = "#0c0c0c";
  ctx.fillRect(0, 0, width, height);

  // Centre line.
  ctx.fillStyle = "#1f2a1f";
  ctx.fillRect(0, mid, width, 1);

  ctx.fillStyle = "#7cd17c";
  for (let col = 0; col < columns; col++) {
    const min = peaks[col * 2];
    const max = peaks[col * 2 + 1];
    const x = (col / columns) * width;
    const w = Math.max(1, width / columns);
    const yMax = mid - max * mid;
    const yMin = mid - min * mid;
    const h = Math.max(1, yMin - yMax);
    ctx.fillRect(x, yMax, w, h);
  }
}
