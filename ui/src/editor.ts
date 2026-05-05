// Single-source waveform editor tab. Owns one `currentSourceId` plus its
// selection, viewport, and gain preview state. The shell (main.ts) boots the
// shared EngineClient and Playback and hands them in.

import type { EngineClient } from "./engine/client";
import type { Playback } from "./audio/playback";
import { drawWaveform } from "./waveform";

interface Selection {
  inFrame: number; // source-native frames
  outFrame: number;
}

interface Viewport {
  startFrame: number; // source-native frames, inclusive
  endFrame: number; // source-native frames, exclusive
}

const TRIM_NUDGE_MS = 10;
const MIN_VIEWPORT_FRAMES = 64; // peak cache decimation; finer than this is wasted

export interface EditorOptions {
  /** Called after a fresh import lands so the arranger can refresh its source list. */
  onSourceImported?: () => void;
}

export interface EditorHandle {
  /** Drop the editor's current source and selection. Used after loading a
   *  fresh project at the shell level so the editor doesn't keep pointing
   *  at an id that no longer exists. */
  reset: () => Promise<void>;
}

export async function mountEditor(
  root: HTMLElement,
  client: EngineClient,
  playback: Playback,
  opts: EditorOptions = {},
): Promise<EditorHandle> {
  const ui = buildUi(root);

  let currentSourceId: string | null = null;
  let sourceFrameCount = 0;
  let sourceSampleRate = 0;
  let selection: Selection | null = null;
  let viewport: Viewport = { startFrame: 0, endFrame: 0 };
  let playheadRaf: number | null = null;
  let pendingPeakRequest = 0; // race guard for in-flight peakSummary calls
  let cachedPeaks: Float32Array | null = null;

  // ---- selection helpers ------------------------------------------------

  const formatSeconds = (seconds: number): string => {
    if (!Number.isFinite(seconds)) return "0.000";
    return seconds.toFixed(3);
  };

  const framesToSeconds = (frames: number): number =>
    sourceSampleRate > 0 ? frames / sourceSampleRate : 0;

  const secondsToFrames = (seconds: number): number =>
    sourceSampleRate > 0 ? Math.round(seconds * sourceSampleRate) : 0;

  const clampFrame = (f: number): number =>
    Math.max(0, Math.min(sourceFrameCount, Math.round(f)));

  const setSelection = (s: Selection | null): void => {
    if (s) {
      const lo = Math.min(s.inFrame, s.outFrame);
      const hi = Math.max(s.inFrame, s.outFrame);
      selection = { inFrame: clampFrame(lo), outFrame: clampFrame(hi) };
      if (selection.outFrame === selection.inFrame) {
        selection = null;
      }
    } else {
      selection = null;
    }
    syncSelectionInputs();
    syncZoomButtons();
    redrawOverlay();
    drawCachedWaveform();
  };

  const syncSelectionInputs = (): void => {
    if (selection) {
      ui.inInput.value = formatSeconds(framesToSeconds(selection.inFrame));
      ui.outInput.value = formatSeconds(framesToSeconds(selection.outFrame));
    } else {
      ui.inInput.value = "";
      ui.outInput.value = "";
    }
    const trimEnabled = selection !== null;
    for (const b of [ui.inMinus, ui.inPlus, ui.outMinus, ui.outPlus]) {
      b.disabled = !trimEnabled;
    }
    ui.inInput.disabled = sourceFrameCount === 0;
    ui.outInput.disabled = sourceFrameCount === 0;
    ui.gainSlider.disabled = !trimEnabled;
    ui.gainApplyBtn.disabled = !trimEnabled;
  };

  const trim = (which: "in" | "out", deltaMs: number): void => {
    if (!selection) return;
    const deltaFrames = Math.round((deltaMs / 1000) * sourceSampleRate);
    const next: Selection =
      which === "in"
        ? { inFrame: selection.inFrame + deltaFrames, outFrame: selection.outFrame }
        : { inFrame: selection.inFrame, outFrame: selection.outFrame + deltaFrames };
    if (which === "in" && next.inFrame >= next.outFrame) return;
    if (which === "out" && next.outFrame <= next.inFrame) return;
    setSelection(next);
  };

  // ---- viewport / zoom --------------------------------------------------

  const viewportLength = (): number =>
    Math.max(1, viewport.endFrame - viewport.startFrame);

  const setViewport = (start: number, end: number): void => {
    if (sourceFrameCount === 0) {
      viewport = { startFrame: 0, endFrame: 0 };
      return;
    }
    let s = Math.max(0, Math.floor(start));
    let e = Math.min(sourceFrameCount, Math.ceil(end));
    if (e - s < MIN_VIEWPORT_FRAMES) {
      const mid = Math.floor((s + e) / 2);
      s = Math.max(0, mid - Math.floor(MIN_VIEWPORT_FRAMES / 2));
      e = Math.min(sourceFrameCount, s + MIN_VIEWPORT_FRAMES);
    }
    if (e <= s) {
      s = 0;
      e = sourceFrameCount;
    }
    viewport = { startFrame: s, endFrame: e };
    syncScrollbar();
    redrawOverlay();
    void redrawWaveform();
    syncZoomButtons();
  };

  const zoomBy = (factor: number, pivotFrame: number): void => {
    const len = viewportLength();
    const newLen = Math.max(MIN_VIEWPORT_FRAMES, Math.min(sourceFrameCount, len * factor));
    if (Math.abs(newLen - len) < 1) return;
    const fracAtPivot = (pivotFrame - viewport.startFrame) / len;
    const newStart = pivotFrame - fracAtPivot * newLen;
    setViewport(newStart, newStart + newLen);
  };

  /** Zoom by `factor` and centre the pivot in the viewport. Used by the
   *  zoom buttons so the selection's in-point (or playhead) stays under the
   *  user's eye. */
  const zoomCenterOn = (factor: number, pivotFrame: number): void => {
    const len = viewportLength();
    const newLen = Math.max(MIN_VIEWPORT_FRAMES, Math.min(sourceFrameCount, len * factor));
    if (Math.abs(newLen - len) < 1) return;
    const newStart = pivotFrame - newLen / 2;
    setViewport(newStart, newStart + newLen);
  };

  const zoomFull = (): void => setViewport(0, sourceFrameCount);
  const zoomToSelection = (): void => {
    if (!selection) return;
    setViewport(selection.inFrame, selection.outFrame);
  };

  const syncZoomButtons = (): void => {
    const enabled = sourceFrameCount > 0;
    ui.zoomInBtn.disabled = !enabled || viewportLength() <= MIN_VIEWPORT_FRAMES;
    ui.zoomOutBtn.disabled = !enabled || viewportLength() >= sourceFrameCount;
    ui.zoomFullBtn.disabled = !enabled || viewportLength() >= sourceFrameCount;
    ui.zoomSelBtn.disabled = !enabled || !selection;
  };

  const syncScrollbar = (): void => {
    const len = viewportLength();
    const max = Math.max(0, sourceFrameCount - len);
    ui.scrollbar.disabled = sourceFrameCount === 0 || max === 0;
    ui.scrollbar.min = "0";
    ui.scrollbar.max = String(max);
    ui.scrollbar.step = String(Math.max(1, Math.floor(len / 100)));
    ui.scrollbar.value = String(viewport.startFrame);
  };

  const redrawWaveform = async (): Promise<void> => {
    if (!currentSourceId || sourceFrameCount === 0) return;
    const reqId = ++pendingPeakRequest;
    const peaks = await client.peakSummary(
      currentSourceId,
      ui.canvas.width,
      viewport.startFrame,
      viewport.endFrame,
    );
    if (reqId !== pendingPeakRequest) return;
    cachedPeaks = peaks;
    drawCachedWaveform();
  };

  /** Redraw the waveform with the slider's gain preview overlaid on the
   *  selection columns. Multiplicative on `cachedPeaks` — what you see is
   *  what an Apply at the current slider value would give you. */
  const drawCachedWaveform = (): void => {
    if (!cachedPeaks) return;
    const percent = parseFloat(ui.gainSlider.value);
    const gainLin = Number.isFinite(percent) ? percent / 100 : 1;
    if (gainLin === 1 || !selection || sourceFrameCount === 0) {
      drawWaveform(ui.canvas, cachedPeaks);
      return;
    }
    const w = ui.canvas.width;
    const vLen = Math.max(1, viewport.endFrame - viewport.startFrame);
    const colFromFrame = (f: number): number =>
      Math.max(0, Math.min(w, Math.round(((f - viewport.startFrame) / vLen) * w)));
    const colIn = colFromFrame(selection.inFrame);
    const colOut = colFromFrame(selection.outFrame);
    if (colOut <= colIn) {
      drawWaveform(ui.canvas, cachedPeaks);
      return;
    }
    const previewed = cachedPeaks.slice();
    for (let col = colIn; col < colOut; col++) {
      const i = col * 2;
      previewed[i] = Math.max(-1, Math.min(1, cachedPeaks[i] * gainLin));
      previewed[i + 1] = Math.max(-1, Math.min(1, cachedPeaks[i + 1] * gainLin));
    }
    drawWaveform(ui.canvas, previewed);
  };

  // ---- canvas drawing ---------------------------------------------------

  const redrawOverlay = (): void => {
    const ctx = ui.playhead.getContext("2d");
    if (!ctx) return;
    const w = ui.playhead.width;
    const h = ui.playhead.height;
    ctx.clearRect(0, 0, w, h);
    if (sourceFrameCount === 0) return;
    const vLen = viewportLength();

    if (selection) {
      const lo = Math.max(selection.inFrame, viewport.startFrame);
      const hi = Math.min(selection.outFrame, viewport.endFrame);
      if (hi > lo) {
        const x0 = ((lo - viewport.startFrame) / vLen) * w;
        const x1 = ((hi - viewport.startFrame) / vLen) * w;
        ctx.fillStyle = "rgba(124, 209, 124, 0.18)";
        ctx.fillRect(x0, 0, Math.max(1, x1 - x0), h);
        ctx.fillStyle = "#7cd17c";
        if (selection.inFrame >= viewport.startFrame && selection.inFrame <= viewport.endFrame) {
          ctx.fillRect(Math.floor(x0), 0, 1, h);
        }
        if (selection.outFrame >= viewport.startFrame && selection.outFrame <= viewport.endFrame) {
          ctx.fillRect(Math.max(0, Math.floor(x1) - 1), 0, 1, h);
        }
      }
    }

    const pos = playback.position();
    if (pos && pos.sourceLength > 0) {
      const sourceFrame = (pos.sourceFrame / pos.sourceLength) * sourceFrameCount;
      if (sourceFrame >= viewport.startFrame && sourceFrame <= viewport.endFrame) {
        const x = ((sourceFrame - viewport.startFrame) / vLen) * w;
        ctx.fillStyle = "#e6e6e6";
        ctx.fillRect(Math.floor(Math.min(w - 1, x)), 0, 1, h);
      }
    }
  };

  const startPlayheadLoop = (): void => {
    const tick = (): void => {
      redrawOverlay();
      if (playback.isPlaying()) {
        playheadRaf = requestAnimationFrame(tick);
      } else {
        playheadRaf = null;
      }
    };
    if (playheadRaf === null) playheadRaf = requestAnimationFrame(tick);
  };

  const haltPlayheadLoop = (): void => {
    if (playheadRaf !== null) cancelAnimationFrame(playheadRaf);
    playheadRaf = null;
  };

  const stopPlayheadLoop = (): void => {
    haltPlayheadLoop();
    redrawOverlay();
  };

  // ---- transport helpers ------------------------------------------------

  const refreshTransport = (canPlay: boolean): void => {
    const active = playback.isPlaying();
    const looping = playback.isLooping();
    const playing = active && !playback.isPaused();
    ui.playBtn.disabled = !canPlay;
    ui.loopBtn.disabled = !canPlay;
    ui.playBtn.textContent = active && !looping && playing ? "Pause" : "Play";
    ui.loopBtn.textContent = active && looping && playing ? "Pause" : "Loop";
    ui.stopBtn.disabled = !active;
  };
  const setTransportEnabled = (canPlay: boolean): void => refreshTransport(canPlay);

  const togglePauseResume = async (): Promise<void> => {
    if (playback.isPaused()) {
      await playback.resume();
      ui.status.textContent = "playing…";
      startPlayheadLoop();
    } else {
      await playback.pause();
      ui.status.textContent = "paused";
      haltPlayheadLoop();
    }
    refreshTransport(currentSourceId !== null);
  };

  const startPlay = async (loop: boolean, fromFrame: number): Promise<void> => {
    if (!currentSourceId) return;
    playback.setLoop(loop);
    setTransportEnabled(false);
    try {
      await playback.play(
        currentSourceId,
        () => {
          ui.status.textContent = "playback ended";
          stopPlayheadLoop();
          refreshTransport(currentSourceId !== null);
        },
        {
          startSourceFrame: fromFrame,
          loopStartSourceFrame: selection ? selection.inFrame : 0,
          loopEndSourceFrame: selection ? selection.outFrame : 0,
          sourceSampleRate,
        },
      );
      ui.status.textContent = loop ? "looping…" : "playing…";
      refreshTransport(currentSourceId !== null);
      startPlayheadLoop();
    } catch (err) {
      ui.status.textContent = `playback failed: ${String(err)}`;
      refreshTransport(currentSourceId !== null);
    }
  };

  const idleStartFrameSrc = (): number => (selection ? selection.inFrame : 0);

  const commitSelectionToPlayback = (): void => {
    if (!selection) return;
    if (!playback.isPlaying()) return;
    playback.updateLoopRange(selection.inFrame, selection.outFrame, sourceSampleRate);
  };

  const currentSourceFrame = (): number => {
    const pos = playback.position();
    const outSr = playback.outputSampleRate();
    if (!pos || !outSr || sourceSampleRate === 0) return idleStartFrameSrc();
    return Math.round((pos.sourceFrame * sourceSampleRate) / outSr);
  };

  // ---- canvas drag interaction -----------------------------------------

  const clientXToFrame = (clientX: number): number => {
    const rect = ui.playhead.getBoundingClientRect();
    const fraction = (clientX - rect.left) / rect.width;
    return clampFrame(viewport.startFrame + fraction * viewportLength());
  };

  let dragAnchor: number | null = null;
  let panAnchor: { clientX: number; startFrame: number } | null = null;

  ui.playhead.addEventListener("mousedown", (ev) => {
    if (sourceFrameCount === 0) return;
    if (ev.button === 0) {
      ev.preventDefault();
      const frame = clientXToFrame(ev.clientX);
      dragAnchor = frame;
      setSelection({ inFrame: frame, outFrame: frame });
    } else if (ev.button === 1) {
      ev.preventDefault();
      panAnchor = { clientX: ev.clientX, startFrame: viewport.startFrame };
      ui.playhead.style.cursor = "grabbing";
    }
  });
  window.addEventListener("mousemove", (ev) => {
    if (panAnchor !== null) {
      const rect = ui.playhead.getBoundingClientRect();
      const deltaPx = ev.clientX - panAnchor.clientX;
      const len = viewportLength();
      const deltaFrames = -(deltaPx / rect.width) * len;
      const newStart = Math.max(
        0,
        Math.min(sourceFrameCount - len, Math.round(panAnchor.startFrame + deltaFrames)),
      );
      setViewport(newStart, newStart + len);
      return;
    }
    if (dragAnchor === null) return;
    const frame = clientXToFrame(ev.clientX);
    setSelection({ inFrame: dragAnchor, outFrame: frame });
  });
  window.addEventListener("mouseup", (ev) => {
    if (panAnchor !== null && (ev.button === 1 || ev.button === undefined)) {
      panAnchor = null;
      ui.playhead.style.cursor = "crosshair";
      return;
    }
    const wasDragging = dragAnchor !== null;
    dragAnchor = null;
    if (!wasDragging) return;
    commitSelectionToPlayback();
  });
  ui.playhead.addEventListener("auxclick", (ev) => {
    if (ev.button === 1) ev.preventDefault();
  });

  // ---- file load -------------------------------------------------------

  ui.fileInput.addEventListener("change", async () => {
    const file = ui.fileInput.files?.[0];
    if (!file) return;
    await playback.stop();
    stopPlayheadLoop();
    currentSourceId = null;
    sourceFrameCount = 0;
    sourceSampleRate = 0;
    viewport = { startFrame: 0, endFrame: 0 };
    cachedPeaks = null;
    ui.gainSlider.value = "100";
    ui.gainLabel.textContent = "100%";
    setSelection(null);
    setTransportEnabled(false);
    ui.status.textContent = `decoding ${file.name}…`;
    try {
      const buf = await file.arrayBuffer();
      const imp = await client.importWav(file.name, new Uint8Array(buf));
      currentSourceId = imp.sourceId;
      sourceFrameCount = imp.frames;
      sourceSampleRate = imp.sampleRate;
      viewport = { startFrame: 0, endFrame: imp.frames };
      ui.status.textContent = `${file.name} · ${imp.sourceId} · ${imp.frames.toLocaleString()} frames @ ${imp.sampleRate} Hz`;
      syncSelectionInputs();
      syncScrollbar();
      syncZoomButtons();
      setTransportEnabled(true);
      ui.undoBtn.disabled = false;
      ui.redoBtn.disabled = false;
      await redrawWaveform();
      redrawOverlay();
      opts.onSourceImported?.();
    } catch (err) {
      ui.status.textContent = `import failed: ${String(err)}`;
    }
  });

  // ---- transport buttons -----------------------------------------------

  ui.playBtn.addEventListener("click", async () => {
    if (playback.isPlaying()) {
      if (!playback.isLooping()) {
        await togglePauseResume();
        return;
      }
      await startPlay(false, currentSourceFrame());
      return;
    }
    await startPlay(false, idleStartFrameSrc());
  });

  ui.loopBtn.addEventListener("click", async () => {
    if (playback.isPlaying()) {
      if (playback.isLooping()) {
        await togglePauseResume();
        return;
      }
      await startPlay(true, currentSourceFrame());
      return;
    }
    await startPlay(true, idleStartFrameSrc());
  });

  ui.stopBtn.addEventListener("click", async () => {
    await playback.stop();
    stopPlayheadLoop();
    ui.status.textContent = "stopped";
    setTransportEnabled(true);
  });

  // ---- selection inputs / trim buttons ----------------------------------

  const onTimeInputChange = (which: "in" | "out") => (): void => {
    if (!selection || sourceSampleRate === 0) return;
    const input = which === "in" ? ui.inInput : ui.outInput;
    const seconds = parseFloat(input.value);
    if (!Number.isFinite(seconds)) {
      syncSelectionInputs();
      return;
    }
    const frame = secondsToFrames(seconds);
    const next: Selection =
      which === "in"
        ? { inFrame: frame, outFrame: selection.outFrame }
        : { inFrame: selection.inFrame, outFrame: frame };
    if (which === "in" && next.inFrame >= next.outFrame) {
      next.inFrame = Math.max(0, next.outFrame - 1);
    }
    if (which === "out" && next.outFrame <= next.inFrame) {
      next.outFrame = Math.min(sourceFrameCount, next.inFrame + 1);
    }
    setSelection(next);
  };

  const wrapCommit = (fn: () => void) => (): void => {
    fn();
    commitSelectionToPlayback();
  };
  ui.inInput.addEventListener("change", wrapCommit(onTimeInputChange("in")));
  ui.outInput.addEventListener("change", wrapCommit(onTimeInputChange("out")));
  ui.inMinus.addEventListener("click", wrapCommit(() => trim("in", -TRIM_NUDGE_MS)));
  ui.inPlus.addEventListener("click", wrapCommit(() => trim("in", +TRIM_NUDGE_MS)));
  ui.outMinus.addEventListener("click", wrapCommit(() => trim("out", -TRIM_NUDGE_MS)));
  ui.outPlus.addEventListener("click", wrapCommit(() => trim("out", +TRIM_NUDGE_MS)));

  // ---- zoom controls ---------------------------------------------------

  // Buttons zoom around the in-point if there is one (so the selection
  // stays visible), else around the viewport centre. The wheel zoom below
  // keeps its cursor-anchored behaviour.
  const buttonZoomPivot = (): number => {
    if (selection) return selection.inFrame;
    return (viewport.startFrame + viewport.endFrame) / 2;
  };
  ui.zoomInBtn.addEventListener("click", () => {
    if (sourceFrameCount === 0) return;
    zoomCenterOn(0.5, buttonZoomPivot());
  });
  ui.zoomOutBtn.addEventListener("click", () => {
    if (sourceFrameCount === 0) return;
    zoomCenterOn(2, buttonZoomPivot());
  });
  ui.zoomFullBtn.addEventListener("click", () => zoomFull());
  ui.zoomSelBtn.addEventListener("click", () => zoomToSelection());

  ui.playhead.addEventListener("wheel", (ev) => {
    if (sourceFrameCount === 0) return;
    ev.preventDefault();
    const factor = ev.deltaY > 0 ? 1.25 : 0.8;
    const pivot = clientXToFrame(ev.clientX);
    zoomBy(factor, pivot);
  });

  ui.scrollbar.addEventListener("input", () => {
    if (sourceFrameCount === 0) return;
    const start = parseInt(ui.scrollbar.value, 10);
    if (!Number.isFinite(start)) return;
    const len = viewportLength();
    setViewport(start, start + len);
  });

  // ---- effects --------------------------------------------------------

  /** Convert 0..200% scale to dB. 100% → 0 dB; 0% maps to -120 dB. */
  const percentToDb = (percent: number): number => {
    if (percent <= 0) return -120;
    return 20 * Math.log10(percent / 100);
  };

  ui.gainSlider.addEventListener("input", () => {
    ui.gainLabel.textContent = `${ui.gainSlider.value}%`;
    drawCachedWaveform();
  });

  const refreshAfterEdit = async (): Promise<void> => {
    await redrawWaveform();
    redrawOverlay();
    if (playback.isPlaying()) {
      const fromFrame =
        playback.isPaused() && selection ? selection.inFrame : currentSourceFrame();
      await startPlay(playback.isLooping(), fromFrame);
    }
  };

  ui.undoBtn.addEventListener("click", async () => {
    if (!currentSourceId) return;
    ui.undoBtn.disabled = true;
    try {
      const did = await client.undo(currentSourceId);
      ui.status.textContent = did ? "undone" : "nothing to undo";
      await refreshAfterEdit();
    } catch (err) {
      ui.status.textContent = `undo failed: ${String(err)}`;
    } finally {
      ui.undoBtn.disabled = currentSourceId === null;
    }
  });

  ui.redoBtn.addEventListener("click", async () => {
    if (!currentSourceId) return;
    ui.redoBtn.disabled = true;
    try {
      const did = await client.redo(currentSourceId);
      ui.status.textContent = did ? "redone" : "nothing to redo";
      await refreshAfterEdit();
    } catch (err) {
      ui.status.textContent = `redo failed: ${String(err)}`;
    } finally {
      ui.redoBtn.disabled = currentSourceId === null;
    }
  });

  ui.gainApplyBtn.addEventListener("click", async () => {
    if (!currentSourceId || !selection) return;
    const percent = parseFloat(ui.gainSlider.value);
    if (!Number.isFinite(percent)) return;
    const db = percentToDb(percent);
    const opJson = JSON.stringify({
      Gain: { range: { start: selection.inFrame, end: selection.outFrame }, db },
    });
    ui.gainApplyBtn.disabled = true;
    try {
      await client.applyOp(currentSourceId, opJson);
      ui.gainSlider.value = "100";
      ui.gainLabel.textContent = "100%";
      await redrawWaveform();
      redrawOverlay();
      if (playback.isPlaying()) {
        const fromFrame = playback.isPaused() ? selection.inFrame : currentSourceFrame();
        await startPlay(playback.isLooping(), fromFrame);
      }
      ui.status.textContent = `gain ${percent}% (${db.toFixed(2)} dB) applied`;
    } catch (err) {
      ui.status.textContent = `gain failed: ${String(err)}`;
    } finally {
      ui.gainApplyBtn.disabled = selection === null;
    }
  });

  syncSelectionInputs();
  syncZoomButtons();
  syncScrollbar();

  const reset = async (): Promise<void> => {
    await playback.stop();
    stopPlayheadLoop();
    currentSourceId = null;
    sourceFrameCount = 0;
    sourceSampleRate = 0;
    viewport = { startFrame: 0, endFrame: 0 };
    cachedPeaks = null;
    ui.gainSlider.value = "100";
    ui.gainLabel.textContent = "100%";
    setSelection(null);
    setTransportEnabled(false);
    ui.undoBtn.disabled = true;
    ui.redoBtn.disabled = true;
    ui.fileInput.value = "";
    ui.status.textContent = "no source loaded";
    const ctx = ui.canvas.getContext("2d");
    if (ctx) ctx.clearRect(0, 0, ui.canvas.width, ui.canvas.height);
    redrawOverlay();
    syncScrollbar();
    syncZoomButtons();
  };

  return { reset };
}

