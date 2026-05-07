// Single-source waveform editor tab. Cool-Edit-style layout: library pane on
// the left (every imported source, click-to-load, double-click-to-rename,
// Duplicate button), destructive FX panel + waveform + transport on the right.
// The shell (main.ts) boots the shared EngineClient and Playback and hands
// them in.

import type { EngineClient, NoiseProfileInfo, SourceInfo } from "./engine/client";
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
const NOTE_NAMES = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

export interface EditorOptions {
  /** Called after a fresh import / duplicate / rename so other tabs (the
   *  arranger) can refresh their source list. */
  onSourceImported?: () => void;
}

export interface EditorHandle {
  /** Drop the editor's current source and selection. Used after loading a
   *  fresh project at the shell level so the editor doesn't keep pointing
   *  at an id that no longer exists. */
  reset: () => Promise<void>;
  /** Re-fetch the source list from the engine. Called when the arranger
   *  imports a wav so the editor's library reflects it without a tab switch. */
  refreshLibrary: () => Promise<void>;
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
  let sources: SourceInfo[] = [];
  let noiseProfiles: NoiseProfileInfo[] = [];

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
    const hasSelection = selection !== null;
    for (const b of [ui.inMinus, ui.inPlus, ui.outMinus, ui.outPlus]) {
      b.disabled = !hasSelection;
    }
    ui.inInput.disabled = sourceFrameCount === 0;
    ui.outInput.disabled = sourceFrameCount === 0;
    ui.trimBtn.disabled = !hasSelection || currentSourceId === null;
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

