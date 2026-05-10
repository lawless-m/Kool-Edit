/**
 * Draw a flat min/max peak buffer (alternating min,max,min,max,...) into a
 * canvas. One vertical bar per pair, in the spec's near-black-on-green palette.
 *
 * If `channels` > 1, `peaks` is expected channel-major: all of channel 0's
 * `[min, max, min, max, ...]` first, then channel 1's, etc. Each channel
 * gets its own horizontal lane stacked top-to-bottom. Mono falls back to
 * the original full-height single-lane render.
 */
export function drawWaveform(
  canvas: HTMLCanvasElement,
  peaks: Float32Array,
  channels = 1,
): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;

  const width = canvas.width;
  const height = canvas.height;
  const ch = Math.max(1, channels);
  // Each channel gets an equal horizontal slice; a 1-pixel divider
  // separates them so L/R don't visually bleed.
  const laneHeight = ch === 1 ? height : Math.floor((height - (ch - 1)) / ch);
  const columnsPerChannel = peaks.length / (ch * 2);

  ctx.fillStyle = "#0c0c0c";
  ctx.fillRect(0, 0, width, height);

  for (let c = 0; c < ch; c++) {
    const laneTop = c * (laneHeight + 1);
    const mid = laneTop + laneHeight / 2;
    // Centre line.
    ctx.fillStyle = "#1f2a1f";
    ctx.fillRect(0, mid, width, 1);
    // Channel divider (between this lane and the next).
    if (c < ch - 1) {
      ctx.fillStyle = "#222";
      ctx.fillRect(0, laneTop + laneHeight, width, 1);
    }
    ctx.fillStyle = "#7cd17c";
    const channelOffset = c * columnsPerChannel * 2;
    for (let col = 0; col < columnsPerChannel; col++) {
      const min = peaks[channelOffset + col * 2];
      const max = peaks[channelOffset + col * 2 + 1];
      const x = (col / columnsPerChannel) * width;
      const w = Math.max(1, width / columnsPerChannel);
      const halfH = laneHeight / 2;
      const yMax = mid - max * halfH;
      const yMin = mid - min * halfH;
      const h = Math.max(1, yMin - yMax);
      ctx.fillRect(x, yMax, w, h);
    }
  }
}