interface UiHandles {
  fileInput: HTMLInputElement;
  canvas: HTMLCanvasElement;
  playhead: HTMLCanvasElement;
  status: HTMLElement;
  playBtn: HTMLButtonElement;
  stopBtn: HTMLButtonElement;
  loopBtn: HTMLButtonElement;
  inInput: HTMLInputElement;
  outInput: HTMLInputElement;
  inMinus: HTMLButtonElement;
  inPlus: HTMLButtonElement;
  outMinus: HTMLButtonElement;
  outPlus: HTMLButtonElement;
  zoomInBtn: HTMLButtonElement;
  zoomOutBtn: HTMLButtonElement;
  zoomFullBtn: HTMLButtonElement;
  zoomSelBtn: HTMLButtonElement;
  scrollbar: HTMLInputElement;
  gainSlider: HTMLInputElement;
  gainLabel: HTMLElement;
  gainApplyBtn: HTMLButtonElement;
  undoBtn: HTMLButtonElement;
  redoBtn: HTMLButtonElement;
}

function buildUi(root: HTMLElement): UiHandles {
  root.innerHTML = "";
  Object.assign(root.style, {
    display: "flex",
    flexDirection: "column",
    gap: "12px",
  } satisfies Partial<CSSStyleDeclaration>);

  const fileRow = document.createElement("label");
  fileRow.style.display = "flex";
  fileRow.style.alignItems = "center";
  fileRow.style.gap = "8px";
  fileRow.textContent = "Open WAV: ";
  const fileInput = document.createElement("input");
  fileInput.type = "file";
  fileInput.accept = ".wav,audio/wav,audio/x-wav";
  fileRow.appendChild(fileInput);
  root.appendChild(fileRow);

  const transport = document.createElement("div");
  transport.style.display = "flex";
  transport.style.gap = "8px";
  const playBtn = document.createElement("button");
  playBtn.textContent = "Play";
  playBtn.disabled = true;
  const stopBtn = document.createElement("button");
  stopBtn.textContent = "Stop";
  stopBtn.disabled = true;
  const loopBtn = document.createElement("button");
  loopBtn.textContent = "Loop";
  loopBtn.disabled = true;
  loopBtn.type = "button";
  for (const b of [playBtn, stopBtn, loopBtn]) {
    Object.assign(b.style, btnStyle());
  }
  transport.appendChild(playBtn);
  transport.appendChild(loopBtn);
  transport.appendChild(stopBtn);
  root.appendChild(transport);

  const canvasWrap = document.createElement("div");
  Object.assign(canvasWrap.style, {
    position: "relative",
    width: "100%",
    height: "200px",
  } satisfies Partial<CSSStyleDeclaration>);
  const canvas = document.createElement("canvas");
  canvas.width = 1024;
  canvas.height = 200;
  Object.assign(canvas.style, {
    position: "absolute",
    inset: "0",
    width: "100%",
    height: "100%",
    background: "#0c0c0c",
    border: "1px solid #2a2a2a",
    boxSizing: "border-box",
  } satisfies Partial<CSSStyleDeclaration>);
  const playhead = document.createElement("canvas");
  playhead.width = 1024;
  playhead.height = 200;
  Object.assign(playhead.style, {
    position: "absolute",
    inset: "0",
    width: "100%",
    height: "100%",
    cursor: "crosshair",
  } satisfies Partial<CSSStyleDeclaration>);
  canvasWrap.appendChild(canvas);
  canvasWrap.appendChild(playhead);
  root.appendChild(canvasWrap);

  const scrollbar = document.createElement("input");
  scrollbar.type = "range";
  scrollbar.disabled = true;
  Object.assign(scrollbar.style, {
    width: "100%",
    margin: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  root.appendChild(scrollbar);

  const zoomRow = document.createElement("div");
  Object.assign(zoomRow.style, {
    display: "flex",
    alignItems: "center",
    gap: "8px",
    fontSize: "13px",
  } satisfies Partial<CSSStyleDeclaration>);
  const zoomLabel = document.createElement("span");
  zoomLabel.textContent = "Zoom:";
  const zoomInBtn = document.createElement("button");
  zoomInBtn.textContent = "In";
  const zoomOutBtn = document.createElement("button");
  zoomOutBtn.textContent = "Out";
  const zoomFullBtn = document.createElement("button");
  zoomFullBtn.textContent = "Full";
  const zoomSelBtn = document.createElement("button");
  zoomSelBtn.textContent = "Selection";
  for (const b of [zoomInBtn, zoomOutBtn, zoomFullBtn, zoomSelBtn]) {
    Object.assign(b.style, btnStyle(), {
      padding: "4px 10px",
    } satisfies Partial<CSSStyleDeclaration>);
    b.type = "button";
    b.disabled = true;
  }
  const wheelHint = document.createElement("span");
  wheelHint.textContent = "(scroll on waveform to zoom around cursor)";
  wheelHint.style.color = "#9a9a9a";
  zoomRow.appendChild(zoomLabel);
  zoomRow.appendChild(zoomInBtn);
  zoomRow.appendChild(zoomOutBtn);
  zoomRow.appendChild(zoomFullBtn);
  zoomRow.appendChild(zoomSelBtn);
  zoomRow.appendChild(wheelHint);
  root.appendChild(zoomRow);

  const fxRow = document.createElement("div");
  Object.assign(fxRow.style, {
    display: "flex",
    alignItems: "center",
    gap: "8px",
    fontSize: "13px",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxLabel = document.createElement("span");
  fxLabel.textContent = "Amplify:";
  const gainSlider = document.createElement("input");
  gainSlider.type = "range";
  gainSlider.min = "0";
  gainSlider.max = "200";
  gainSlider.step = "1";
  gainSlider.value = "100";
  gainSlider.disabled = true;
  Object.assign(gainSlider.style, {
    flex: "1",
    maxWidth: "300px",
  } satisfies Partial<CSSStyleDeclaration>);
  const gainLabel = document.createElement("span");
  gainLabel.textContent = "100%";
  Object.assign(gainLabel.style, {
    fontFamily: "ui-monospace, monospace",
    minWidth: "70px",
    textAlign: "right",
  } satisfies Partial<CSSStyleDeclaration>);
  const gainApplyBtn = document.createElement("button");
  gainApplyBtn.textContent = "Apply to selection";
  gainApplyBtn.type = "button";
  gainApplyBtn.disabled = true;
  Object.assign(gainApplyBtn.style, btnStyle(), {
    padding: "4px 10px",
  } satisfies Partial<CSSStyleDeclaration>);
  const gainHint = document.createElement("span");
  gainHint.textContent = "(100% = unchanged · 0% = silent)";
  gainHint.style.color = "#9a9a9a";
  const undoBtn = document.createElement("button");
  undoBtn.textContent = "Undo";
  undoBtn.type = "button";
  undoBtn.disabled = true;
  Object.assign(undoBtn.style, btnStyle(), {
    padding: "4px 10px",
  } satisfies Partial<CSSStyleDeclaration>);
  const redoBtn = document.createElement("button");
  redoBtn.textContent = "Redo";
  redoBtn.type = "button";
  redoBtn.disabled = true;
  Object.assign(redoBtn.style, btnStyle(), {
    padding: "4px 10px",
  } satisfies Partial<CSSStyleDeclaration>);

  fxRow.appendChild(fxLabel);
  fxRow.appendChild(gainSlider);
  fxRow.appendChild(gainLabel);
  fxRow.appendChild(gainApplyBtn);
  fxRow.appendChild(undoBtn);
  fxRow.appendChild(redoBtn);
  fxRow.appendChild(gainHint);
  root.appendChild(fxRow);

  const selRow = document.createElement("div");
  Object.assign(selRow.style, {
    display: "flex",
    alignItems: "center",
    gap: "12px",
    fontSize: "13px",
    flexWrap: "wrap",
  } satisfies Partial<CSSStyleDeclaration>);

  const makeNudgeBtn = (label: string): HTMLButtonElement => {
    const b = document.createElement("button");
    b.textContent = label;
    b.type = "button";
    b.disabled = true;
    Object.assign(b.style, btnStyle(), {
      padding: "4px 8px",
      minWidth: "32px",
    } satisfies Partial<CSSStyleDeclaration>);
    return b;
  };

  const makeTimeInput = (): HTMLInputElement => {
    const i = document.createElement("input");
    i.type = "number";
    i.step = "0.001";
    i.min = "0";
    i.disabled = true;
    Object.assign(i.style, {
      width: "100px",
      padding: "4px 6px",
      background: "#0c0c0c",
      color: "#d8d8d8",
      border: "1px solid #3a3a3a",
      fontFamily: "ui-monospace, monospace",
      fontSize: "13px",
    } satisfies Partial<CSSStyleDeclaration>);
    return i;
  };

  const inLabel = document.createElement("span");
  inLabel.textContent = "In:";
  const inInput = makeTimeInput();
  const inMinus = makeNudgeBtn("◀");
  const inPlus = makeNudgeBtn("▶");
  const outLabel = document.createElement("span");
  outLabel.textContent = "Out:";
  const outInput = makeTimeInput();
  const outMinus = makeNudgeBtn("◀");
  const outPlus = makeNudgeBtn("▶");
  const unitsLabel = document.createElement("span");
  unitsLabel.textContent = "(seconds, ±10 ms nudge)";
  unitsLabel.style.color = "#9a9a9a";

  selRow.appendChild(inLabel);
  selRow.appendChild(inInput);
  selRow.appendChild(inMinus);
  selRow.appendChild(inPlus);
  selRow.appendChild(outLabel);
  selRow.appendChild(outInput);
  selRow.appendChild(outMinus);
  selRow.appendChild(outPlus);
  selRow.appendChild(unitsLabel);
  root.appendChild(selRow);

  const status = document.createElement("div");
  status.textContent = "no source loaded";
  status.style.fontSize = "12px";
  status.style.color = "#9a9a9a";
  status.style.fontFamily = "ui-monospace, monospace";
  root.appendChild(status);

  return {
    fileInput,
    canvas,
    playhead,
    status,
    playBtn,
    stopBtn,
    loopBtn,
    inInput,
    outInput,
    inMinus,
    inPlus,
    outMinus,
    outPlus,
    zoomInBtn,
    zoomOutBtn,
    zoomFullBtn,
    zoomSelBtn,
    scrollbar,
    gainSlider,
    gainLabel,
    gainApplyBtn,
    undoBtn,
    redoBtn,
  };
}

export function btnStyle(): Partial<CSSStyleDeclaration> {
  return {
    padding: "6px 14px",
    background: "#2a2a2a",
    color: "#d8d8d8",
    border: "1px solid #3a3a3a",
    cursor: "pointer",
    fontFamily: "inherit",
  };
}