  const drawCachedWaveform = (): void => {
    if (!cachedPeaks) return;
    drawWaveform(ui.canvas, cachedPeaks);
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

  // ---- library ---------------------------------------------------------

  const refreshLibrary = async (): Promise<void> => {
    sources = await client.listSources();
    renderLibrary();
  };

  // Folders the user has collapsed in the library. Persists across
  // refreshes within a session; folders default to expanded.
  const collapsedFolders = new Set<string>();

  const renderLibrary = (): void => {
    ui.libraryList.innerHTML = "";
    if (sources.length === 0) {
      const empty = document.createElement("div");
      empty.textContent = "No sources yet. Open a WAV.";
      Object.assign(empty.style, {
        color: "#888",
        fontStyle: "italic",
        padding: "8px",
        fontSize: "12px",
      } satisfies Partial<CSSStyleDeclaration>);
      ui.libraryList.appendChild(empty);
    } else {
      // Group every source under a folder header, including a default
      // "/" header for sources with no folder set — so the whole library
      // is collapsible. Folders sort alphabetically; "/" sorts to the
      // top because it's lexicographically before letters.
      const byName = (a: SourceInfo, b: SourceInfo): number =>
        a.name.localeCompare(b.name, undefined, { sensitivity: "base" }) ||
        a.id.localeCompare(b.id);
      const byFolder = new Map<string, SourceInfo[]>();
      for (const s of sources) {
        const key = s.folder && s.folder.length > 0 ? s.folder : "/";
        const list = byFolder.get(key) ?? [];
        list.push(s);
        byFolder.set(key, list);
      }
      for (const list of byFolder.values()) list.sort(byName);
      const folderNames = Array.from(byFolder.keys()).sort((a, b) =>
        a.localeCompare(b, undefined, { sensitivity: "base" }),
      );
      for (const folder of folderNames) {
        ui.libraryList.appendChild(makeFolderHeader(folder));
        if (collapsedFolders.has(folder)) continue;
        for (const s of byFolder.get(folder)!) {
          const row = makeLibraryRow(s);
          row.style.paddingLeft = "16px"; // visually nest folder children
          ui.libraryList.appendChild(row);
        }
      }
    }
    ui.duplicateBtn.disabled = currentSourceId === null;
  };

  const makeFolderHeader = (folder: string): HTMLDivElement => {
    const hdr = document.createElement("div");
    const collapsed = collapsedFolders.has(folder);
    Object.assign(hdr.style, {
      padding: "4px 8px",
      cursor: "pointer",
      background: "#1a1a1a",
      borderBottom: "1px solid #2a2a2a",
      display: "flex",
      alignItems: "center",
      gap: "6px",
      fontFamily: "var(--ff-mono)",
      fontSize: "11px",
      color: "var(--text-2)",
      letterSpacing: "0.04em",
    } satisfies Partial<CSSStyleDeclaration>);
    hdr.textContent = `${collapsed ? "▸" : "▾"} ${folder}`;
    hdr.title = "Click to collapse/expand";
    hdr.addEventListener("click", () => {
      if (collapsedFolders.has(folder)) collapsedFolders.delete(folder);
      else collapsedFolders.add(folder);
      renderLibrary();
    });
    return hdr;
  };

  const makeLibraryRow = (s: SourceInfo): HTMLDivElement => {
    const row = document.createElement("div");
    row.dataset.sourceId = s.id;
    Object.assign(row.style, {
      padding: "6px 8px",
      cursor: "pointer",
      borderBottom: "1px solid #1f1f1f",
      background: s.id === currentSourceId ? "#2a3a2a" : "transparent",
      display: "flex",
      flexDirection: "row",
      alignItems: "center",
      gap: "6px",
    } satisfies Partial<CSSStyleDeclaration>);

    // Left: name + meta stacked. flex:1 + minWidth:0 lets the name
    // ellipsis clip instead of pushing the action buttons off the row.
    const text = document.createElement("div");
    Object.assign(text.style, {
      flex: "1 1 auto",
      minWidth: "0",
      display: "flex",
      flexDirection: "column",
      gap: "2px",
      textAlign: "left",
    } satisfies Partial<CSSStyleDeclaration>);

    const nameEl = document.createElement("div");
    nameEl.textContent = s.name;
    nameEl.title = `${s.id} · ${s.frames.toLocaleString()} fr · ${s.sampleRate} Hz · ${s.channels}ch`;
    Object.assign(nameEl.style, {
      overflow: "hidden",
      textOverflow: "ellipsis",
      whiteSpace: "nowrap",
      fontSize: "12px",
      color: s.id === currentSourceId ? "#cfe9cf" : "#d8d8d8",
      textAlign: "left",
    } satisfies Partial<CSSStyleDeclaration>);

    const meta = document.createElement("div");
    const seconds = s.sampleRate > 0 ? s.frames / s.sampleRate : 0;
    meta.textContent = `${seconds.toFixed(2)}s · ${s.channels}ch`;
    Object.assign(meta.style, {
      color: "#888",
      fontSize: "10px",
      fontFamily: "ui-monospace, monospace",
      textAlign: "left",
    } satisfies Partial<CSSStyleDeclaration>);

    text.appendChild(nameEl);
    text.appendChild(meta);

    // Right: action buttons. Tiny so they don't crowd the row; clicks
    // don't propagate to the row's click-to-load handler.
    const actions = document.createElement("div");
    Object.assign(actions.style, {
      display: "inline-flex",
      gap: "2px",
      flexShrink: "0",
    } satisfies Partial<CSSStyleDeclaration>);
    const mkActionBtn = (label: string, title: string): HTMLButtonElement => {
      const b = document.createElement("button");
      b.type = "button";
      b.textContent = label;
      b.title = title;
      Object.assign(b.style, {
        width: "20px",
        height: "20px",
        padding: "0",
        background: "transparent",
        color: "var(--text-3)",
        border: "1px solid var(--line-1)",
        borderRadius: "3px",
        cursor: "pointer",
        fontSize: "11px",
        lineHeight: "1",
      } satisfies Partial<CSSStyleDeclaration>);
      b.addEventListener("mouseenter", () => {
        b.style.color = "var(--text-1)";
        b.style.borderColor = "var(--line-strong)";
      });
      b.addEventListener("mouseleave", () => {
        b.style.color = "var(--text-3)";
        b.style.borderColor = "var(--line-1)";
      });
      return b;
    };
    const folderBtn = mkActionBtn("📁", "Move to folder…");
    folderBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      void promptMoveToFolder(s);
    });
    const delBtn = mkActionBtn("×", "Delete from library");
    delBtn.style.fontSize = "14px";
    delBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      void confirmDeleteSource(s);
    });
    actions.appendChild(folderBtn);
    actions.appendChild(delBtn);

    row.appendChild(text);
    row.appendChild(actions);

    row.addEventListener("click", () => {
      if (s.id === currentSourceId) return;
      void loadSource(s.id);
    });
    nameEl.addEventListener("dblclick", (ev) => {
      ev.stopPropagation();
      beginRename(s, nameEl);
    });
    return row;
  };

  const promptMoveToFolder = async (s: SourceInfo): Promise<void> => {
    const current = s.folder ?? "";
    const next = window.prompt(
      `Move "${s.name}" to folder (blank = root):`,
      current,
    );
    if (next === null) return;
    const trimmed = next.trim();
    try {
      await client.setSourceFolder(s.id, trimmed === "" ? null : trimmed);
      await refreshLibrary();
      opts.onSourceImported?.();
    } catch (err) {
      ui.status.textContent = `move failed: ${String(err)}`;
    }
  };

  const confirmDeleteSource = async (s: SourceInfo): Promise<void> => {
    const ok = window.confirm(
      `Delete "${s.name}" from the library?\n\nAny clips referencing it on the arranger will also be removed.`,
    );
    if (!ok) return;
    try {
      const removed = await client.removeSource(s.id);
      if (!removed) {
        ui.status.textContent = "delete: source not found";
        return;
      }
      // If we just deleted the active source, clear the editor's view.
      if (currentSourceId === s.id) {
        currentSourceId = null;
        sourceFrameCount = 0;
        sourceSampleRate = 0;
        viewport = { startFrame: 0, endFrame: 0 };
        cachedPeaks = null;
        setSelection(null);
        setTransportEnabled(false);
        const ctx = ui.canvas.getContext("2d");
        if (ctx) ctx.clearRect(0, 0, ui.canvas.width, ui.canvas.height);
        redrawOverlay();
      }
      await refreshLibrary();
      opts.onSourceImported?.();
      ui.status.textContent = `deleted ${s.name}`;
    } catch (err) {
      ui.status.textContent = `delete failed: ${String(err)}`;
    }
  };

  const beginRename = (s: SourceInfo, nameEl: HTMLDivElement): void => {
    const input = document.createElement("input");
    input.type = "text";
    input.value = s.name;
    Object.assign(input.style, {
      width: "100%",
      padding: "2px 4px",
      background: "#0c0c0c",
      color: "#d8d8d8",
      border: "1px solid #5a5a5a",
      fontFamily: "inherit",
      fontSize: "12px",
      boxSizing: "border-box",
    } satisfies Partial<CSSStyleDeclaration>);
    nameEl.replaceWith(input);
    input.focus();
    input.select();

    let done = false;
    const finish = async (commit: boolean): Promise<void> => {
      if (done) return;
      done = true;
      if (commit) {
        const next = input.value.trim();
        if (next && next !== s.name) {
          try {
            await client.renameSource(s.id, next);
            opts.onSourceImported?.();
          } catch (err) {
            ui.status.textContent = `rename failed: ${String(err)}`;
          }
        }
      }
      await refreshLibrary();
    };
    input.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") {
        ev.preventDefault();
        void finish(true);
      } else if (ev.key === "Escape") {
        ev.preventDefault();
        void finish(false);
      }
    });
    input.addEventListener("blur", () => {
      void finish(true);
    });
  };

  /** Switch the editor to an existing source from the library. */
  const loadSource = async (id: string): Promise<void> => {
    await playback.stop();
    stopPlayheadLoop();
    const info = sources.find((s) => s.id === id);
    if (!info) {
      ui.status.textContent = `source ${id} not found`;
      return;
    }
    currentSourceId = id;
    sourceFrameCount = info.frames;
    sourceSampleRate = info.sampleRate;
    viewport = { startFrame: 0, endFrame: info.frames };
    cachedPeaks = null;
    setSelection(null);
    setTransportEnabled(true);
    ui.undoBtn.disabled = false;
    ui.redoBtn.disabled = false;
    ui.duplicateBtn.disabled = false;
    syncFxApplyEnabled();
    ui.status.textContent = `${info.name} · ${info.id} · ${info.frames.toLocaleString()} frames @ ${info.sampleRate} Hz`;
    renderLibrary();
    syncSelectionInputs();
    syncScrollbar();
    syncZoomButtons();
    await redrawWaveform();
    redrawOverlay();
  };

  // ---- file load -------------------------------------------------------

  ui.fileInput.addEventListener("change", async () => {
    const files = Array.from(ui.fileInput.files ?? []);
    if (files.length === 0) return;
    let lastImported: string | null = null;
    let succeeded = 0;
    const failures: string[] = [];
    for (const file of files) {
      ui.status.textContent =
        files.length > 1
          ? `decoding ${file.name} (${succeeded + failures.length + 1}/${files.length})…`
          : `decoding ${file.name}…`;
      try {
        const buf = await file.arrayBuffer();
        const imp = await client.importWav(file.name, new Uint8Array(buf));
        lastImported = imp.sourceId;
        succeeded++;
      } catch (err) {
        failures.push(`${file.name}: ${String(err)}`);
      }
    }
    ui.fileInput.value = "";
    await refreshLibrary();
    if (lastImported !== null) {
      await loadSource(lastImported);
    }
    opts.onSourceImported?.();
    if (failures.length === 0) {
      ui.status.textContent =
        succeeded === 1
          ? `imported ${files[0]!.name}`
          : `imported ${succeeded} files`;
    } else if (succeeded === 0) {
      ui.status.textContent = `import failed: ${failures.join("; ")}`;
    } else {
      ui.status.textContent = `imported ${succeeded}, failed ${failures.length}: ${failures.join("; ")}`;
    }
  });

  // ---- new blank source ------------------------------------------------

  ui.newSourceBtn.addEventListener("click", async () => {
    const raw = window.prompt("Length of new blank source (seconds):", "5");
    if (raw === null) return;
    const seconds = parseFloat(raw);
    if (!Number.isFinite(seconds) || seconds <= 0) {
      ui.status.textContent = "new source: enter a positive duration";
      return;
    }
    ui.newSourceBtn.disabled = true;
    try {
      const projectSr = await client.projectSampleRate();
      const lengthFrames = Math.max(1, Math.round(seconds * projectSr));
      const newId = await client.createEmptySource(lengthFrames, 2, "blank.wav");
      await refreshLibrary();
      await loadSource(newId);
      opts.onSourceImported?.();
      ui.status.textContent = `created blank source (${seconds.toFixed(2)}s @ ${projectSr} Hz)`;
    } catch (err) {
      ui.status.textContent = `new source failed: ${String(err)}`;
    } finally {
      ui.newSourceBtn.disabled = false;
    }
  });

  // ---- duplicate -------------------------------------------------------

  ui.duplicateBtn.addEventListener("click", async () => {
    if (!currentSourceId) return;
    ui.duplicateBtn.disabled = true;
    try {
      const newId = await client.duplicateSource(currentSourceId);
      await refreshLibrary();
      await loadSource(newId);
      opts.onSourceImported?.();
      ui.status.textContent = `duplicated`;
    } catch (err) {
      ui.status.textContent = `duplicate failed: ${String(err)}`;
      ui.duplicateBtn.disabled = currentSourceId === null;
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

  // ---- effects ---------------------------------------------------------
  // Destructive ops applied to the current source. Range = current
  // selection if any, else the whole source. Each kind has its own
  // params row; Autotune is a special case because it has two modes.

  type FxParam =
    | {
        kind: "number";
        key: string;
        label: string;
        min: number;
        max: number;
        step: number;
        default: number;
      }
    | { kind: "select"; key: string; label: string; options: string[]; default: string };

  interface FxDef {
    id: string;
    label: string;
    params: FxParam[];
    build: (range: { start: number; end: number }, vals: Record<string, string>) => unknown;
  }

  const num = (vals: Record<string, string>, k: string, fallback: number): number => {
    const v = parseFloat(vals[k] ?? "");
    return Number.isFinite(v) ? v : fallback;
  };

  const fxDefs: FxDef[] = [
    {
      id: "Gain",
      label: "Gain",
      params: [{ kind: "number", key: "db", label: "dB", min: -60, max: 24, step: 0.5, default: 0 }],
      build: (range, vals) => ({ Gain: { range, db: num(vals, "db", 0) } }),
    },
    {
      id: "Normalize",
      label: "Normalize",
      params: [
        {
          kind: "select",
          key: "target",
          label: "target",
          options: ["Peak", "Rms", "LufsIntegrated"],
          default: "Peak",
        },
        { kind: "number", key: "value_db", label: "dB", min: -30, max: 0, step: 0.1, default: -1 },
      ],
      build: (range, vals) => ({
        Normalize: {
          range,
          target: vals.target ?? "Peak",
          value_db: num(vals, "value_db", -1),
        },
      }),
    },
    { id: "Reverse", label: "Reverse", params: [], build: (range) => ({ Reverse: { range } }) },
    { id: "DcRemove", label: "DC Remove", params: [], build: (range) => ({ DcRemove: { range } }) },
    { id: "Silence", label: "Silence", params: [], build: (range) => ({ Silence: { range } }) },
    { id: "Cut", label: "Cut", params: [], build: (range) => ({ Cut: { range } }) },
    {
      id: "Fade",
      label: "Fade",
      params: [
        { kind: "select", key: "direction", label: "dir", options: ["In", "Out"], default: "In" },
        {
          kind: "select",
          key: "shape",
          label: "shape",
          options: ["Linear", "Logarithmic", "Exponential", "SCurve"],
          default: "Linear",
        },
      ],
      build: (range, vals) => ({
        Fade: {
          range,
          shape: vals.shape ?? "Linear",
          direction: vals.direction ?? "In",
        },
      }),
    },
    {
      id: "Reverb",
      label: "Reverb",
      params: [
        {
          kind: "select",
          key: "model",
          label: "model",
          options: ["Hall", "Room", "Plate"],
          default: "Hall",
        },
        { kind: "number", key: "size", label: "size", min: 0, max: 1, step: 0.05, default: 0.5 },
        { kind: "number", key: "damping", label: "damp", min: 0, max: 1, step: 0.05, default: 0.5 },
        { kind: "number", key: "mix", label: "mix", min: 0, max: 1, step: 0.05, default: 0.3 },
      ],
      build: (range, vals) => ({
        Reverb: {
          range,
          params: {
            model: vals.model ?? "Hall",
            size: num(vals, "size", 0.5),
            damping: num(vals, "damping", 0.5),
            mix: num(vals, "mix", 0.3),
          },
        },
      }),
    },
    {
      id: "Delay",
      label: "Delay",
      params: [
        { kind: "number", key: "time_ms", label: "ms", min: 1, max: 2000, step: 10, default: 250 },
        { kind: "number", key: "feedback", label: "fb", min: 0, max: 0.95, step: 0.05, default: 0.4 },
        { kind: "number", key: "mix", label: "mix", min: 0, max: 1, step: 0.05, default: 0.3 },
        {
          kind: "select",
          key: "ping_pong",
          label: "ping-pong",
          options: ["off", "on"],
          default: "off",
        },
      ],
      build: (range, vals) => ({
        Delay: {
          range,
          params: {
            time_ms: num(vals, "time_ms", 250),
            feedback: num(vals, "feedback", 0.4),
            mix: num(vals, "mix", 0.3),
            ping_pong: vals.ping_pong === "on",
            feedback_lp_hz: null,
          },
        },
      }),
    },
    {
      id: "Distortion",
      label: "Distortion",
      params: [
        { kind: "number", key: "drive_db", label: "drive dB", min: 0, max: 48, step: 0.5, default: 12 },
        { kind: "number", key: "tone_hz", label: "tone Hz", min: 0, max: 20000, step: 100, default: 0 },
        { kind: "number", key: "mix", label: "mix", min: 0, max: 1, step: 0.05, default: 1.0 },
      ],
      // tone_hz=0 means "no lowpass" — pass null on the wire so the
      // engine's optional field reads as None.
      build: (range, vals) => {
        const tone = num(vals, "tone_hz", 0);
        return {
          Distortion: {
            range,
            params: {
              drive_db: num(vals, "drive_db", 12),
              tone_hz: tone > 0 ? tone : null,
              mix: num(vals, "mix", 1.0),
            },
          },
        };
      },
    },
    {
      id: "Chorus",
      label: "Chorus",
      params: [
        { kind: "number", key: "rate_hz", label: "rate Hz", min: 0.05, max: 8, step: 0.05, default: 1.0 },
        { kind: "number", key: "depth_ms", label: "depth ms", min: 0.5, max: 20, step: 0.5, default: 4.0 },
        { kind: "number", key: "mix", label: "mix", min: 0, max: 1, step: 0.05, default: 0.5 },
        { kind: "number", key: "voices", label: "voices", min: 1, max: 6, step: 1, default: 2 },
      ],
      build: (range, vals) => ({
        Chorus: {
          range,
          params: {
            rate_hz: num(vals, "rate_hz", 1.0),
            depth_ms: num(vals, "depth_ms", 4.0),
            mix: num(vals, "mix", 0.5),
            voices: Math.round(num(vals, "voices", 2)),
          },
        },
      }),
    },
    {
      id: "Compressor",
      label: "Compressor",
      params: [
        { kind: "number", key: "threshold_db", label: "thresh dB", min: -60, max: 0, step: 0.5, default: -18 },
        { kind: "number", key: "ratio", label: "ratio", min: 1, max: 20, step: 0.5, default: 4 },
        { kind: "number", key: "attack_ms", label: "atk ms", min: 0.1, max: 100, step: 0.5, default: 5 },
        { kind: "number", key: "release_ms", label: "rel ms", min: 5, max: 1000, step: 5, default: 80 },
        { kind: "number", key: "knee_db", label: "knee dB", min: 0, max: 24, step: 0.5, default: 6 },
        { kind: "number", key: "makeup_db", label: "makeup dB", min: 0, max: 24, step: 0.5, default: 0 },
      ],
      build: (range, vals) => ({
        Compress: {
          range,
          params: {
            threshold_db: num(vals, "threshold_db", -18),
            ratio: num(vals, "ratio", 4),
            attack_ms: num(vals, "attack_ms", 5),
            release_ms: num(vals, "release_ms", 80),
            knee_db: num(vals, "knee_db", 6),
            makeup_db: num(vals, "makeup_db", 0),
          },
        },
      }),
    },
    {
      id: "Limiter",
      label: "Limiter",
      params: [
        { kind: "number", key: "ceiling_db", label: "ceiling dB", min: -12, max: 0, step: 0.1, default: -0.3 },
        { kind: "number", key: "lookahead_ms", label: "lookahead ms", min: 0, max: 20, step: 0.5, default: 2 },
        { kind: "number", key: "release_ms", label: "release ms", min: 10, max: 500, step: 5, default: 50 },
      ],
      build: (range, vals) => ({
        Limit: {
          range,
          params: {
            ceiling_db: num(vals, "ceiling_db", -0.3),
            lookahead_ms: num(vals, "lookahead_ms", 2),
            release_ms: num(vals, "release_ms", 50),
          },
        },
      }),
    },
    {
      id: "TimeStretch",
      label: "Time Stretch",
      params: [
        { kind: "number", key: "ratio", label: "ratio", min: 0.25, max: 4, step: 0.05, default: 1.0 },
      ],
      build: (range, vals) => ({ TimeStretch: { range, ratio: num(vals, "ratio", 1.0) } }),
    },
    {
      id: "PitchShift",
      label: "Pitch Shift",
      params: [
        { kind: "number", key: "cents", label: "cents", min: -2400, max: 2400, step: 50, default: 0 },
      ],
      build: (range, vals) => ({ PitchShift: { range, cents: num(vals, "cents", 0) } }),
    },
  ];

  for (const def of fxDefs) {
    const opt = document.createElement("option");
    opt.value = def.id;
    opt.textContent = def.label;
    ui.fxKindSelect.appendChild(opt);
  }
  // Autotune, EQ, Generate, and NoiseReduce are special cases that don't
  // fit the simple fxDefs schema. Autotune has mode-dependent inputs, EQ
  // uses a fixed set of three bands, Generate inserts at a position
  // rather than processing a range, and NoiseReduce is a two-stage
  // capture-then-apply workflow that needs the project's profile list.
  for (const id of ["Eq", "Generate", "NoiseReduce", "Autotune"]) {
    const opt = document.createElement("option");
    opt.value = id;
    const labels: Record<string, string> = {
      Eq: "EQ",
      Generate: "Generate",
      NoiseReduce: "Noise Reduction",
    };
    opt.textContent = labels[id] ?? id;
    ui.fxKindSelect.appendChild(opt);
  }

  let autotuneInputs: {
    mode: HTMLSelectElement;
    scaleRow: HTMLElement;
    scale: HTMLSelectElement;
    key: HTMLSelectElement;
    referenceRow: HTMLElement;
    refSource: HTMLSelectElement;
    retune: HTMLInputElement;
    formant: HTMLInputElement;
  } | null = null;

  /** Three fixed-shape EQ bands: lowshelf, peak, highshelf. Sufficient
   *  for tonal sweeps without requiring a full variable band-list editor.
   *  A free band list can land later with the same Op::Eq shape. */
  let eqInputs: Array<{
    freq: HTMLInputElement;
    gain: HTMLInputElement;
    q: HTMLInputElement;
    kind: "Lowshelf" | "Peak" | "Highshelf";
  }> | null = null;

  /** Generate-op inputs. Only the Tone and Noise (white) variants are
   *  exposed in v1 — pink/brown noise, DTMF, and sweeps return
   *  Unsupported in the engine today. */
  let generateInputs: {
    mode: HTMLSelectElement;
    toneRow: HTMLElement;
    shape: HTMLSelectElement;
    freq: HTMLInputElement;
    toneAmp: HTMLInputElement;
    noiseRow: HTMLElement;
    noiseColor: HTMLSelectElement;
    noiseAmp: HTMLInputElement;
    length: HTMLInputElement;
  } | null = null;

  /** Noise-reduce two-stage workflow. Capture grabs an averaged spectrum
   *  from a selection of pure-noise audio; Apply uses a captured profile
   *  to subtract that spectrum from the working selection. */
  let nrInputs: {
    mode: HTMLSelectElement;
    captureRow: HTMLElement;
    captureName: HTMLInputElement;
    applyRow: HTMLElement;
    profile: HTMLSelectElement;
    amountDb: HTMLInputElement;
    floorDb: HTMLInputElement;
    oversub: HTMLInputElement;
  } | null = null;

  let currentFxInputs: Record<string, HTMLInputElement | HTMLSelectElement> = {};

  const populateReferenceSources = (excludeId: string | null): void => {
    if (!autotuneInputs) return;
    const sel = autotuneInputs.refSource;
    sel.innerHTML = "";
    const candidates = sources.filter((s) => s.id !== excludeId);
    if (candidates.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "(no other source)";
      opt.disabled = true;
      sel.appendChild(opt);
      return;
    }
    for (const s of candidates) {
      const opt = document.createElement("option");
      opt.value = s.id;
      opt.textContent = s.name;
      sel.appendChild(opt);
    }
  };

  const wrapStyle: Partial<CSSStyleDeclaration> = {
    display: "inline-flex",
    alignItems: "center",
    gap: "4px",
    fontSize: "11px",
    color: "#aaa",
  };
  const selStyle: Partial<CSSStyleDeclaration> = {
    padding: "2px 4px",
    background: "#0c0c0c",
    color: "#d8d8d8",
    border: "1px solid #3a3a3a",
    fontSize: "12px",
  };
  const numberInputStyle: Partial<CSSStyleDeclaration> = {
    width: "60px",
    padding: "2px 4px",
    background: "#0c0c0c",
    color: "#d8d8d8",
    border: "1px solid #3a3a3a",
    fontFamily: "ui-monospace, monospace",
    fontSize: "12px",
  };

  const makeNumberInput = (): HTMLInputElement => {
    const i = document.createElement("input");
    i.type = "number";
    Object.assign(i.style, numberInputStyle);
    return i;
  };

  const buildAutotuneInputs = (): void => {
    ui.fxParamsRow.innerHTML = "";
    currentFxInputs = {};

    const modeWrap = document.createElement("label");
    Object.assign(modeWrap.style, wrapStyle);
    modeWrap.appendChild(document.createTextNode("mode"));
    const mode = document.createElement("select");
    Object.assign(mode.style, selStyle);
    for (const m of ["Scale", "Reference"]) {
      const o = document.createElement("option");
      o.value = m;
      o.textContent = m;
      mode.appendChild(o);
    }
    modeWrap.appendChild(mode);

    const scaleRow = document.createElement("span");
    Object.assign(scaleRow.style, { display: "inline-flex", gap: "6px" } satisfies Partial<CSSStyleDeclaration>);
    const scaleWrap = document.createElement("label");
    Object.assign(scaleWrap.style, wrapStyle);
    scaleWrap.appendChild(document.createTextNode("scale"));
    const scale = document.createElement("select");
    Object.assign(scale.style, selStyle);
    for (const s of ["Chromatic", "Major", "Minor"]) {
      const o = document.createElement("option");
      o.value = s;
      o.textContent = s;
      scale.appendChild(o);
    }
    scaleWrap.appendChild(scale);
    const keyWrap = document.createElement("label");
    Object.assign(keyWrap.style, wrapStyle);
    keyWrap.appendChild(document.createTextNode("key"));
    const key = document.createElement("select");
    Object.assign(key.style, selStyle);
    for (let i = 0; i < 12; i++) {
      const o = document.createElement("option");
      o.value = String(i);
      o.textContent = NOTE_NAMES[i]!;
      key.appendChild(o);
    }
    keyWrap.appendChild(key);
    scaleRow.appendChild(scaleWrap);
    scaleRow.appendChild(keyWrap);

    const referenceRow = document.createElement("span");
    Object.assign(referenceRow.style, { display: "none", gap: "6px" } satisfies Partial<CSSStyleDeclaration>);
    const refWrap = document.createElement("label");
    Object.assign(refWrap.style, wrapStyle);
    refWrap.appendChild(document.createTextNode("reference"));
    const refSource = document.createElement("select");
    Object.assign(refSource.style, selStyle);
    refWrap.appendChild(refSource);
    referenceRow.appendChild(refWrap);

    const retuneWrap = document.createElement("label");
    Object.assign(retuneWrap.style, wrapStyle);
    retuneWrap.appendChild(document.createTextNode("retune ms"));
    const retune = makeNumberInput();
    retune.min = "0";
    retune.max = "500";
    retune.step = "5";
    retune.value = "0";
    retune.title = "0 = instant snap (T-Pain). Larger values glide toward target.";
    retuneWrap.appendChild(retune);

    const formantWrap = document.createElement("label");
    Object.assign(formantWrap.style, wrapStyle);
    formantWrap.title =
      "Phase-vocoder path with cepstral envelope preservation — keeps formants in place during pitch shift.";
    const formant = document.createElement("input");
    formant.type = "checkbox";
    formantWrap.appendChild(formant);
    formantWrap.appendChild(document.createTextNode("preserve formants"));

    ui.fxParamsRow.appendChild(modeWrap);
    ui.fxParamsRow.appendChild(scaleRow);
    ui.fxParamsRow.appendChild(referenceRow);
    ui.fxParamsRow.appendChild(retuneWrap);
    ui.fxParamsRow.appendChild(formantWrap);

    autotuneInputs = {
      mode,
      scaleRow,
      scale,
      key,
      referenceRow,
      refSource,
      retune,
      formant,
    };

    mode.addEventListener("change", () => {
      const isRef = mode.value === "Reference";
      scaleRow.style.display = isRef ? "none" : "inline-flex";
      referenceRow.style.display = isRef ? "inline-flex" : "none";
      if (isRef) populateReferenceSources(currentSourceId);
    });
  };

  const buildEqInputs = (): void => {
    ui.fxParamsRow.innerHTML = "";
    currentFxInputs = {};
    autotuneInputs = null;

    const defaults: Array<{
      kind: "Lowshelf" | "Peak" | "Highshelf";
      label: string;
      freq: number;
      q: number;
    }> = [
      { kind: "Lowshelf", label: "low", freq: 100, q: 0.7 },
      { kind: "Peak", label: "mid", freq: 1000, q: 1.0 },
      { kind: "Highshelf", label: "high", freq: 8000, q: 0.7 },
    ];

    const bands: Array<{
      freq: HTMLInputElement;
      gain: HTMLInputElement;
      q: HTMLInputElement;
      kind: "Lowshelf" | "Peak" | "Highshelf";
    }> = [];

    for (const d of defaults) {
      const bandWrap = document.createElement("span");
      Object.assign(bandWrap.style, {
        display: "inline-flex",
        alignItems: "center",
        gap: "4px",
        padding: "2px 6px",
        background: "var(--bg-sunken)",
        border: "1px solid var(--line-1)",
        borderRadius: "var(--r-2)",
      } satisfies Partial<CSSStyleDeclaration>);

      const tag = document.createElement("span");
      tag.textContent = d.label;
      tag.title = d.kind;
      Object.assign(tag.style, {
        fontFamily: "var(--ff-mono)",
        fontSize: "10px",
        color: "var(--text-3)",
        textTransform: "uppercase",
        letterSpacing: "0.1em",
        marginRight: "2px",
      } satisfies Partial<CSSStyleDeclaration>);
      bandWrap.appendChild(tag);

      const freq = makeNumberInput();
      freq.min = "20";
      freq.max = "20000";
      freq.step = "10";
      freq.value = String(d.freq);
      freq.title = "Hz";
      bandWrap.appendChild(freq);

      const gain = makeNumberInput();
      gain.min = "-24";
      gain.max = "24";
      gain.step = "0.5";
      gain.value = "0";
      gain.title = "dB";
      bandWrap.appendChild(gain);
      const gainLab = document.createElement("span");
      gainLab.textContent = "dB";
      gainLab.style.fontSize = "11px";
      gainLab.style.color = "var(--text-4)";
      bandWrap.appendChild(gainLab);

      const q = makeNumberInput();
      q.min = "0.1";
      q.max = "10";
      q.step = "0.1";
      q.value = String(d.q);
      q.title = "Q";
      bandWrap.appendChild(q);
      const qLab = document.createElement("span");
      qLab.textContent = "Q";
      qLab.style.fontSize = "11px";
      qLab.style.color = "var(--text-4)";
      bandWrap.appendChild(qLab);

      ui.fxParamsRow.appendChild(bandWrap);
      bands.push({ freq, gain, q, kind: d.kind });
    }

    eqInputs = bands;
  };

  const refreshNoiseProfiles = async (): Promise<void> => {
    try {
      noiseProfiles = await client.listNoiseProfiles();
    } catch {
      noiseProfiles = [];
    }
    if (nrInputs) populateNoiseProfilePicker();
  };

  const populateNoiseProfilePicker = (): void => {
    if (!nrInputs) return;
    const sel = nrInputs.profile;
    sel.innerHTML = "";
    if (noiseProfiles.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "(no profiles — capture one first)";
      opt.disabled = true;
      sel.appendChild(opt);
      return;
    }
    for (const p of noiseProfiles) {
      const opt = document.createElement("option");
      opt.value = p.id;
      const src = sources.find((s) => s.id === p.sourceId);
      opt.textContent = `${p.name} (${src?.name ?? p.sourceId})`;
      sel.appendChild(opt);
    }
  };

  const buildNrInputs = (): void => {
    ui.fxParamsRow.innerHTML = "";
    currentFxInputs = {};
    autotuneInputs = null;
    eqInputs = null;
    generateInputs = null;

    const modeWrap = document.createElement("label");
    Object.assign(modeWrap.style, wrapStyle);
    modeWrap.appendChild(document.createTextNode("mode"));
    const mode = document.createElement("select");
    Object.assign(mode.style, selStyle);
    for (const m of ["Apply", "Capture"]) {
      const o = document.createElement("option");
      o.value = m;
      o.textContent = m;
      mode.appendChild(o);
    }
    modeWrap.appendChild(mode);

    // Capture-mode controls.
    const captureRow = document.createElement("span");
    Object.assign(captureRow.style, { display: "none", gap: "6px", alignItems: "center" } satisfies Partial<CSSStyleDeclaration>);
    const nameWrap = document.createElement("label");
    Object.assign(nameWrap.style, wrapStyle);
    nameWrap.appendChild(document.createTextNode("name"));
    const captureName = document.createElement("input");
    captureName.type = "text";
    captureName.value = "noise";
    captureName.title = "Profile name";
    Object.assign(captureName.style, {
      width: "100px",
      padding: "2px 4px",
      background: "var(--bg-sunken)",
      color: "var(--text-2)",
      border: "1px solid var(--line-2)",
      borderRadius: "var(--r-2)",
      fontSize: "12px",
    } satisfies Partial<CSSStyleDeclaration>);
    nameWrap.appendChild(captureName);
    const captureHint = document.createElement("span");
    captureHint.textContent = "select a region of pure noise, then Apply";
    captureHint.style.color = "var(--text-4)";
    captureHint.style.fontSize = "11px";
    captureRow.appendChild(nameWrap);
    captureRow.appendChild(captureHint);

    // Apply-mode controls.
    const applyRow = document.createElement("span");
    Object.assign(applyRow.style, { display: "inline-flex", gap: "6px", alignItems: "center" } satisfies Partial<CSSStyleDeclaration>);
    const profWrap = document.createElement("label");
    Object.assign(profWrap.style, wrapStyle);
    profWrap.appendChild(document.createTextNode("profile"));
    const profile = document.createElement("select");
    Object.assign(profile.style, selStyle);
    profWrap.appendChild(profile);

    const amountWrap = document.createElement("label");
    Object.assign(amountWrap.style, wrapStyle);
    amountWrap.appendChild(document.createTextNode("amount dB"));
    const amountDb = makeNumberInput();
    amountDb.min = "-30";
    amountDb.max = "0";
    amountDb.step = "0.5";
    amountDb.value = "-12";
    amountDb.title = "How aggressively to subtract the noise spectrum";
    amountWrap.appendChild(amountDb);

    const floorWrap = document.createElement("label");
    Object.assign(floorWrap.style, wrapStyle);
    floorWrap.appendChild(document.createTextNode("floor dB"));
    const floorDb = makeNumberInput();
    floorDb.min = "-90";
    floorDb.max = "-10";
    floorDb.step = "1";
    floorDb.value = "-30";
    floorDb.title = "Lowest gain a bin can be reduced to (musical-noise control)";
    floorWrap.appendChild(floorDb);

    const overWrap = document.createElement("label");
    Object.assign(overWrap.style, wrapStyle);
    overWrap.appendChild(document.createTextNode("oversub"));
    const oversub = makeNumberInput();
    oversub.min = "1";
    oversub.max = "4";
    oversub.step = "0.1";
    oversub.value = "1.5";
    oversub.title = "Over-subtraction factor (>1 reduces tonal residue)";
    overWrap.appendChild(oversub);

    applyRow.appendChild(profWrap);
    applyRow.appendChild(amountWrap);
    applyRow.appendChild(floorWrap);
    applyRow.appendChild(overWrap);

    ui.fxParamsRow.appendChild(modeWrap);
    ui.fxParamsRow.appendChild(captureRow);
    ui.fxParamsRow.appendChild(applyRow);

    nrInputs = {
      mode,
      captureRow,
      captureName,
      applyRow,
      profile,
      amountDb,
      floorDb,
      oversub,
    };

    mode.addEventListener("change", () => {
      const isCapture = mode.value === "Capture";
      captureRow.style.display = isCapture ? "inline-flex" : "none";
      applyRow.style.display = isCapture ? "none" : "inline-flex";
    });

    void refreshNoiseProfiles();
  };

  const buildGenerateInputs = (): void => {
    ui.fxParamsRow.innerHTML = "";
    currentFxInputs = {};
    autotuneInputs = null;
    eqInputs = null;

    const modeWrap = document.createElement("label");
    Object.assign(modeWrap.style, wrapStyle);
    modeWrap.appendChild(document.createTextNode("kind"));
    const mode = document.createElement("select");
    Object.assign(mode.style, selStyle);
    for (const m of ["Silence", "Tone", "Noise"]) {
      const o = document.createElement("option");
      o.value = m;
      o.textContent = m;
      mode.appendChild(o);
    }
    mode.value = "Tone";
    modeWrap.appendChild(mode);

    // Tone-only controls.
    const toneRow = document.createElement("span");
    Object.assign(toneRow.style, { display: "inline-flex", gap: "6px" } satisfies Partial<CSSStyleDeclaration>);
    const shapeWrap = document.createElement("label");
    Object.assign(shapeWrap.style, wrapStyle);
    shapeWrap.appendChild(document.createTextNode("shape"));
    const shape = document.createElement("select");
    Object.assign(shape.style, selStyle);
    for (const s of ["Sine", "Triangle", "Saw", "Square"]) {
      const o = document.createElement("option");
      o.value = s;
      o.textContent = s;
      shape.appendChild(o);
    }
    shapeWrap.appendChild(shape);
    const freqWrap = document.createElement("label");
    Object.assign(freqWrap.style, wrapStyle);
    freqWrap.appendChild(document.createTextNode("Hz"));
    const freq = makeNumberInput();
    freq.min = "20";
    freq.max = "20000";
    freq.step = "10";
    freq.value = "440";
    freqWrap.appendChild(freq);
    const ampWrap = document.createElement("label");
    Object.assign(ampWrap.style, wrapStyle);
    ampWrap.appendChild(document.createTextNode("dB"));
    const toneAmp = makeNumberInput();
    toneAmp.min = "-60";
    toneAmp.max = "0";
    toneAmp.step = "0.5";
    toneAmp.value = "-6";
    ampWrap.appendChild(toneAmp);
    toneRow.appendChild(shapeWrap);
    toneRow.appendChild(freqWrap);
    toneRow.appendChild(ampWrap);

    // Noise-only controls.
    const noiseRow = document.createElement("span");
    Object.assign(noiseRow.style, { display: "none", gap: "6px" } satisfies Partial<CSSStyleDeclaration>);
    const colorWrap = document.createElement("label");
    Object.assign(colorWrap.style, wrapStyle);
    colorWrap.appendChild(document.createTextNode("color"));
    const noiseColor = document.createElement("select");
    Object.assign(noiseColor.style, selStyle);
    for (const c of ["White", "Pink", "Brown"]) {
      const o = document.createElement("option");
      o.value = c;
      o.textContent = c;
      noiseColor.appendChild(o);
    }
    colorWrap.appendChild(noiseColor);
    const noiseAmpWrap = document.createElement("label");
    Object.assign(noiseAmpWrap.style, wrapStyle);
    noiseAmpWrap.appendChild(document.createTextNode("dB"));
    const noiseAmp = makeNumberInput();
    noiseAmp.min = "-60";
    noiseAmp.max = "0";
    noiseAmp.step = "0.5";
    noiseAmp.value = "-12";
    noiseAmpWrap.appendChild(noiseAmp);
    noiseRow.appendChild(colorWrap);
    noiseRow.appendChild(noiseAmpWrap);

    // Length applies to all kinds.
    const lengthWrap = document.createElement("label");
    Object.assign(lengthWrap.style, wrapStyle);
    lengthWrap.appendChild(document.createTextNode("ms"));
    const length = makeNumberInput();
    length.min = "1";
    length.max = "60000";
    length.step = "10";
    length.value = "500";
    lengthWrap.appendChild(length);

    const hint = document.createElement("span");
    hint.textContent = "inserts at selection start, or end of source";
    hint.style.color = "var(--text-4)";
    hint.style.fontSize = "11px";

    ui.fxParamsRow.appendChild(modeWrap);
    ui.fxParamsRow.appendChild(toneRow);
    ui.fxParamsRow.appendChild(noiseRow);
    ui.fxParamsRow.appendChild(lengthWrap);
    ui.fxParamsRow.appendChild(hint);

    generateInputs = {
      mode,
      toneRow,
      shape,
      freq,
      toneAmp,
      noiseRow,
      noiseColor,
      noiseAmp,
      length,
    };

    mode.addEventListener("change", () => {
      const m = mode.value;
      toneRow.style.display = m === "Tone" ? "inline-flex" : "none";
      noiseRow.style.display = m === "Noise" ? "inline-flex" : "none";
    });
  };

  const buildFxParamInputs = (def: FxDef): void => {
    ui.fxParamsRow.innerHTML = "";
    currentFxInputs = {};
    for (const p of def.params) {
      const wrap = document.createElement("label");
      Object.assign(wrap.style, wrapStyle);
      const lab = document.createElement("span");
      lab.textContent = p.label;
      wrap.appendChild(lab);
      if (p.kind === "number") {
        const input = makeNumberInput();
        input.min = String(p.min);
        input.max = String(p.max);
        input.step = String(p.step);
        input.value = String(p.default);
        wrap.appendChild(input);
        currentFxInputs[p.key] = input;
      } else {
        const sel = document.createElement("select");
        Object.assign(sel.style, selStyle);
        for (const o of p.options) {
          const op = document.createElement("option");
          op.value = o;
          op.textContent = o;
          sel.appendChild(op);
        }
        sel.value = p.default;
        wrap.appendChild(sel);
        currentFxInputs[p.key] = sel;
      }
      ui.fxParamsRow.appendChild(wrap);
    }
  };

  buildFxParamInputs(fxDefs[0]!);

  const syncFxApplyEnabled = (): void => {
    ui.fxApplyBtn.disabled = currentSourceId === null;
  };

  ui.fxKindSelect.addEventListener("change", () => {
    const v = ui.fxKindSelect.value;
    // Reset every special-case state up front; whichever builder runs
    // below sets its own back to a non-null value.
    autotuneInputs = null;
    eqInputs = null;
    generateInputs = null;
    nrInputs = null;
    if (v === "Autotune") {
      buildAutotuneInputs();
    } else if (v === "Eq") {
      buildEqInputs();
    } else if (v === "Generate") {
      buildGenerateInputs();
    } else if (v === "NoiseReduce") {
      buildNrInputs();
    } else {
      const def = fxDefs.find((d) => d.id === v);
      if (def) buildFxParamInputs(def);
    }
    syncFxApplyEnabled();
  });

  /** Build a Reference-mode pitch contour aligned source-start to
   *  source-start: hop `i` of the input maps to hop `offsetHops + i` of
   *  the reference, where `offsetHops = floor(inputRange.start / hop)`.
   *  Frames past the reference's end stay at 0 Hz (unvoiced → no retune). */
  const buildReferenceContour = async (
    refSourceId: string,
    inputRange: { start: number; end: number },
    hopSamples: number,
    windowSamples: number,
  ): Promise<number[]> => {
    const refInfo = sources.find((s) => s.id === refSourceId);
    if (!refInfo) return [];
    const refContour = await client.detectPitchContour(
      refSourceId,
      0,
      refInfo.frames,
      hopSamples,
      windowSamples,
    );
    const inputFrames = inputRange.end - inputRange.start;
    const numHops = Math.ceil(inputFrames / hopSamples) + 1;
    const offsetHops = Math.floor(inputRange.start / hopSamples);
    const aligned: number[] = new Array(numHops).fill(0);
    for (let i = 0; i < numHops; i++) {
      aligned[i] = refContour[offsetHops + i] ?? 0;
    }
    return aligned;
  };

  ui.fxApplyBtn.addEventListener("click", async () => {
    if (!currentSourceId) return;

    // Generate is position-based (insert at `at` with `length`), not
    // range-based — it doesn't need a non-empty range and works fine on
    // an empty source. Handle it before the range check.
    if (ui.fxKindSelect.value === "Generate") {
      if (!generateInputs) return;
      const lengthMs = parseFloat(generateInputs.length.value);
      if (!Number.isFinite(lengthMs) || lengthMs <= 0) {
        ui.status.textContent = "Generate: length must be positive";
        return;
      }
      const lengthFrames = Math.max(1, Math.round((lengthMs / 1000) * sourceSampleRate));
      // Insert at selection start when one exists; otherwise append at
      // the source's current end. The Generate dsp path validates that
      // `at` is in [0, total_frames].
      const at = selection ? selection.inFrame : sourceFrameCount;
      let params: unknown;
      const m = generateInputs.mode.value;
      if (m === "Silence") {
        params = "Silence";
      } else if (m === "Tone") {
        const shape = generateInputs.shape.value;
        const f = parseFloat(generateInputs.freq.value);
        const a = parseFloat(generateInputs.toneAmp.value);
        params = {
          Tone: {
            shape,
            frequency_hz: Number.isFinite(f) ? f : 440,
            amplitude_db: Number.isFinite(a) ? a : -6,
          },
        };
      } else {
        const a = parseFloat(generateInputs.noiseAmp.value);
        params = {
          Noise: {
            color: generateInputs.noiseColor.value,
            amplitude_db: Number.isFinite(a) ? a : -12,
          },
        };
      }
      ui.fxApplyBtn.disabled = true;
      const lenBefore = sourceFrameCount;
      try {
        await client.applyOp(
          currentSourceId,
          JSON.stringify({ Generate: { at, length: lengthFrames, params } }),
        );
        await refreshLibrary();
        await loadSource(currentSourceId);
        opts.onSourceImported?.();
        ui.status.textContent = `Generate ${m} applied (length ${lenBefore.toLocaleString()} → ${sourceFrameCount.toLocaleString()})`;
      } catch (err) {
        ui.status.textContent = `Generate failed: ${String(err)}`;
      } finally {
        syncFxApplyEnabled();
      }
      return;
    }

    // Every other op processes a range — selection if any, else whole source.
    const range = selection
      ? { start: selection.inFrame, end: selection.outFrame }
      : { start: 0, end: sourceFrameCount };
    if (range.end <= range.start) {
      ui.status.textContent = "fx: empty range";
      return;
    }

    if (ui.fxKindSelect.value === "NoiseReduce") {
      if (!nrInputs) return;
      const FFT_SIZE = 2048;
      ui.fxApplyBtn.disabled = true;
      try {
        if (nrInputs.mode.value === "Capture") {
          if (!selection) {
            ui.status.textContent = "NR capture: select a region of pure noise first";
            return;
          }
          const name = nrInputs.captureName.value.trim() || "noise";
          // Generate a unique-ish profile id from a timestamp; the engine
          // doesn't care about format, just uniqueness within the project.
          const profileId = `np_${Date.now().toString(36)}`;
          await client.captureNoiseProfile(
            currentSourceId,
            range.start,
            range.end,
            name,
            profileId,
            FFT_SIZE,
          );
          await refreshNoiseProfiles();
          // Switch the user to Apply mode and select their newly-captured
          // profile so a follow-up Apply is one click away.
          nrInputs.mode.value = "Apply";
          nrInputs.mode.dispatchEvent(new Event("change"));
          nrInputs.profile.value = profileId;
          ui.status.textContent = `NR profile captured (${name})`;
        } else {
          const profileId = nrInputs.profile.value;
          if (!profileId) {
            ui.status.textContent = "NR apply: capture a profile first";
            return;
          }
          const amount = parseFloat(nrInputs.amountDb.value);
          const floor = parseFloat(nrInputs.floorDb.value);
          const over = parseFloat(nrInputs.oversub.value);
          await client.applyOp(
            currentSourceId,
            JSON.stringify({
              NoiseReduce: {
                range,
                profile: profileId,
                params: {
                  amount_db: Number.isFinite(amount) ? amount : -12,
                  floor_db: Number.isFinite(floor) ? floor : -30,
                  oversubtraction: Number.isFinite(over) ? over : 1.5,
                  attack_ms: 5.0,
                  release_ms: 50.0,
                  freq_smoothing: 1.0,
                  fft_size: FFT_SIZE,
                },
              },
            }),
          );
          await refreshAfterEdit();
          opts.onSourceImported?.();
          ui.status.textContent = `NR applied (${nrInputs.profile.options[nrInputs.profile.selectedIndex]?.textContent ?? profileId})`;
        }
      } catch (err) {
        ui.status.textContent = `NR failed: ${String(err)}`;
      } finally {
        syncFxApplyEnabled();
      }
      return;
    }

    if (ui.fxKindSelect.value === "Eq") {
      if (!eqInputs) return;
      const bands = eqInputs
        .map((b) => ({
          kind: b.kind,
          frequency_hz: parseFloat(b.freq.value),
          gain_db: parseFloat(b.gain.value),
          q: parseFloat(b.q.value),
          enabled: true,
        }))
        // Drop bands with 0 dB gain so we don't pay biquad cost for a no-op.
        .filter((b) => Number.isFinite(b.gain_db) && Math.abs(b.gain_db) > 1e-3);
      if (bands.length === 0) {
        ui.status.textContent = "EQ: every band is at 0 dB";
        return;
      }
      ui.fxApplyBtn.disabled = true;
      try {
        await client.applyOp(
          currentSourceId,
          JSON.stringify({ Eq: { range, params: { bands } } }),
        );
        await refreshAfterEdit();
        opts.onSourceImported?.();
        ui.status.textContent = `EQ applied (${bands.length} band${bands.length === 1 ? "" : "s"})`;
      } catch (err) {
        ui.status.textContent = `EQ failed: ${String(err)}`;
      } finally {
        syncFxApplyEnabled();
      }
      return;
    }

    if (ui.fxKindSelect.value === "Autotune") {
      if (!autotuneInputs) return;
      const retuneMs = parseFloat(autotuneInputs.retune.value);
      const preserveFormants = autotuneInputs.formant.checked;
      ui.fxApplyBtn.disabled = true;
      try {
        let target: unknown;
        if (autotuneInputs.mode.value === "Scale") {
          const scale = autotuneInputs.scale.value;
          const keyPc = parseInt(autotuneInputs.key.value, 10) || 0;
          target = { Scale: { scale, key_pc: keyPc } };
        } else {
          const refId = autotuneInputs.refSource.value;
          if (!refId) {
            ui.status.textContent = "autotune: pick a reference source";
            return;
          }
          // 25 ms hop, 50 ms window — same family as the engine's autotune
          // analysis windows, so contour alignment is straightforward.
          const hopSamples = Math.max(1, Math.round((sourceSampleRate * 25) / 1000));
          const windowSamples = Math.max(64, Math.round((sourceSampleRate * 50) / 1000));
          const contour = await buildReferenceContour(refId, range, hopSamples, windowSamples);
          target = { Reference: { contour_hz: contour, hop_samples: hopSamples } };
        }
        const opJson = JSON.stringify({
          Autotune: {
            range,
            params: {
              target,
              retune_ms: Number.isFinite(retuneMs) ? retuneMs : 0,
              preserve_formants: preserveFormants,
            },
          },
        });
        await client.applyOp(currentSourceId, opJson);
        await refreshAfterEdit();
        ui.status.textContent = `Autotune applied (${autotuneInputs.mode.value})`;
      } catch (err) {
        ui.status.textContent = `autotune failed: ${String(err)}`;
      } finally {
        syncFxApplyEnabled();
      }
      return;
    }

    const def = fxDefs.find((d) => d.id === ui.fxKindSelect.value);
    if (!def) return;
    const vals: Record<string, string> = {};
    for (const [k, el] of Object.entries(currentFxInputs)) vals[k] = el.value;
    const op = def.build(range, vals);
    ui.fxApplyBtn.disabled = true;
    const lenBefore = sourceFrameCount;
    try {
      await client.applyOp(currentSourceId, JSON.stringify(op));
      // Generic length-change detection: anything that grew or shrank
      // the source (Trim/Cut/TimeStretch, but also Delay's tail
      // extension and similar) needs a full reload so the viewport,
      // selection, and cached frame count line up with the new buffer.
      await refreshLibrary();
      const fresh = sources.find((s) => s.id === currentSourceId);
      const lenAfter = fresh?.frames ?? lenBefore;
      if (lenAfter !== lenBefore) {
        await loadSource(currentSourceId);
        opts.onSourceImported?.();
        ui.status.textContent =
          def.id === "TimeStretch"
            ? `Time Stretch ×${num(vals, "ratio", 1.0).toFixed(2)} applied`
            : `${def.label} applied (length ${lenBefore.toLocaleString()} → ${lenAfter.toLocaleString()})`;
        return;
      }
      await refreshAfterEdit();
      // Source content changed — let the arranger refresh its peak cache
      // and (for length-changing ops we don't special-case here) its
      // cached source.frames.
      opts.onSourceImported?.();
      ui.status.textContent = `${def.label} applied`;
    } catch (err) {
      ui.status.textContent = `${def.label} failed: ${String(err)}`;
    } finally {
      syncFxApplyEnabled();
    }
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

  ui.trimBtn.addEventListener("click", async () => {
    if (!currentSourceId || !selection) return;
    const range = { start: selection.inFrame, end: selection.outFrame };
    if (range.end <= range.start) return;
    ui.trimBtn.disabled = true;
    try {
      await client.applyOp(currentSourceId, JSON.stringify({ Trim: { range } }));
      // Trim shrinks the source — reload from the engine so frame count,
      // viewport, and selection all align with the new length.
      await refreshLibrary();
      await loadSource(currentSourceId);
      // Tell the arranger so its cached source.frames updates — otherwise
      // a subsequent drop-onto-track would size the clip against the old
      // length and reference frames past the trimmed end.
      opts.onSourceImported?.();
      ui.status.textContent = `trimmed to selection (${range.end - range.start} frames)`;
    } catch (err) {
      ui.status.textContent = `trim failed: ${String(err)}`;
    } finally {
      syncSelectionInputs();
    }
  });

  ui.undoBtn.addEventListener("click", async () => {
    if (!currentSourceId) return;
    ui.undoBtn.disabled = true;
    try {
      const did = await client.undo(currentSourceId);
      ui.status.textContent = did ? "undone" : "nothing to undo";
      // Frame count may have changed (undoing a Trim/Cut) so reload from
      // the engine rather than just redrawing the waveform in place.
      if (did) {
        await refreshLibrary();
        await loadSource(currentSourceId);
        opts.onSourceImported?.();
      } else {
        await refreshAfterEdit();
      }
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
      if (did) {
        await refreshLibrary();
        await loadSource(currentSourceId);
        opts.onSourceImported?.();
      } else {
        await refreshAfterEdit();
      }
    } catch (err) {
      ui.status.textContent = `redo failed: ${String(err)}`;
    } finally {
      ui.redoBtn.disabled = currentSourceId === null;
    }
  });

  syncSelectionInputs();
  syncZoomButtons();
  syncScrollbar();
  syncFxApplyEnabled();
  await refreshLibrary();

  const reset = async (): Promise<void> => {
    await playback.stop();
    stopPlayheadLoop();
    currentSourceId = null;
    sourceFrameCount = 0;
    sourceSampleRate = 0;
    viewport = { startFrame: 0, endFrame: 0 };
    cachedPeaks = null;
    setSelection(null);
    setTransportEnabled(false);
    ui.undoBtn.disabled = true;
    ui.redoBtn.disabled = true;
    ui.duplicateBtn.disabled = true;
    ui.fileInput.value = "";
    ui.status.textContent = "no source loaded";
    const ctx = ui.canvas.getContext("2d");
    if (ctx) ctx.clearRect(0, 0, ui.canvas.width, ui.canvas.height);
    redrawOverlay();
    syncScrollbar();
    syncZoomButtons();
    syncFxApplyEnabled();
    await refreshLibrary();
  };

  return { reset, refreshLibrary };
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
  libraryList: HTMLDivElement;
  duplicateBtn: HTMLButtonElement;
  newSourceBtn: HTMLButtonElement;
  fxKindSelect: HTMLSelectElement;
  fxParamsRow: HTMLDivElement;
  fxApplyBtn: HTMLButtonElement;
  trimBtn: HTMLButtonElement;
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

  // ---- top "Open WAV" row ----
  const fileRow = document.createElement("label");
  fileRow.style.display = "flex";
  fileRow.style.alignItems = "center";
  fileRow.style.gap = "8px";
  fileRow.textContent = "Open WAV: ";
  const fileInput = document.createElement("input");
  fileInput.type = "file";
  fileInput.accept = ".wav,audio/wav,audio/x-wav";
  fileInput.multiple = true;
  fileRow.appendChild(fileInput);
  root.appendChild(fileRow);

  // ---- horizontal split: library | main ----
  // flex: 1 + minHeight: 0 lets the split absorb the editor's vertical
  // space and shrink past content height — without those, a long source
  // list pushes the FX/transport rows off the bottom.
  const split = document.createElement("div");
  Object.assign(split.style, {
    display: "flex",
    flexDirection: "row",
    gap: "12px",
    alignItems: "stretch",
    flex: "1 1 auto",
    minHeight: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  root.appendChild(split);

  // ---- library pane (left) ----
  const libraryPane = document.createElement("div");
  Object.assign(libraryPane.style, {
    width: "240px",
    flex: "0 0 240px",
    display: "flex",
    flexDirection: "column",
    background: "#101010",
    border: "1px solid #2a2a2a",
    boxSizing: "border-box",
  } satisfies Partial<CSSStyleDeclaration>);

  const libraryHeader = document.createElement("div");
  Object.assign(libraryHeader.style, {
    padding: "6px 8px",
    fontSize: "12px",
    fontWeight: "600",
    color: "#cfcfcf",
    borderBottom: "1px solid #2a2a2a",
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
  } satisfies Partial<CSSStyleDeclaration>);
  const libraryTitle = document.createElement("span");
  libraryTitle.textContent = "Library";
  const libraryActions = document.createElement("span");
  Object.assign(libraryActions.style, {
    display: "inline-flex",
    gap: "4px",
  } satisfies Partial<CSSStyleDeclaration>);
  const newSourceBtn = document.createElement("button");
  newSourceBtn.type = "button";
  newSourceBtn.textContent = "+ New";
  newSourceBtn.title = "Create a new blank stereo source (asks for length in seconds)";
  Object.assign(newSourceBtn.style, btnStyle(), {
    padding: "2px 8px",
    fontSize: "11px",
  } satisfies Partial<CSSStyleDeclaration>);
  const duplicateBtn = document.createElement("button");
  duplicateBtn.type = "button";
  duplicateBtn.textContent = "Duplicate";
  duplicateBtn.title = "Duplicate the current source (creates an independent copy)";
  duplicateBtn.disabled = true;
  Object.assign(duplicateBtn.style, btnStyle(), {
    padding: "2px 8px",
    fontSize: "11px",
  } satisfies Partial<CSSStyleDeclaration>);
  libraryActions.appendChild(newSourceBtn);
  libraryActions.appendChild(duplicateBtn);
  libraryHeader.appendChild(libraryTitle);
  libraryHeader.appendChild(libraryActions);
  libraryPane.appendChild(libraryHeader);

  const libraryList = document.createElement("div");
  Object.assign(libraryList.style, {
    flex: "1 1 auto",
    overflowY: "auto",
    // Has to be 0 (not a positive floor) so the flex parent can shrink
    // it below content height; the inner overflowY then handles scroll.
    minHeight: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  libraryPane.appendChild(libraryList);

  const libraryHint = document.createElement("div");
  libraryHint.textContent = "click = load · double-click name = rename";
  Object.assign(libraryHint.style, {
    padding: "4px 8px",
    fontSize: "10px",
    color: "#777",
    borderTop: "1px solid #2a2a2a",
  } satisfies Partial<CSSStyleDeclaration>);
  libraryPane.appendChild(libraryHint);

  split.appendChild(libraryPane);

  // ---- main pane (right) ----
  const main = document.createElement("div");
  Object.assign(main.style, {
    display: "flex",
    flexDirection: "column",
    gap: "12px",
    flex: "1 1 auto",
    minWidth: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  split.appendChild(main);

  // Transport
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
  main.appendChild(transport);

  // Waveform canvas
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
  main.appendChild(canvasWrap);

  const scrollbar = document.createElement("input");
  scrollbar.type = "range";
  scrollbar.disabled = true;
  Object.assign(scrollbar.style, {
    width: "100%",
    margin: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  main.appendChild(scrollbar);

  // Zoom row
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
  main.appendChild(zoomRow);

  // FX row: kind select + params + apply + undo/redo
  const fxRow = document.createElement("div");
  Object.assign(fxRow.style, {
    display: "flex",
    alignItems: "center",
    gap: "8px",
    fontSize: "13px",
    flexWrap: "wrap",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxLabel = document.createElement("span");
  fxLabel.textContent = "Effect:";
  const fxKindSelect = document.createElement("select");
  Object.assign(fxKindSelect.style, {
    padding: "4px 6px",
    background: "#0c0c0c",
    color: "#d8d8d8",
    border: "1px solid #3a3a3a",
    fontSize: "13px",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxParamsRow = document.createElement("div");
  Object.assign(fxParamsRow.style, {
    display: "inline-flex",
    alignItems: "center",
    gap: "8px",
    flexWrap: "wrap",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxApplyBtn = document.createElement("button");
  fxApplyBtn.type = "button";
  fxApplyBtn.textContent = "Apply";
  fxApplyBtn.disabled = true;
  Object.assign(fxApplyBtn.style, btnStyle(), {
    padding: "4px 10px",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxSep = document.createElement("span");
  fxSep.textContent = "·";
  fxSep.style.color = "#555";
  const trimBtn = document.createElement("button");
  trimBtn.textContent = "Trim";
  trimBtn.type = "button";
  trimBtn.disabled = true;
  trimBtn.title = "Keep only the selection, discard everything outside";
  Object.assign(trimBtn.style, btnStyle(), {
    padding: "4px 10px",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxSep2 = document.createElement("span");
  fxSep2.textContent = "·";
  fxSep2.style.color = "#555";
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
  const fxHint = document.createElement("span");
  fxHint.textContent = "(applies to selection if any, else whole source)";
  fxHint.style.color = "#9a9a9a";
  fxRow.appendChild(fxLabel);
  fxRow.appendChild(fxKindSelect);
  fxRow.appendChild(fxParamsRow);
  fxRow.appendChild(fxApplyBtn);
  fxRow.appendChild(fxSep);
  fxRow.appendChild(trimBtn);
  fxRow.appendChild(fxSep2);
  fxRow.appendChild(undoBtn);
  fxRow.appendChild(redoBtn);
  fxRow.appendChild(fxHint);
  main.appendChild(fxRow);

  // Selection inputs
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
  main.appendChild(selRow);

  const status = document.createElement("div");
  status.textContent = "no source loaded";
  status.style.fontSize = "12px";
  status.style.color = "#9a9a9a";
  status.style.fontFamily = "ui-monospace, monospace";
  main.appendChild(status);

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
    libraryList,
    duplicateBtn,
    newSourceBtn,
    fxKindSelect,
    fxParamsRow,
    fxApplyBtn,
    trimBtn,
    undoBtn,
    redoBtn,
  };
}

export function btnStyle(): Partial<CSSStyleDeclaration> {
  return {
    padding: "6px 14px",
    background: "var(--bg-2)",
    color: "var(--text-2)",
    border: "1px solid var(--line-2)",
    borderRadius: "var(--r-2)",
    cursor: "pointer",
    fontFamily: "inherit",
  };
}
