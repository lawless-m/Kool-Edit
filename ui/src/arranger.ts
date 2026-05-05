// Multitrack arrangement tab. Sources column on the left, track lanes on the
// right with a shared horizontal scroll, beat/bar grid, and a ruler row.
// Clip positions are stored in frames in the engine; the UI snaps to beats
// when the grid mode is `beats`. Changing BPM or time-sig re-positions every
// clip so the beat alignment is preserved (a clip "at bar 3 beat 2" stays
// "at bar 3 beat 2" — the time changes, the beat doesn't).

import type { ClipInfo, EngineClient, SourceInfo, TrackInfo } from "./engine/client";
import { btnStyle } from "./editor";

const DEFAULT_PIXELS_PER_SECOND = 60;
const MIN_PIXELS_PER_SECOND = 5;
const MAX_PIXELS_PER_SECOND = 1000;
const ZOOM_FACTOR = 1.5;
const LANE_HEIGHT = 64;
const RULER_HEIGHT = 24;
const HEADER_COL_WIDTH = 180;
const MIN_LANE_PX = 800;

type GridMode = "beats" | "time";

export interface ArrangerHandle {
  refresh(): Promise<void>;
}

export async function mountArranger(
  root: HTMLElement,
  client: EngineClient,
): Promise<ArrangerHandle> {
  const ui = buildUi(root);

  let projectSr = await client.projectSampleRate();
  let sources: SourceInfo[] = [];
  let tracks: TrackInfo[] = [];
  const clipsByTrack = new Map<number, ClipInfo[]>();
  // Pre-baked peak summaries (one Float32Array per source, length = 2 × COLS)
  // so clip rendering doesn't pay a round trip per redraw. Refreshed on every
  // refresh() call so destructive edits in the editor tab show up here.
  const sourcePeaks = new Map<string, Float32Array>();
  const PEAK_COLS = 2048;
  let stagedSourceId: string | null = null;
  let selectedClip: { trackId: number; clipId: number } | null = null;
  let audioCtx: AudioContext | null = null;
  let currentBufferSrc: AudioBufferSourceNode | null = null;

  // Transport / selection state. inFrame/outFrame are project-frame positions
  // (i.e. frames at projectSr). playheadFrame is where the next Play will
  // start when there's no selection. playSession describes an active play.
  let inFrame: number | null = null;
  let outFrame: number | null = null;
  let playheadFrame = 0;
  let playSession: {
    startCtxTime: number;
    offsetSec: number;
    loop: boolean;
    loopStartSec: number;
    loopEndSec: number;
    bufferDuration: number;
  } | null = null;
  let playRaf: number | null = null;
  let dragState: { anchorFrame: number; moved: boolean } | null = null;
  let clipDragState: {
    trackId: number;
    clipId: number;
    elt: HTMLElement;
    startMouseX: number;
    startPositionFrame: number;
    currentPositionFrame: number;
    moved: boolean;
  } | null = null;
  let edgeDragState: {
    edge: "in" | "out";
    startMouseX: number;
    startFrame: number;
  } | null = null;
  let binHovered = false;

  // Grid state. BPM means quarter-note BPM regardless of time-sig denominator
  // (the universal convention). secondsPerBeat() factors in the denominator.
  let bpm = 120;
  let beatsPerBar = 4;
  let beatUnit = 4; // 4 = quarter, 8 = eighth, etc.
  let gridMode: GridMode = "beats";
  let pixelsPerSecond = DEFAULT_PIXELS_PER_SECOND;

  const setStatus = (msg: string): void => {
    ui.status.textContent = msg;
  };

  const rerenderIfPlaying = (): void => {
    if (playSession) {
      const wasLooping = playSession.loop;
      void startPlayback(wasLooping);
    }
  };

  const secondsPerBeat = (): number => (60 / bpm) * (4 / beatUnit);
  const secondsPerBar = (): number => secondsPerBeat() * beatsPerBar;
  const pxPerBeat = (): number => secondsPerBeat() * pixelsPerSecond;
  const pxPerBar = (): number => pxPerBeat() * beatsPerBar;

  const framesToPx = (frames: number): number =>
    projectSr > 0 ? (frames / projectSr) * pixelsPerSecond : 0;

  const pxToFrames = (px: number): number =>
    projectSr > 0 ? Math.max(0, Math.round((px / pixelsPerSecond) * projectSr)) : 0;

  const framesToBeats = (frames: number): number =>
    projectSr > 0 ? frames / projectSr / secondsPerBeat() : 0;

  const beatsToFrames = (beats: number): number =>
    Math.max(0, Math.round(beats * secondsPerBeat() * projectSr));

  const snapFrames = (frames: number): number => {
    if (gridMode !== "beats") return frames;
    const beats = framesToBeats(frames);
    return beatsToFrames(Math.round(beats));
  };

  const projectLengthFrames = (): number => {
    let end = 0;
    for (const list of clipsByTrack.values()) {
      for (const c of list) end = Math.max(end, c.endPosition);
    }
    return end;
  };

  const refresh = async (): Promise<void> => {
    projectSr = await client.projectSampleRate();
    // Pick up persisted tempo so a loaded project keeps its BPM/time-sig.
    try {
      const t = await client.getTempo();
      bpm = t.bpm;
      beatsPerBar = t.beatsPerBar;
      beatUnit = t.beatUnit;
      ui.bpmInput.value = String(bpm);
      ui.beatsPerBarInput.value = String(beatsPerBar);
      ui.beatUnitInput.value = String(beatUnit);
    } catch {
      // ignore — older engine builds without getTempo
    }
    sources = await client.listSources();
    tracks = await client.listTracks();
    clipsByTrack.clear();
    for (const t of tracks) {
      clipsByTrack.set(t.id, await client.listClips(t.id));
    }
    // Refresh peak summaries for every source. Cheap per call; the engine's
    // cache does the heavy lifting.
    sourcePeaks.clear();
    for (const s of sources) {
      try {
        const peaks = await client.peakSummary(s.id, PEAK_COLS);
        sourcePeaks.set(s.id, peaks);
      } catch {
        // ignore — clip will render without a waveform
      }
    }
    if (selectedClip) {
      const list = clipsByTrack.get(selectedClip.trackId);
      if (!list || !list.some((c) => c.id === selectedClip!.clipId)) {
        selectedClip = null;
      }
    }
    drawSources();
    drawTracks();
    syncToolbar();
  };

  const syncToolbar = (): void => {
    // The bin is enabled when there's a selection (so clicking it works) AND
    // during clip drag (so it's a visible drop target). The drag path forces
    // it on directly; this covers the click-to-bin path.
    ui.deleteClipBtn.disabled = selectedClip === null && clipDragState === null;
    const noProject = projectLengthFrames() === 0;
    ui.playBtn.disabled = noProject;
    ui.loopBtn.disabled = noProject;
    syncFxPanel();
  };

  const setBinHover = (b: boolean): void => {
    if (binHovered === b) return;
    binHovered = b;
    ui.deleteClipBtn.style.background = b ? "#a83030" : "#2a2a2a";
    ui.deleteClipBtn.style.borderColor = b ? "#ff6060" : "#3a3a3a";
    ui.deleteClipBtn.style.color = b ? "#ffffff" : "#d8d8d8";
  };

  // ---- sources panel ----------------------------------------------------

  const drawSources = (): void => {
    ui.sourceList.innerHTML = "";
    if (sources.length === 0) {
      const empty = document.createElement("div");
      empty.textContent = "No sources yet. Load a WAV.";
      empty.style.color = "#7a7a7a";
      empty.style.fontSize = "12px";
      empty.style.padding = "8px";
      ui.sourceList.appendChild(empty);
      return;
    }
    for (const s of sources) {
      const row = document.createElement("div");
      const staged = stagedSourceId === s.id;
      Object.assign(row.style, {
        padding: "6px 8px",
        borderBottom: "1px solid #222",
        cursor: "pointer",
        background: staged ? "#2a3f2a" : "transparent",
        color: staged ? "#ffffff" : "#d8d8d8",
      } satisfies Partial<CSSStyleDeclaration>);
      const name = document.createElement("div");
      name.textContent = s.name;
      Object.assign(name.style, {
        fontSize: "12px",
        fontWeight: "500",
        whiteSpace: "nowrap",
        overflow: "hidden",
        textOverflow: "ellipsis",
      } satisfies Partial<CSSStyleDeclaration>);
      const meta = document.createElement("div");
      const seconds = s.sampleRate > 0 ? (s.frames / s.sampleRate).toFixed(2) : "?";
      meta.textContent = `${seconds}s · ${s.channels}ch`;
      Object.assign(meta.style, {
        color: "#888",
        fontSize: "10px",
        fontFamily: "ui-monospace, monospace",
      } satisfies Partial<CSSStyleDeclaration>);
      row.title = `${s.id} · ${s.sampleRate} Hz · ${s.channels}ch · ${s.frames} frames`;
      row.appendChild(name);
      row.appendChild(meta);
      row.addEventListener("click", () => {
        stagedSourceId = stagedSourceId === s.id ? null : s.id;
        drawSources();
        drawTracks();
        setStatus(stagedSourceId ? `staged ${s.name} — click a lane to place` : "");
      });
      ui.sourceList.appendChild(row);
    }
  };

  // ---- tracks panel -----------------------------------------------------

  const computeLaneWidth = (): number => {
    const projSec = projectLengthFrames() / Math.max(1, projectSr);
    // Trailer of a few bars (or 4 seconds in time mode) so the user can
    // place clips past the current end without immediately re-flowing.
    const trailerSec = gridMode === "beats" ? secondsPerBar() * 4 : 4;
    const laneSec = Math.max(8, projSec + trailerSec);
    return Math.max(MIN_LANE_PX, laneSec * pixelsPerSecond);
  };

  /** Build the CSS for a lane's background grid. Bar lines are heavier;
   *  beat lines are lighter. In time mode there's just one tick per second. */
  const laneGridStyle = (): { backgroundImage: string; backgroundSize: string } => {
    if (gridMode === "time") {
      const px = pixelsPerSecond;
      return {
        backgroundImage: `repeating-linear-gradient(to right,
          transparent 0,
          transparent ${px - 1}px,
          rgba(120,120,120,0.20) ${px - 1}px,
          rgba(120,120,120,0.20) ${px}px)`,
        backgroundSize: `${px}px 100%`,
      };
    }
    const beatPx = pxPerBeat();
    const barPx = pxPerBar();
    return {
      backgroundImage: [
        `repeating-linear-gradient(to right,
          transparent 0,
          transparent ${beatPx - 1}px,
          rgba(120,120,120,0.16) ${beatPx - 1}px,
          rgba(120,120,120,0.16) ${beatPx}px)`,
        `repeating-linear-gradient(to right,
          transparent 0,
          transparent ${barPx - 1}px,
          rgba(180,230,180,0.30) ${barPx - 1}px,
          rgba(180,230,180,0.30) ${barPx}px)`,
      ].join(", "),
      backgroundSize: `${beatPx}px 100%, ${barPx}px 100%`,
    };
  };

  const drawRuler = (laneWidth: number): HTMLElement => {
    const ruler = document.createElement("div");
    Object.assign(ruler.style, {
      position: "relative",
      height: `${RULER_HEIGHT}px`,
      width: `${laneWidth}px`,
      background: "#1a1a1a",
      borderBottom: "1px solid #2a2a2a",
      flexShrink: "0",
      fontSize: "10px",
      fontFamily: "ui-monospace, monospace",
      color: "#aaa",
      cursor: "text",
      userSelect: "none",
    } satisfies Partial<CSSStyleDeclaration>);
    ruler.addEventListener("mousedown", (ev) => {
      if (ev.button !== 0) return;
      ev.preventDefault();
      const rect = ruler.getBoundingClientRect();
      const x = ev.clientX - rect.left;
      let frame = pxToFrames(x);
      if (!ev.shiftKey) frame = snapFrames(frame);
      dragState = { anchorFrame: frame, moved: false };
    });

    const addTick = (x: number, label: string, strong: boolean): void => {
      const t = document.createElement("div");
      Object.assign(t.style, {
        position: "absolute",
        left: `${x}px`,
        top: "0",
        bottom: "0",
        width: "1px",
        background: strong ? "rgba(180,230,180,0.5)" : "rgba(120,120,120,0.4)",
      } satisfies Partial<CSSStyleDeclaration>);
      ruler.appendChild(t);
      if (label) {
        const lab = document.createElement("div");
        lab.textContent = label;
        Object.assign(lab.style, {
          position: "absolute",
          left: `${x + 3}px`,
          top: "4px",
          color: strong ? "#cfe6cf" : "#888",
          fontWeight: strong ? "600" : "400",
        } satisfies Partial<CSSStyleDeclaration>);
        ruler.appendChild(lab);
      }
    };

    if (gridMode === "beats") {
      const barPx = pxPerBar();
      const beatPx = pxPerBeat();
      const totalBars = Math.ceil(laneWidth / barPx);
      for (let bar = 0; bar <= totalBars; bar++) {
        const xBar = bar * barPx;
        addTick(xBar, `${bar + 1}`, true);
        for (let beat = 1; beat < beatsPerBar; beat++) {
          const xBeat = xBar + beat * beatPx;
          if (xBeat > laneWidth) break;
          addTick(xBeat, "", false);
        }
      }
    } else {
      const sec = pixelsPerSecond;
      const total = Math.ceil(laneWidth / sec);
      for (let s = 0; s <= total; s++) {
        const x = s * sec;
        const m = Math.floor(s / 60);
        const sm = s % 60;
        const label = `${m}:${sm.toString().padStart(2, "0")}`;
        addTick(x, label, s % 5 === 0);
      }
    }
    return ruler;
  };

  // ---- selection / playhead overlays -----------------------------------
  // Defined ahead of drawTracks because drawTracks calls them at the end of
  // each render. Hoisting via `function` would also work; using a function
  // expression here lets us share the closure cleanly.

  const hasSelection = (): boolean =>
    inFrame !== null && outFrame !== null && outFrame > inFrame;

  const drawSelection = (): void => {
    const o = ui.selectionOverlay;
    if (!hasSelection()) {
      o.style.display = "none";
      ui.inHandle.style.display = "none";
      ui.outHandle.style.display = "none";
      return;
    }
    const lo = framesToPx(inFrame!);
    const hi = framesToPx(outFrame!);
    o.style.display = "block";
    o.style.left = `${lo}px`;
    o.style.width = `${Math.max(1, hi - lo)}px`;
    ui.inHandle.style.display = "block";
    ui.inHandle.style.left = `${lo}px`;
    ui.outHandle.style.display = "block";
    ui.outHandle.style.left = `${hi}px`;
  };

  const drawPlayhead = (): void => {
    const o = ui.playheadOverlay;
    if (projectSr === 0) {
      o.style.display = "none";
      return;
    }
    o.style.display = "block";
    o.style.left = `${framesToPx(playheadFrame)}px`;
  };

  const drawTracks = (): void => {
    ui.headersCol.innerHTML = "";
    ui.lanesStack.innerHTML = "";

    const laneWidth = computeLaneWidth();
    ui.lanesContent.style.width = `${laneWidth}px`;
    ui.lanesStack.style.width = `${laneWidth}px`;

    // Ruler at the top of the lanes column. The headers column gets a
    // matching spacer so track headers line up with their lanes.
    const headerSpacer = document.createElement("div");
    Object.assign(headerSpacer.style, {
      height: `${RULER_HEIGHT}px`,
      background: "#1a1a1a",
      borderBottom: "1px solid #2a2a2a",
      flexShrink: "0",
    } satisfies Partial<CSSStyleDeclaration>);
    ui.headersCol.appendChild(headerSpacer);
    ui.lanesStack.appendChild(drawRuler(laneWidth));

    if (tracks.length === 0) {
      const empty = document.createElement("div");
      empty.textContent = "No tracks yet. Add one to start arranging.";
      Object.assign(empty.style, {
        color: "#7a7a7a",
        fontSize: "12px",
        padding: "16px",
      } satisfies Partial<CSSStyleDeclaration>);
      ui.lanesStack.appendChild(empty);
      drawSelection();
      drawPlayhead();
      return;
    }

    const grid = laneGridStyle();

    for (const track of tracks) {
      // Track header — fixed-width, on the left column. Three rows:
      //   1. name + Remove
      //   2. gain slider + dB readout
      //   3. clip count
      const header = document.createElement("div");
      Object.assign(header.style, {
        height: `${LANE_HEIGHT}px`,
        boxSizing: "border-box",
        padding: "6px 8px",
        background: "#1f1f1f",
        borderBottom: "1px solid #222",
        borderRight: "1px solid #2a2a2a",
        display: "flex",
        flexDirection: "column",
        gap: "3px",
        flexShrink: "0",
      } satisfies Partial<CSSStyleDeclaration>);

      const topRow = document.createElement("div");
      Object.assign(topRow.style, {
        display: "flex",
        alignItems: "center",
        gap: "4px",
      } satisfies Partial<CSSStyleDeclaration>);
      const name = document.createElement("div");
      name.textContent = track.name;
      Object.assign(name.style, {
        flex: "1",
        fontWeight: "500",
        fontSize: "12px",
        whiteSpace: "nowrap",
        overflow: "hidden",
        textOverflow: "ellipsis",
      } satisfies Partial<CSSStyleDeclaration>);

      // Mute / Solo state pills. Tiny, click-to-toggle. Solo overrides mute
      // in the mixdown when any track is soloed.
      const makeToggle = (
        label: string,
        active: boolean,
        activeColor: string,
      ): HTMLButtonElement => {
        const b = document.createElement("button");
        b.type = "button";
        b.textContent = label;
        Object.assign(b.style, {
          width: "16px",
          height: "16px",
          padding: "0",
          fontSize: "10px",
          fontWeight: "700",
          lineHeight: "14px",
          background: active ? activeColor : "#2a2a2a",
          color: active ? "#000" : "#888",
          border: `1px solid ${active ? activeColor : "#3a3a3a"}`,
          borderRadius: "2px",
          cursor: "pointer",
        } satisfies Partial<CSSStyleDeclaration>);
        return b;
      };
      const muteBtn = makeToggle("M", track.mute, "#e06060");
      muteBtn.title = "Mute this track";
      const soloBtn = makeToggle("S", track.solo, "#e0c060");
      soloBtn.title = "Solo this track";

      muteBtn.addEventListener("click", async () => {
        const next = !track.mute;
        track.mute = next;
        await client.setTrackMute(track.id, next);
        await refresh();
        rerenderIfPlaying();
      });
      soloBtn.addEventListener("click", async () => {
        const next = !track.solo;
        track.solo = next;
        await client.setTrackSolo(track.id, next);
        await refresh();
        rerenderIfPlaying();
      });

      const removeBtn = document.createElement("button");
      removeBtn.textContent = "×";
      removeBtn.type = "button";
      removeBtn.title = "Remove this track";
      Object.assign(removeBtn.style, btnStyle(), {
        padding: "0 6px",
        fontSize: "12px",
        lineHeight: "16px",
      } satisfies Partial<CSSStyleDeclaration>);
      removeBtn.addEventListener("click", async () => {
        await client.removeTrack(track.id);
        await refresh();
      });
      topRow.appendChild(name);
      topRow.appendChild(muteBtn);
      topRow.appendChild(soloBtn);
      topRow.appendChild(removeBtn);

      const bottomRow = document.createElement("div");
      Object.assign(bottomRow.style, {
        display: "flex",
        alignItems: "center",
        gap: "4px",
      } satisfies Partial<CSSStyleDeclaration>);

      // Classic spinner: editable value + stacked ▲/▼ buttons. The buttons
      // step the value by 0.5 dB; double-clicking the value resets to 0.
      const gainInput = document.createElement("input");
      gainInput.type = "text";
      gainInput.value = track.gainDb.toFixed(1);
      gainInput.title = "Track volume (dB) — type or use the arrows";
      Object.assign(gainInput.style, {
        width: "34px",
        padding: "1px 3px",
        background: "#0c0c0c",
        color: "#d8d8d8",
        border: "1px solid #3a3a3a",
        fontFamily: "ui-monospace, monospace",
        fontSize: "11px",
        textAlign: "right",
      } satisfies Partial<CSSStyleDeclaration>);

      const spinnerCol = document.createElement("div");
      Object.assign(spinnerCol.style, {
        display: "flex",
        flexDirection: "column",
      } satisfies Partial<CSSStyleDeclaration>);
      const makeArrow = (label: string): HTMLButtonElement => {
        const b = document.createElement("button");
        b.type = "button";
        b.textContent = label;
        Object.assign(b.style, {
          width: "14px",
          height: "10px",
          padding: "0",
          fontSize: "7px",
          lineHeight: "10px",
          background: "#2a2a2a",
          color: "#d8d8d8",
          border: "1px solid #3a3a3a",
          cursor: "pointer",
        } satisfies Partial<CSSStyleDeclaration>);
        return b;
      };
      const upBtn = makeArrow("▲");
      const downBtn = makeArrow("▼");
      spinnerCol.appendChild(upBtn);
      spinnerCol.appendChild(downBtn);

      const dbLabel = document.createElement("span");
      dbLabel.textContent = "dB";
      Object.assign(dbLabel.style, {
        color: "#888",
        fontSize: "10px",
        fontFamily: "ui-monospace, monospace",
      } satisfies Partial<CSSStyleDeclaration>);

      const sub = document.createElement("span");
      sub.textContent = `${track.clipCount} clip${track.clipCount === 1 ? "" : "s"}`;
      Object.assign(sub.style, {
        color: "#888",
        fontSize: "10px",
        marginLeft: "auto",
      } satisfies Partial<CSSStyleDeclaration>);

      const commitGain = (db: number, rerender: boolean): void => {
        const clamped = Math.max(-60, Math.min(6, db));
        gainInput.value = clamped.toFixed(1);
        void client.setTrackGain(track.id, clamped);
        if (rerender && playSession) {
          const wasLooping = playSession.loop;
          void startPlayback(wasLooping);
        }
      };
      gainInput.addEventListener("change", () => {
        const db = parseFloat(gainInput.value);
        if (!Number.isFinite(db)) {
          gainInput.value = track.gainDb.toFixed(1);
          return;
        }
        commitGain(db, true);
      });
      gainInput.addEventListener("dblclick", () => commitGain(0, true));
      upBtn.addEventListener("click", () => {
        const cur = parseFloat(gainInput.value);
        commitGain((Number.isFinite(cur) ? cur : 0) + 0.5, true);
      });
      downBtn.addEventListener("click", () => {
        const cur = parseFloat(gainInput.value);
        commitGain((Number.isFinite(cur) ? cur : 0) - 0.5, true);
      });

      bottomRow.appendChild(gainInput);
      bottomRow.appendChild(spinnerCol);
      bottomRow.appendChild(dbLabel);
      bottomRow.appendChild(sub);

      header.appendChild(topRow);
      header.appendChild(bottomRow);
      ui.headersCol.appendChild(header);

      // Lane.
      const lane = document.createElement("div");
      Object.assign(lane.style, {
        position: "relative",
        height: `${LANE_HEIGHT}px`,
        width: `${laneWidth}px`,
        background: "#0c0c0c",
        borderBottom: "1px solid #222",
        cursor: stagedSourceId ? "copy" : "default",
        flexShrink: "0",
        backgroundImage: grid.backgroundImage,
        backgroundSize: grid.backgroundSize,
        backgroundRepeat: "no-repeat",
      } satisfies Partial<CSSStyleDeclaration>);
      lane.addEventListener("click", async (ev) => {
        if (!stagedSourceId) return;
        const target = ev.target as HTMLElement;
        if (target !== lane) return;
        const rect = lane.getBoundingClientRect();
        const x = ev.clientX - rect.left;
        const positionFrame = snapFrames(pxToFrames(x));
        const src = sources.find((s) => s.id === stagedSourceId);
        if (!src) return;
        try {
          await client.addClip(track.id, stagedSourceId, positionFrame, 0, src.frames);
          setStatus(`placed ${src.name} on ${track.name}`);
          await refresh();
        } catch (err) {
          setStatus(`add clip failed: ${String(err)}`);
        }
      });

      const clips = clipsByTrack.get(track.id) ?? [];
      for (const c of clips) {
        const elt = document.createElement("div");
        const left = framesToPx(c.position);
        const width = Math.max(2, framesToPx(c.endPosition - c.position));
        const innerH = LANE_HEIGHT - 8;
        const isSelected =
          selectedClip?.trackId === track.id && selectedClip?.clipId === c.id;
        Object.assign(elt.style, {
          position: "absolute",
          left: `${left}px`,
          top: "4px",
          height: `${innerH}px`,
          width: `${width}px`,
          background: isSelected ? "#2f4a2f" : "#1f2f1f",
          border: isSelected ? "1px solid #b6e6b6" : "1px solid #4a6a4a",
          borderRadius: "3px",
          boxSizing: "border-box",
          overflow: "hidden",
          cursor: "pointer",
          userSelect: "none",
        } satisfies Partial<CSSStyleDeclaration>);
        const src = sources.find((s) => s.id === c.sourceId);

        // Waveform canvas. Size in device pixels matches the clip's display
        // width; if the clip is very wide we cap to a sane max so we don't
        // allocate a 30k-wide canvas at extreme zoom (rare but possible).
        const peaks = sourcePeaks.get(c.sourceId);
        if (peaks && src) {
          const cv = document.createElement("canvas");
          const cvW = Math.max(1, Math.floor(width));
          const cvH = innerH;
          cv.width = Math.min(cvW, 4096);
          cv.height = cvH;
          Object.assign(cv.style, {
            position: "absolute",
            top: "0",
            left: "0",
            width: `${width}px`,
            height: `${innerH}px`,
            pointerEvents: "none",
          } satisfies Partial<CSSStyleDeclaration>);
          const totalCols = peaks.length / 2;
          const startCol = Math.floor((c.sourceIn / Math.max(1, src.frames)) * totalCols);
          const endCol = Math.ceil((c.sourceOut / Math.max(1, src.frames)) * totalCols);
          paintClipWaveform(
            cv,
            peaks,
            startCol,
            Math.max(startCol + 1, endCol),
            isSelected ? "#cfe6cf" : "#9ece9e",
          );
          elt.appendChild(cv);
        }

        // Label: source name with text shadow so it's readable over peaks.
        const label = document.createElement("div");
        label.textContent = src?.name ?? c.sourceId;
        Object.assign(label.style, {
          position: "absolute",
          top: "1px",
          left: "4px",
          right: "4px",
          fontSize: "11px",
          color: "#ffffff",
          textShadow: "0 0 3px #000, 0 1px 1px #000",
          pointerEvents: "none",
          whiteSpace: "nowrap",
          overflow: "hidden",
          textOverflow: "ellipsis",
        } satisfies Partial<CSSStyleDeclaration>);
        elt.appendChild(label);

        const beats = framesToBeats(c.position);
        const beatLabel =
          gridMode === "beats"
            ? `bar ${Math.floor(beats / beatsPerBar) + 1} beat ${(beats % beatsPerBar) + 1}`
            : `${(c.position / projectSr).toFixed(3)}s`;
        elt.title = `${c.sourceId} @ ${beatLabel}`;
        elt.style.cursor = "move";
        elt.addEventListener("mousedown", (ev) => {
          if (ev.button !== 0) return;
          ev.stopPropagation();
          ev.preventDefault();
          setBinHover(false);
          clipDragState = {
            trackId: track.id,
            clipId: c.id,
            elt,
            startMouseX: ev.clientX,
            startPositionFrame: c.position,
            currentPositionFrame: c.position,
            moved: false,
          };
          ui.deleteClipBtn.disabled = false;
        });
        lane.appendChild(elt);
      }

      ui.lanesStack.appendChild(lane);
    }
    drawSelection();
    drawPlayhead();
  };

  // ---- BPM / time-sig change preserves beat alignment ------------------

  const reflowForGridChange = async (
    oldSecondsPerBeat: number,
    oldBeatsPerBar: number,
  ): Promise<void> => {
    const moves: Array<[number, number, number]> = []; // trackId, clipId, newFrames
    for (const [trackId, list] of clipsByTrack) {
      for (const c of list) {
        const beatsAbsolute = c.position / projectSr / oldSecondsPerBeat;
        // Carry the absolute beat value through to the new BPM. The
        // beats-per-bar change leaves bar boundaries in the same places
        // because beatsAbsolute is bar-agnostic.
        void oldBeatsPerBar;
        const newFrames = beatsToFrames(beatsAbsolute);
        if (newFrames !== c.position) {
          moves.push([trackId, c.id, newFrames]);
        }
      }
    }
    for (const [t, c, f] of moves) {
      try {
        await client.moveClip(t, c, f);
      } catch (err) {
        setStatus(`reflow move failed: ${String(err)}`);
      }
    }
    await refresh();
  };

  // ---- file load --------------------------------------------------------

  ui.fileInput.addEventListener("change", async () => {
    const file = ui.fileInput.files?.[0];
    if (!file) return;
    setStatus(`importing ${file.name}…`);
    try {
      const buf = await file.arrayBuffer();
      const imp = await client.importWav(file.name, new Uint8Array(buf));
      setStatus(`imported ${file.name} as ${imp.sourceId}`);
      ui.fileInput.value = "";
      await refresh();
    } catch (err) {
      setStatus(`import failed: ${String(err)}`);
    }
  });

  // ---- track / clip controls -------------------------------------------

  ui.addTrackBtn.addEventListener("click", async () => {
    const n = tracks.length + 1;
    await client.addTrack(`Track ${n}`);
    await refresh();
  });

  ui.deleteClipBtn.addEventListener("click", async () => {
    if (!selectedClip) return;
    await client.removeClip(selectedClip.trackId, selectedClip.clipId);
    selectedClip = null;
    await refresh();
  });

  // ---- clip FX --------------------------------------------------------
  // Destructive ops applied to the underlying source over the clip's
  // [sourceIn, sourceOut) window. Same path as the editor's gain effect:
  // build an opJson and call applyOp; refresh peaks for the affected source
  // and redraw; if playing, restart so the change is heard.

  type FxParam =
    | { kind: "number"; key: string; label: string; min: number; max: number; step: number; default: number }
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
    {
      id: "Reverse",
      label: "Reverse",
      params: [],
      build: (range) => ({ Reverse: { range } }),
    },
    {
      id: "DcRemove",
      label: "DC Remove",
      params: [],
      build: (range) => ({ DcRemove: { range } }),
    },
    {
      id: "Silence",
      label: "Silence",
      params: [],
      build: (range) => ({ Silence: { range } }),
    },
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
        { kind: "select", key: "ping_pong", label: "ping-pong", options: ["off", "on"], default: "off" },
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
  // Autotune is special: its UI depends on the chosen mode (Scale vs
  // Reference) and Reference mode requires async work (run YIN over a
  // reference clip) before the op can be built. So it lives outside the
  // simple fxDefs schema, picked by id "Autotune".
  {
    const opt = document.createElement("option");
    opt.value = "Autotune";
    opt.textContent = "Autotune";
    ui.fxKindSelect.appendChild(opt);
  }

  // Autotune-specific inputs live here. Built lazily when the user picks
  // "Autotune" in the kind dropdown; cleared when they pick anything else.
  let autotuneInputs: {
    mode: HTMLSelectElement;
    scaleRow: HTMLElement;
    scale: HTMLSelectElement;
    key: HTMLSelectElement;
    referenceRow: HTMLElement;
    refClip: HTMLSelectElement;
    retune: HTMLInputElement;
    formant: HTMLInputElement;
  } | null = null;

  const NOTE_NAMES = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];

  /** Populate the reference-clip dropdown with every clip currently in the
   *  arranger except the one that's being autotuned (we don't tune to
   *  ourselves). Each option's value is "trackId:clipId". */
  const populateReferenceClips = (excludeTrackId: number, excludeClipId: number): void => {
    if (!autotuneInputs) return;
    const sel = autotuneInputs.refClip;
    sel.innerHTML = "";
    for (const t of tracks) {
      const list = clipsByTrack.get(t.id) ?? [];
      for (const c of list) {
        if (t.id === excludeTrackId && c.id === excludeClipId) continue;
        const opt = document.createElement("option");
        opt.value = `${t.id}:${c.id}`;
        const src = findSource(c.sourceId);
        opt.textContent = `${t.name} — ${src?.name ?? c.sourceId}`;
        sel.appendChild(opt);
      }
    }
    if (sel.options.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "(no other clip available)";
      opt.disabled = true;
      sel.appendChild(opt);
    }
  };

  const buildAutotuneInputs = (): void => {
    ui.fxParamsRow.innerHTML = "";
    currentFxInputs = {};
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

    // Scale-mode controls (scale + key)
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

    // Reference-mode controls (clip picker)
    const referenceRow = document.createElement("span");
    Object.assign(referenceRow.style, { display: "none", gap: "6px" } satisfies Partial<CSSStyleDeclaration>);
    const refWrap = document.createElement("label");
    Object.assign(refWrap.style, wrapStyle);
    refWrap.appendChild(document.createTextNode("reference"));
    const refClip = document.createElement("select");
    Object.assign(refClip.style, selStyle);
    refWrap.appendChild(refClip);
    referenceRow.appendChild(refWrap);

    // Retune speed (always visible)
    const retuneWrap = document.createElement("label");
    Object.assign(retuneWrap.style, wrapStyle);
    retuneWrap.appendChild(document.createTextNode("retune ms"));
    const retune = makeNumberInput(60);
    retune.min = "0";
    retune.max = "500";
    retune.step = "5";
    retune.value = "0";
    retune.title = "0 = instant snap (T-Pain). Larger values glide toward target.";
    retuneWrap.appendChild(retune);

    // Formant preserve toggle (wired but unimplemented in v1)
    const formantWrap = document.createElement("label");
    Object.assign(formantWrap.style, wrapStyle);
    formantWrap.title =
      "Preserve formants — slot reserved, not yet implemented; toggle is stored in the project.";
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
      refClip,
      retune,
      formant,
    };

    mode.addEventListener("change", () => {
      const isRef = mode.value === "Reference";
      scaleRow.style.display = isRef ? "none" : "inline-flex";
      referenceRow.style.display = isRef ? "inline-flex" : "none";
      if (isRef && selectedClip) {
        populateReferenceClips(selectedClip.trackId, selectedClip.clipId);
      }
    });

    if (selectedClip) {
      populateReferenceClips(selectedClip.trackId, selectedClip.clipId);
    }
  };

  // Per-effect input elements live here so we can read their current values
  // at apply time. Recreated whenever the picker changes.
  let currentFxInputs: Record<string, HTMLInputElement | HTMLSelectElement> = {};

  const buildFxParamInputs = (def: FxDef): void => {
    ui.fxParamsRow.innerHTML = "";
    currentFxInputs = {};
    for (const p of def.params) {
      const wrap = document.createElement("label");
      Object.assign(wrap.style, {
        display: "inline-flex",
        alignItems: "center",
        gap: "4px",
        fontSize: "11px",
        color: "#aaa",
      } satisfies Partial<CSSStyleDeclaration>);
      const lab = document.createElement("span");
      lab.textContent = p.label;
      wrap.appendChild(lab);
      if (p.kind === "number") {
        const input = makeNumberInput(60);
        input.min = String(p.min);
        input.max = String(p.max);
        input.step = String(p.step);
        input.value = String(p.default);
        wrap.appendChild(input);
        currentFxInputs[p.key] = input;
      } else {
        const sel = document.createElement("select");
        Object.assign(sel.style, {
          padding: "2px 4px",
          background: "#0c0c0c",
          color: "#d8d8d8",
          border: "1px solid #3a3a3a",
          fontSize: "12px",
        } satisfies Partial<CSSStyleDeclaration>);
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

  const findClip = (): ClipInfo | null => {
    if (!selectedClip) return null;
    const list = clipsByTrack.get(selectedClip.trackId) ?? [];
    return list.find((c) => c.id === selectedClip!.clipId) ?? null;
  };

  const findSource = (sourceId: string): SourceInfo | null =>
    sources.find((s) => s.id === sourceId) ?? null;

  const syncFxPanel = (): void => {
    const clip = findClip();
    const enabled = clip !== null;
    ui.fxKindSelect.disabled = !enabled;
    ui.fxApplyBtn.disabled = !enabled;
    for (const el of Object.values(currentFxInputs)) el.disabled = !enabled;
    if (autotuneInputs) {
      autotuneInputs.mode.disabled = !enabled;
      autotuneInputs.scale.disabled = !enabled;
      autotuneInputs.key.disabled = !enabled;
      autotuneInputs.refClip.disabled = !enabled;
      autotuneInputs.retune.disabled = !enabled;
      autotuneInputs.formant.disabled = !enabled;
      if (enabled && selectedClip && autotuneInputs.mode.value === "Reference") {
        populateReferenceClips(selectedClip.trackId, selectedClip.clipId);
      }
    }
    // Undo/redo are per-source: enabled when a clip is selected.
    ui.fxUndoBtn.disabled = !enabled;
    ui.fxRedoBtn.disabled = !enabled;
    if (clip) {
      const src = findSource(clip.sourceId);
      const span = clip.sourceOut - clip.sourceIn;
      ui.fxInfo.textContent = src
        ? `Source: ${src.name} — applying to clip range [${clip.sourceIn}, ${clip.sourceOut}) (${span} frames). Destructive: every clip using this source will change.`
        : `Source ${clip.sourceId} (range ${clip.sourceIn}..${clip.sourceOut})`;
    } else {
      ui.fxInfo.textContent =
        "(select a clip to edit it destructively — affects every clip sharing the source)";
    }
  };

  ui.fxKindSelect.addEventListener("change", () => {
    if (ui.fxKindSelect.value === "Autotune") {
      buildAutotuneInputs();
    } else {
      autotuneInputs = null;
      const def = fxDefs.find((d) => d.id === ui.fxKindSelect.value);
      if (def) buildFxParamInputs(def);
    }
    syncFxPanel();
  });

  /** Build a Reference-mode contour aligned to the input clip's source
   *  range. For each hop within the input range we look up the timeline
   *  position, find the same timeline position inside the reference clip,
   *  and pull that hop's pitch from the reference's contour. Frames outside
   *  the reference clip stay at 0 Hz (autotune treats those as unvoiced
   *  and leaves the audio untouched). */
  const buildReferenceContour = async (
    inputClip: ClipInfo,
    refClip: ClipInfo,
    hopSamples: number,
    windowSamples: number,
  ): Promise<{ contour: number[]; hop: number }> => {
    const refContour = await client.detectPitchContour(
      refClip.sourceId,
      refClip.sourceIn,
      refClip.sourceOut,
      hopSamples,
      windowSamples,
    );
    const inputFrames = inputClip.sourceOut - inputClip.sourceIn;
    const numHops = Math.ceil(inputFrames / hopSamples) + 1;
    const aligned: number[] = new Array(numHops).fill(0);
    for (let i = 0; i < numHops; i++) {
      // Frame within the input source range, relative to inputClip.sourceIn.
      const inputOffset = i * hopSamples;
      // Project-timeline frame for that input position.
      const timelinePos = inputClip.position + inputOffset;
      // Same timeline frame mapped back to the reference clip's source.
      const refSourcePos = timelinePos - refClip.position + refClip.sourceIn;
      if (refSourcePos < refClip.sourceIn || refSourcePos >= refClip.sourceOut) {
        continue;
      }
      const refOffset = refSourcePos - refClip.sourceIn;
      const refIdx = Math.floor(refOffset / hopSamples);
      aligned[i] = refContour[refIdx] ?? 0;
    }
    return { contour: aligned, hop: hopSamples };
  };

  ui.fxApplyBtn.addEventListener("click", async () => {
    const clip = findClip();
    const sel = selectedClip;
    if (!clip || !sel) return;

    // Autotune lives outside fxDefs so it gets its own branch up front.
    if (ui.fxKindSelect.value === "Autotune") {
      if (!autotuneInputs) return;
      const range = { start: clip.sourceIn, end: clip.sourceOut };
      if (range.end <= range.start) {
        setStatus("fx: clip has empty range");
        return;
      }
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
          const refKey = autotuneInputs.refClip.value;
          if (!refKey) {
            setStatus("autotune: pick a reference clip");
            return;
          }
          const [refTrackIdStr, refClipIdStr] = refKey.split(":");
          const refTrackId = parseInt(refTrackIdStr ?? "", 10);
          const refClipId = parseInt(refClipIdStr ?? "", 10);
          const refList = clipsByTrack.get(refTrackId) ?? [];
          const refClip = refList.find((c) => c.id === refClipId);
          if (!refClip) {
            setStatus("autotune: reference clip not found");
            return;
          }
          // 25 ms hop, 50 ms window — same family as the engine's autotune
          // analysis windows, so contour alignment is straightforward.
          const hopSamples = Math.max(1, Math.round((projectSr * 25) / 1000));
          const windowSamples = Math.max(64, Math.round((projectSr * 50) / 1000));
          const { contour, hop } = await buildReferenceContour(
            clip,
            refClip,
            hopSamples,
            windowSamples,
          );
          target = {
            Reference: {
              contour_hz: contour,
              hop_samples: hop,
            },
          };
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
        await client.applyOp(clip.sourceId, opJson);
        await refreshSourceAfterFx(clip.sourceId);
        setStatus(`Autotune applied (${autotuneInputs.mode.value})`);
      } catch (err) {
        setStatus(`autotune failed: ${String(err)}`);
      } finally {
        syncFxPanel();
      }
      return;
    }

    const def = fxDefs.find((d) => d.id === ui.fxKindSelect.value);
    if (!def) return;
    const vals: Record<string, string> = {};
    for (const [k, el] of Object.entries(currentFxInputs)) vals[k] = el.value;
    const range = { start: clip.sourceIn, end: clip.sourceOut };
    if (range.end <= range.start) {
      setStatus("fx: clip has empty range");
      return;
    }
    const op = def.build(range, vals);
    ui.fxApplyBtn.disabled = true;
    try {
      await client.applyOp(clip.sourceId, JSON.stringify(op));

      // TimeStretch resizes the source range. Replace the clip so its
      // window covers the stretched content rather than just the prefix.
      // PitchShift preserves length, so no clip surgery needed.
      if (def.id === "TimeStretch") {
        const ratio = num(vals, "ratio", 1.0);
        if (Math.abs(ratio - 1.0) > 1e-3) {
          const oldLen = clip.sourceOut - clip.sourceIn;
          const newLen = Math.max(1, Math.round(oldLen * ratio));
          const newSourceOut = clip.sourceIn + newLen;
          await client.removeClip(sel.trackId, sel.clipId);
          const newId = await client.addClip(
            sel.trackId,
            clip.sourceId,
            clip.position,
            clip.sourceIn,
            newSourceOut,
          );
          selectedClip = { trackId: sel.trackId, clipId: newId };
          await refresh();
          setStatus(`Time Stretch ×${ratio.toFixed(2)} applied (clip resized)`);
          return;
        }
      }

      await refreshSourceAfterFx(clip.sourceId);
      setStatus(`${def.label} applied to ${clip.sourceId}`);
    } catch (err) {
      setStatus(`fx ${def.label} failed: ${String(err)}`);
    } finally {
      syncFxPanel();
    }
  });

  ui.fxUndoBtn.addEventListener("click", async () => {
    const clip = findClip();
    if (!clip) return;
    ui.fxUndoBtn.disabled = true;
    try {
      const did = await client.undo(clip.sourceId);
      setStatus(did ? "undone" : "nothing to undo");
      await refreshSourceAfterFx(clip.sourceId);
    } catch (err) {
      setStatus(`undo failed: ${String(err)}`);
    } finally {
      syncFxPanel();
    }
  });

  ui.fxRedoBtn.addEventListener("click", async () => {
    const clip = findClip();
    if (!clip) return;
    ui.fxRedoBtn.disabled = true;
    try {
      const did = await client.redo(clip.sourceId);
      setStatus(did ? "redone" : "nothing to redo");
      await refreshSourceAfterFx(clip.sourceId);
    } catch (err) {
      setStatus(`redo failed: ${String(err)}`);
    } finally {
      syncFxPanel();
    }
  });

  // After a destructive op runs we only need to refresh peaks for the one
  // affected source; everything else (clip lengths, tempo, tracks) is
  // unchanged. Then redraw and re-render any active playback.
  const refreshSourceAfterFx = async (sourceId: string): Promise<void> => {
    try {
      const peaks = await client.peakSummary(sourceId, PEAK_COLS);
      sourcePeaks.set(sourceId, peaks);
    } catch {
      sourcePeaks.delete(sourceId);
    }
    drawTracks();
    rerenderIfPlaying();
  };

  // ---- zoom -----------------------------------------------------------

  /** Zoom keeping `anchorContentPx` at `targetViewportX`. Both are in
   *  lanesScroll coordinates: anchorContentPx is the content offset of the
   *  point we want to keep stable, targetViewportX is where (relative to
   *  the visible viewport) we want it to land after the zoom. */
  const setZoom = (
    newPxPerSec: number,
    anchorContentPx: number,
    targetViewportX: number,
  ): void => {
    const clamped = Math.max(
      MIN_PIXELS_PER_SECOND,
      Math.min(MAX_PIXELS_PER_SECOND, newPxPerSec),
    );
    if (Math.abs(clamped - pixelsPerSecond) < 1e-3) return;
    const seconds = anchorContentPx / Math.max(0.001, pixelsPerSecond);
    pixelsPerSecond = clamped;
    drawTracks();
    const newAnchor = seconds * pixelsPerSecond;
    ui.lanesScroll.scrollLeft = Math.max(0, newAnchor - targetViewportX);
  };

  /** When the user presses a zoom button, pick something interesting to
   *  keep in view: in-point if a selection exists, else playhead if it's
   *  been placed, else the viewport centre. The chosen anchor lands in the
   *  middle of the viewport after the zoom. */
  const buttonZoomAnchor = (): number => {
    if (hasSelection() && inFrame !== null) return framesToPx(inFrame);
    if (playheadFrame > 0) return framesToPx(playheadFrame);
    return ui.lanesScroll.scrollLeft + ui.lanesScroll.clientWidth / 2;
  };

  ui.zoomInBtn.addEventListener("click", () => {
    setZoom(pixelsPerSecond * ZOOM_FACTOR, buttonZoomAnchor(), ui.lanesScroll.clientWidth / 2);
  });
  ui.zoomOutBtn.addEventListener("click", () => {
    setZoom(pixelsPerSecond / ZOOM_FACTOR, buttonZoomAnchor(), ui.lanesScroll.clientWidth / 2);
  });
  ui.zoomFitBtn.addEventListener("click", () => {
    const projFrames = projectLengthFrames();
    if (projFrames === 0 || projectSr === 0) {
      setZoom(DEFAULT_PIXELS_PER_SECOND, 0, 0);
      return;
    }
    const projSec = projFrames / projectSr;
    // Account for the trailer the lane reserves; aim for the project to
    // occupy ~85% of the viewport so it isn't crammed against the right.
    const visible = ui.lanesScroll.clientWidth * 0.85;
    setZoom(visible / Math.max(0.1, projSec), 0, 0);
    ui.lanesScroll.scrollLeft = 0;
  });

  ui.lanesScroll.addEventListener("wheel", (ev) => {
    if (!ev.ctrlKey && !ev.metaKey) return;
    ev.preventDefault();
    const rect = ui.lanesScroll.getBoundingClientRect();
    const cursorViewportX = ev.clientX - rect.left;
    const cursorContentPx = cursorViewportX + ui.lanesScroll.scrollLeft;
    const factor = ev.deltaY > 0 ? 1 / ZOOM_FACTOR : ZOOM_FACTOR;
    setZoom(pixelsPerSecond * factor, cursorContentPx, cursorViewportX);
  });

  // ---- grid controls ---------------------------------------------------

  const setMode = (mode: GridMode): void => {
    gridMode = mode;
    ui.modeBeatsBtn.style.background = mode === "beats" ? "#3a3a3a" : "#2a2a2a";
    ui.modeTimeBtn.style.background = mode === "time" ? "#3a3a3a" : "#2a2a2a";
    drawTracks();
  };
  ui.modeBeatsBtn.addEventListener("click", () => setMode("beats"));
  ui.modeTimeBtn.addEventListener("click", () => setMode("time"));
  setMode("beats");

  const persistTempo = (): void => {
    void client.setTempo(bpm, beatsPerBar, beatUnit);
  };

  ui.bpmInput.addEventListener("change", async () => {
    const v = parseFloat(ui.bpmInput.value);
    if (!Number.isFinite(v) || v <= 0) {
      ui.bpmInput.value = String(bpm);
      return;
    }
    const oldSPB = secondsPerBeat();
    const oldBPB = beatsPerBar;
    bpm = v;
    setStatus(`bpm ${bpm}`);
    await reflowForGridChange(oldSPB, oldBPB);
    persistTempo();
    rerenderIfPlaying();
  });

  ui.beatsPerBarInput.addEventListener("change", async () => {
    const v = parseInt(ui.beatsPerBarInput.value, 10);
    if (!Number.isFinite(v) || v <= 0 || v > 32) {
      ui.beatsPerBarInput.value = String(beatsPerBar);
      return;
    }
    const oldSPB = secondsPerBeat();
    const oldBPB = beatsPerBar;
    beatsPerBar = v;
    setStatus(`time signature ${beatsPerBar}/${beatUnit}`);
    await reflowForGridChange(oldSPB, oldBPB);
    persistTempo();
    rerenderIfPlaying();
  });

  ui.beatUnitInput.addEventListener("change", async () => {
    const v = parseInt(ui.beatUnitInput.value, 10);
    if (!Number.isFinite(v) || ![1, 2, 4, 8, 16, 32].includes(v)) {
      ui.beatUnitInput.value = String(beatUnit);
      return;
    }
    const oldSPB = secondsPerBeat();
    const oldBPB = beatsPerBar;
    beatUnit = v;
    setStatus(`time signature ${beatsPerBar}/${beatUnit}`);
    await reflowForGridChange(oldSPB, oldBPB);
    persistTempo();
    rerenderIfPlaying();
  });

  // ---- selection edge handles ------------------------------------------

  const startEdgeDrag = (edge: "in" | "out", ev: MouseEvent): void => {
    if (ev.button !== 0) return;
    if (inFrame === null || outFrame === null) return;
    ev.preventDefault();
    ev.stopPropagation();
    edgeDragState = {
      edge,
      startMouseX: ev.clientX,
      startFrame: edge === "in" ? inFrame : outFrame,
    };
  };
  ui.inHandle.addEventListener("mousedown", (ev) => startEdgeDrag("in", ev));
  ui.outHandle.addEventListener("mousedown", (ev) => startEdgeDrag("out", ev));

  // ---- ruler drag selection / bare-click playhead ----------------------

  window.addEventListener("mousemove", (ev) => {
    // Clip drag takes precedence — its mousedown stops propagation, but the
    // window-level mousemove still fires, so we branch on which drag is live.
    if (clipDragState) {
      const deltaPx = ev.clientX - clipDragState.startMouseX;
      const deltaFrames = Math.round((deltaPx / pixelsPerSecond) * projectSr);
      let newPos = clipDragState.startPositionFrame + deltaFrames;
      if (!ev.shiftKey) newPos = snapFrames(newPos);
      newPos = Math.max(0, newPos);
      if (Math.abs(deltaPx) > 3) clipDragState.moved = true;
      clipDragState.currentPositionFrame = newPos;
      clipDragState.elt.style.left = `${framesToPx(newPos)}px`;
      // Drop-on-bin detection: highlight while the cursor is over it.
      const r = ui.deleteClipBtn.getBoundingClientRect();
      const overBin =
        ev.clientX >= r.left &&
        ev.clientX <= r.right &&
        ev.clientY >= r.top &&
        ev.clientY <= r.bottom;
      setBinHover(overBin);
      return;
    }
    if (edgeDragState) {
      const deltaPx = ev.clientX - edgeDragState.startMouseX;
      const deltaFrames = Math.round((deltaPx / pixelsPerSecond) * projectSr);
      let newFrame = edgeDragState.startFrame + deltaFrames;
      if (!ev.shiftKey) newFrame = snapFrames(newFrame);
      newFrame = Math.max(0, newFrame);
      if (edgeDragState.edge === "in") {
        // Don't let the in edge cross or touch the out edge.
        inFrame = Math.min(newFrame, (outFrame ?? newFrame + 1) - 1);
      } else {
        outFrame = Math.max(newFrame, (inFrame ?? 0) + 1);
      }
      drawSelection();
      const inSec = (inFrame ?? 0) / Math.max(1, projectSr);
      const outSec = (outFrame ?? 0) / Math.max(1, projectSr);
      setStatus(`in ${inSec.toFixed(3)}s · out ${outSec.toFixed(3)}s`);
      return;
    }
    if (!dragState) return;
    const rect = ui.lanesContent.getBoundingClientRect();
    const x = ev.clientX - rect.left;
    let frame = pxToFrames(x);
    if (!ev.shiftKey) frame = snapFrames(frame);
    if (Math.abs(frame - dragState.anchorFrame) > 4) dragState.moved = true;
    const lo = Math.min(dragState.anchorFrame, frame);
    const hi = Math.max(dragState.anchorFrame, frame);
    inFrame = lo;
    outFrame = hi;
    drawSelection();
  });
  window.addEventListener("mouseup", async () => {
    if (clipDragState) {
      const ds = clipDragState;
      clipDragState = null;
      const droppedOnBin = binHovered;
      setBinHover(false);
      if (droppedOnBin) {
        try {
          await client.removeClip(ds.trackId, ds.clipId);
          if (
            selectedClip &&
            selectedClip.trackId === ds.trackId &&
            selectedClip.clipId === ds.clipId
          ) {
            selectedClip = null;
          }
          setStatus("clip binned");
        } catch (err) {
          setStatus(`delete failed: ${String(err)}`);
        }
        await refresh();
        return;
      }
      if (ds.moved) {
        try {
          await client.moveClip(ds.trackId, ds.clipId, ds.currentPositionFrame);
          const beats =
            ds.currentPositionFrame / Math.max(1, projectSr) / Math.max(1e-6, secondsPerBeat());
          setStatus(
            gridMode === "beats"
              ? `moved to bar ${Math.floor(beats / beatsPerBar) + 1} beat ${(beats % beatsPerBar) + 1}`
              : `moved to ${(ds.currentPositionFrame / projectSr).toFixed(3)}s`,
          );
        } catch (err) {
          setStatus(`move clip failed: ${String(err)}`);
        }
        await refresh();
      } else {
        // Bare click on a clip — select it.
        selectedClip = { trackId: ds.trackId, clipId: ds.clipId };
        drawTracks();
        syncToolbar();
      }
      return;
    }
    if (edgeDragState) {
      edgeDragState = null;
      return;
    }
    if (!dragState) return;
    if (!dragState.moved) {
      // Bare click on the ruler — drop the playhead but keep any existing
      // selection so Set In / Set Out can extend it. Clear Sel removes it.
      playheadFrame = dragState.anchorFrame;
      drawPlayhead();
      const sec = playheadFrame / Math.max(1, projectSr);
      setStatus(`playhead ${sec.toFixed(3)}s`);
    } else if (inFrame !== null && outFrame !== null && outFrame - inFrame > 0) {
      const inSec = inFrame / Math.max(1, projectSr);
      const outSec = outFrame / Math.max(1, projectSr);
      setStatus(`selection ${inSec.toFixed(3)}–${outSec.toFixed(3)}s`);
    } else {
      inFrame = null;
      outFrame = null;
      drawSelection();
    }
    dragState = null;
  });

  // ---- render & play ----------------------------------------------------

  const stopPlayhead = (): void => {
    if (playRaf !== null) cancelAnimationFrame(playRaf);
    playRaf = null;
  };

  const startPlayheadLoop = (): void => {
    const tick = (): void => {
      if (!playSession || !audioCtx) {
        playRaf = null;
        return;
      }
      const elapsed = audioCtx.currentTime - playSession.startCtxTime;
      let posSec = playSession.offsetSec + elapsed;
      if (playSession.loop) {
        const ls = playSession.loopStartSec;
        const le = playSession.loopEndSec;
        const len = Math.max(0.001, le - ls);
        if (posSec >= le) {
          posSec = ls + ((posSec - ls) % len);
        }
      } else if (posSec > playSession.bufferDuration) {
        posSec = playSession.bufferDuration;
      }
      playheadFrame = Math.round(posSec * projectSr);
      drawPlayhead();
      playRaf = requestAnimationFrame(tick);
    };
    if (playRaf === null) playRaf = requestAnimationFrame(tick);
  };

  const stopRendered = (): void => {
    if (currentBufferSrc) {
      try {
        currentBufferSrc.stop();
      } catch {
        /* already stopped */
      }
      currentBufferSrc.disconnect();
      currentBufferSrc = null;
    }
    playSession = null;
    stopPlayhead();
    drawPlayhead(); // keep playhead at last position
    ui.stopBtn.disabled = true;
    ui.playBtn.disabled = projectLengthFrames() === 0;
    ui.loopBtn.disabled = projectLengthFrames() === 0;
  };

  const startPlayback = async (loop: boolean): Promise<void> => {
    if (projectLengthFrames() === 0) return;
    setStatus("rendering mixdown…");
    ui.playBtn.disabled = true;
    ui.loopBtn.disabled = true;
    try {
      const wavBytes = await client.mixdownWav();
      const ab = wavBytes.buffer.slice(
        wavBytes.byteOffset,
        wavBytes.byteOffset + wavBytes.byteLength,
      ) as ArrayBuffer;
      if (!audioCtx) audioCtx = new AudioContext();
      if (audioCtx.state === "suspended") await audioCtx.resume();
      const decoded = await audioCtx.decodeAudioData(ab);
      stopRendered();

      // If the user's out-point sits past the rendered audio, pad with
      // trailing silence so the loop / play range can extend into it.
      // AudioBufferSourceNode's loopEnd is clamped to buffer.duration, so
      // there's no way to "loop into silence" without growing the buffer.
      let activeBuffer = decoded;
      const desiredEndSec = hasSelection() ? outFrame! / projectSr : 0;
      if (desiredEndSec > decoded.duration) {
        const targetFrames = Math.ceil(desiredEndSec * decoded.sampleRate);
        const padded = audioCtx.createBuffer(
          decoded.numberOfChannels,
          targetFrames,
          decoded.sampleRate,
        );
        for (let ch = 0; ch < decoded.numberOfChannels; ch++) {
          padded.copyToChannel(decoded.getChannelData(ch), ch, 0);
        }
        activeBuffer = padded;
      }

      const node = audioCtx.createBufferSource();
      node.buffer = activeBuffer;
      node.connect(audioCtx.destination);

      let offsetSec: number;
      let durationSec: number | undefined;
      let loopStartSec = 0;
      let loopEndSec = activeBuffer.duration;

      if (hasSelection()) {
        const inSec = inFrame! / projectSr;
        const outSec = Math.min(outFrame! / projectSr, activeBuffer.duration);
        if (loop) {
          node.loop = true;
          node.loopStart = inSec;
          node.loopEnd = outSec;
          loopStartSec = inSec;
          loopEndSec = outSec;
          offsetSec = inSec;
        } else {
          offsetSec = inSec;
          durationSec = Math.max(0.001, outSec - inSec);
        }
      } else {
        const startSec = Math.min(playheadFrame / projectSr, activeBuffer.duration);
        offsetSec = startSec;
        if (loop) {
          node.loop = true;
          node.loopStart = 0;
          node.loopEnd = activeBuffer.duration;
        }
      }

      node.onended = () => {
        if (currentBufferSrc === node) {
          stopRendered();
          setStatus("playback ended");
        }
      };
      currentBufferSrc = node;
      const startCtxTime = audioCtx.currentTime;
      if (durationSec !== undefined) {
        node.start(0, offsetSec, durationSec);
      } else {
        node.start(0, offsetSec);
      }
      playSession = {
        startCtxTime,
        offsetSec,
        loop,
        loopStartSec,
        loopEndSec,
        bufferDuration: activeBuffer.duration,
      };
      ui.stopBtn.disabled = false;
      ui.playBtn.disabled = false;
      ui.loopBtn.disabled = false;
      setStatus(
        loop
          ? `looping ${(loopEndSec - loopStartSec).toFixed(2)}s @ ${audioCtx.sampleRate} Hz`
          : `playing ${(durationSec ?? activeBuffer.duration - offsetSec).toFixed(2)}s @ ${audioCtx.sampleRate} Hz`,
      );
      startPlayheadLoop();
    } catch (err) {
      setStatus(`mixdown failed: ${String(err)}`);
      ui.playBtn.disabled = false;
      ui.loopBtn.disabled = false;
    }
  };

  ui.playBtn.addEventListener("click", () => {
    void startPlayback(false);
  });
  ui.loopBtn.addEventListener("click", () => {
    void startPlayback(true);
  });
  ui.stopBtn.addEventListener("click", () => {
    stopRendered();
    setStatus("stopped");
  });

  ui.clearSelBtn.addEventListener("click", () => {
    inFrame = null;
    outFrame = null;
    drawSelection();
    setStatus("selection cleared");
  });

  // Delete (not Backspace — browsers map that to navigate-back) removes the
  // selected clip. Skip when typing in inputs so digits can be edited freely.
  window.addEventListener("keydown", (ev) => {
    if (ev.key !== "Delete") return;
    if (!selectedClip) return;
    const t = ev.target as HTMLElement | null;
    if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) {
      return;
    }
    ev.preventDefault();
    const target = selectedClip;
    void (async () => {
      try {
        await client.removeClip(target.trackId, target.clipId);
        if (
          selectedClip &&
          selectedClip.trackId === target.trackId &&
          selectedClip.clipId === target.clipId
        ) {
          selectedClip = null;
        }
        await refresh();
        setStatus("clip deleted");
      } catch (err) {
        setStatus(`delete failed: ${String(err)}`);
      }
    })();
  });

  ui.bpmInput.value = String(bpm);
  ui.beatsPerBarInput.value = String(beatsPerBar);
  ui.beatUnitInput.value = String(beatUnit);
  await refresh();
  return { refresh };
}

interface ArrangerUi {
  fileInput: HTMLInputElement;
  sourceList: HTMLDivElement;
  headersCol: HTMLDivElement;
  lanesScroll: HTMLDivElement;
  lanesContent: HTMLDivElement;
  lanesStack: HTMLDivElement;
  selectionOverlay: HTMLDivElement;
  inHandle: HTMLDivElement;
  outHandle: HTMLDivElement;
  playheadOverlay: HTMLDivElement;
  addTrackBtn: HTMLButtonElement;
  deleteClipBtn: HTMLButtonElement;
  modeBeatsBtn: HTMLButtonElement;
  modeTimeBtn: HTMLButtonElement;
  bpmInput: HTMLInputElement;
  beatsPerBarInput: HTMLInputElement;
  beatUnitInput: HTMLInputElement;
  zoomInBtn: HTMLButtonElement;
  zoomOutBtn: HTMLButtonElement;
  zoomFitBtn: HTMLButtonElement;
  playBtn: HTMLButtonElement;
  loopBtn: HTMLButtonElement;
  stopBtn: HTMLButtonElement;
  clearSelBtn: HTMLButtonElement;
  fxPanel: HTMLDivElement;
  fxKindSelect: HTMLSelectElement;
  fxParamsRow: HTMLDivElement;
  fxApplyBtn: HTMLButtonElement;
  fxUndoBtn: HTMLButtonElement;
  fxRedoBtn: HTMLButtonElement;
  fxInfo: HTMLSpanElement;
  status: HTMLDivElement;
}

function buildUi(root: HTMLElement): ArrangerUi {
  root.innerHTML = "";
  Object.assign(root.style, {
    display: "flex",
    flexDirection: "column",
    gap: "12px",
  } satisfies Partial<CSSStyleDeclaration>);

  // File-import row.
  const fileRow = document.createElement("label");
  Object.assign(fileRow.style, {
    display: "flex",
    alignItems: "center",
    gap: "8px",
  } satisfies Partial<CSSStyleDeclaration>);
  fileRow.textContent = "Import WAV: ";
  const fileInput = document.createElement("input");
  fileInput.type = "file";
  fileInput.accept = ".wav,audio/wav,audio/x-wav";
  fileRow.appendChild(fileInput);
  root.appendChild(fileRow);

  // Two-pane: narrow Sources column + tracks pane.
  const split = document.createElement("div");
  Object.assign(split.style, {
    display: "flex",
    gap: "12px",
    minHeight: "320px",
  } satisfies Partial<CSSStyleDeclaration>);

  const sourcesPane = document.createElement("div");
  Object.assign(sourcesPane.style, {
    width: "180px",
    flexShrink: "0",
    border: "1px solid #2a2a2a",
    background: "#141414",
    display: "flex",
    flexDirection: "column",
  } satisfies Partial<CSSStyleDeclaration>);
  const sourcesHeader = document.createElement("div");
  sourcesHeader.textContent = "Sources";
  Object.assign(sourcesHeader.style, {
    padding: "6px 8px",
    fontWeight: "600",
    fontSize: "12px",
    background: "#1f1f1f",
    borderBottom: "1px solid #2a2a2a",
  } satisfies Partial<CSSStyleDeclaration>);
  const sourceList = document.createElement("div");
  Object.assign(sourceList.style, {
    flex: "1",
    overflowY: "auto",
  } satisfies Partial<CSSStyleDeclaration>);
  sourcesPane.appendChild(sourcesHeader);
  sourcesPane.appendChild(sourceList);

  // Tracks pane: toolbar (Add Track, Delete clip, mode toggle, BPM, time-sig)
  // → tracks-body (headers column + shared horizontally-scrolling lanes).
  const tracksPane = document.createElement("div");
  Object.assign(tracksPane.style, {
    flex: "1",
    minWidth: "0",
    border: "1px solid #2a2a2a",
    background: "#141414",
    display: "flex",
    flexDirection: "column",
  } satisfies Partial<CSSStyleDeclaration>);

  const toolbar = document.createElement("div");
  Object.assign(toolbar.style, {
    padding: "6px 8px",
    fontSize: "12px",
    background: "#1f1f1f",
    borderBottom: "1px solid #2a2a2a",
    display: "flex",
    alignItems: "center",
    gap: "12px",
    flexWrap: "wrap",
  } satisfies Partial<CSSStyleDeclaration>);
  const tracksTitle = document.createElement("span");
  tracksTitle.textContent = "Tracks";
  tracksTitle.style.fontWeight = "600";

  const addTrackBtn = makeToolbarBtn("Add Track");
  const deleteClipBtn = makeToolbarBtn("Bin");
  deleteClipBtn.disabled = true;
  deleteClipBtn.title = "Click to bin the selected clip — or drag a clip here";

  const sep1 = makeSep();
  const gridLabel = document.createElement("span");
  gridLabel.textContent = "Grid:";
  gridLabel.style.color = "#aaa";
  const modeBeatsBtn = makeToolbarBtn("Beats");
  const modeTimeBtn = makeToolbarBtn("Time");

  const sep2 = makeSep();
  const bpmLabel = document.createElement("span");
  bpmLabel.textContent = "BPM:";
  bpmLabel.style.color = "#aaa";
  const bpmInput = makeNumberInput(50);
  bpmInput.min = "1";
  bpmInput.max = "999";
  bpmInput.step = "1";

  const tsLabel = document.createElement("span");
  tsLabel.textContent = "Time:";
  tsLabel.style.color = "#aaa";
  const beatsPerBarInput = makeNumberInput(40);
  beatsPerBarInput.min = "1";
  beatsPerBarInput.max = "32";
  beatsPerBarInput.step = "1";
  const tsSlash = document.createElement("span");
  tsSlash.textContent = "/";
  tsSlash.style.color = "#aaa";
  const beatUnitInput = makeNumberInput(40);
  beatUnitInput.min = "1";
  beatUnitInput.max = "32";
  beatUnitInput.step = "1";

  const sep3 = makeSep();
  const zoomLabel = document.createElement("span");
  zoomLabel.textContent = "Zoom:";
  zoomLabel.style.color = "#aaa";
  const zoomInBtn = makeToolbarBtn("+");
  const zoomOutBtn = makeToolbarBtn("−");
  const zoomFitBtn = makeToolbarBtn("Fit");

  toolbar.appendChild(tracksTitle);
  toolbar.appendChild(addTrackBtn);
  toolbar.appendChild(deleteClipBtn);
  toolbar.appendChild(sep1);
  toolbar.appendChild(gridLabel);
  toolbar.appendChild(modeBeatsBtn);
  toolbar.appendChild(modeTimeBtn);
  toolbar.appendChild(sep2);
  toolbar.appendChild(bpmLabel);
  toolbar.appendChild(bpmInput);
  toolbar.appendChild(tsLabel);
  toolbar.appendChild(beatsPerBarInput);
  toolbar.appendChild(tsSlash);
  toolbar.appendChild(beatUnitInput);
  toolbar.appendChild(sep3);
  toolbar.appendChild(zoomLabel);
  toolbar.appendChild(zoomOutBtn);
  toolbar.appendChild(zoomInBtn);
  toolbar.appendChild(zoomFitBtn);
  tracksPane.appendChild(toolbar);

  // tracks-body: headers column on the left, lanes-scroll on the right.
  const body = document.createElement("div");
  Object.assign(body.style, {
    display: "flex",
    flex: "1",
    minWidth: "0",
    overflow: "hidden",
  } satisfies Partial<CSSStyleDeclaration>);

  const headersCol = document.createElement("div");
  Object.assign(headersCol.style, {
    width: `${HEADER_COL_WIDTH}px`,
    flexShrink: "0",
    display: "flex",
    flexDirection: "column",
    background: "#181818",
    borderRight: "1px solid #2a2a2a",
  } satisfies Partial<CSSStyleDeclaration>);

  const lanesScroll = document.createElement("div");
  Object.assign(lanesScroll.style, {
    flex: "1",
    minWidth: "0",
    overflowX: "auto",
    overflowY: "hidden",
  } satisfies Partial<CSSStyleDeclaration>);
  // lanesContent owns the static playhead + selection overlays; the dynamic
  // ruler+lane content lives in lanesStack so we can rebuild it freely
  // without clobbering the overlays.
  const lanesContent = document.createElement("div");
  Object.assign(lanesContent.style, {
    position: "relative",
    display: "flex",
    flexDirection: "column",
  } satisfies Partial<CSSStyleDeclaration>);
  const lanesStack = document.createElement("div");
  Object.assign(lanesStack.style, {
    display: "flex",
    flexDirection: "column",
  } satisfies Partial<CSSStyleDeclaration>);
  const selectionOverlay = document.createElement("div");
  Object.assign(selectionOverlay.style, {
    position: "absolute",
    top: "0",
    bottom: "0",
    left: "0",
    width: "0",
    background: "rgba(124, 209, 124, 0.18)",
    pointerEvents: "none",
    display: "none",
    zIndex: "1",
  } satisfies Partial<CSSStyleDeclaration>);
  // Grab handles for the in/out edges. Confined to the ruler row so they
  // don't obstruct clicks on clips below. Each handle is an 8px hit zone
  // with a brighter 2px line in the middle.
  const makeHandle = (): HTMLDivElement => {
    const h = document.createElement("div");
    Object.assign(h.style, {
      position: "absolute",
      top: "0",
      height: `${RULER_HEIGHT}px`,
      width: "10px",
      marginLeft: "-5px",
      cursor: "ew-resize",
      display: "none",
      zIndex: "3",
      background:
        "linear-gradient(to right, transparent 0, transparent 4px, #b6e6b6 4px, #b6e6b6 6px, transparent 6px, transparent 10px)",
    } satisfies Partial<CSSStyleDeclaration>);
    return h;
  };
  const inHandle = makeHandle();
  inHandle.title = "Drag the in point";
  const outHandle = makeHandle();
  outHandle.title = "Drag the out point";
  const playheadOverlay = document.createElement("div");
  Object.assign(playheadOverlay.style, {
    position: "absolute",
    top: "0",
    bottom: "0",
    left: "0",
    width: "1px",
    background: "#e6e6e6",
    pointerEvents: "none",
    display: "none",
    zIndex: "2",
  } satisfies Partial<CSSStyleDeclaration>);
  lanesContent.appendChild(lanesStack);
  lanesContent.appendChild(selectionOverlay);
  lanesContent.appendChild(inHandle);
  lanesContent.appendChild(outHandle);
  lanesContent.appendChild(playheadOverlay);
  lanesScroll.appendChild(lanesContent);

  body.appendChild(headersCol);
  body.appendChild(lanesScroll);
  tracksPane.appendChild(body);

  split.appendChild(sourcesPane);
  split.appendChild(tracksPane);
  root.appendChild(split);

  // Transport row.
  const transport = document.createElement("div");
  Object.assign(transport.style, {
    display: "flex",
    gap: "8px",
    alignItems: "center",
  } satisfies Partial<CSSStyleDeclaration>);
  const playBtn = document.createElement("button");
  playBtn.textContent = "Render & Play";
  playBtn.type = "button";
  playBtn.disabled = true;
  Object.assign(playBtn.style, btnStyle());
  const loopBtn = document.createElement("button");
  loopBtn.textContent = "Loop";
  loopBtn.type = "button";
  loopBtn.disabled = true;
  Object.assign(loopBtn.style, btnStyle());
  const stopBtn = document.createElement("button");
  stopBtn.textContent = "Stop";
  stopBtn.type = "button";
  stopBtn.disabled = true;
  Object.assign(stopBtn.style, btnStyle());
  transport.appendChild(playBtn);
  transport.appendChild(loopBtn);
  transport.appendChild(stopBtn);

  const transportSep = document.createElement("div");
  Object.assign(transportSep.style, {
    width: "1px",
    height: "24px",
    background: "#2a2a2a",
    margin: "0 4px",
  } satisfies Partial<CSSStyleDeclaration>);
  transport.appendChild(transportSep);

  const clearSelBtn = document.createElement("button");
  clearSelBtn.textContent = "Clear Sel";
  clearSelBtn.type = "button";
  Object.assign(clearSelBtn.style, btnStyle());
  transport.appendChild(clearSelBtn);

  const hint = document.createElement("span");
  hint.textContent =
    "(drag the ruler to make a selection, then drag its green edges to refine — Shift bypasses snap)";
  hint.style.color = "#9a9a9a";
  hint.style.fontSize = "12px";
  transport.appendChild(hint);
  root.appendChild(transport);

  // FX panel — destructive effects applied to the selected clip's range on
  // its underlying source. Greyed out when no clip is selected.
  const fxPanel = document.createElement("div");
  Object.assign(fxPanel.style, {
    display: "flex",
    flexDirection: "column",
    gap: "4px",
    padding: "6px 8px",
    background: "#1a1a1a",
    border: "1px solid #2a2a2a",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxRow = document.createElement("div");
  Object.assign(fxRow.style, {
    display: "flex",
    alignItems: "center",
    gap: "8px",
    flexWrap: "wrap",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxLabel = document.createElement("span");
  fxLabel.textContent = "Clip FX:";
  fxLabel.style.color = "#aaa";
  fxLabel.style.fontWeight = "600";
  fxLabel.style.fontSize = "12px";
  const fxKindSelect = document.createElement("select");
  Object.assign(fxKindSelect.style, {
    padding: "2px 4px",
    background: "#0c0c0c",
    color: "#d8d8d8",
    border: "1px solid #3a3a3a",
    fontSize: "12px",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxParamsRow = document.createElement("div");
  Object.assign(fxParamsRow.style, {
    display: "flex",
    alignItems: "center",
    gap: "6px",
    flex: "1",
    flexWrap: "wrap",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxApplyBtn = document.createElement("button");
  fxApplyBtn.textContent = "Apply to clip";
  fxApplyBtn.type = "button";
  fxApplyBtn.disabled = true;
  Object.assign(fxApplyBtn.style, btnStyle());
  const fxSep = document.createElement("div");
  Object.assign(fxSep.style, {
    width: "1px",
    height: "20px",
    background: "#2a2a2a",
    margin: "0 2px",
  } satisfies Partial<CSSStyleDeclaration>);
  const fxUndoBtn = document.createElement("button");
  fxUndoBtn.textContent = "Undo";
  fxUndoBtn.type = "button";
  fxUndoBtn.disabled = true;
  Object.assign(fxUndoBtn.style, btnStyle());
  const fxRedoBtn = document.createElement("button");
  fxRedoBtn.textContent = "Redo";
  fxRedoBtn.type = "button";
  fxRedoBtn.disabled = true;
  Object.assign(fxRedoBtn.style, btnStyle());
  fxRow.appendChild(fxLabel);
  fxRow.appendChild(fxKindSelect);
  fxRow.appendChild(fxParamsRow);
  fxRow.appendChild(fxApplyBtn);
  fxRow.appendChild(fxSep);
  fxRow.appendChild(fxUndoBtn);
  fxRow.appendChild(fxRedoBtn);
  const fxInfo = document.createElement("span");
  Object.assign(fxInfo.style, {
    color: "#9a9a9a",
    fontSize: "11px",
    fontFamily: "ui-monospace, monospace",
  } satisfies Partial<CSSStyleDeclaration>);
  fxInfo.textContent = "(select a clip to edit it destructively — affects every clip sharing the source)";
  fxPanel.appendChild(fxRow);
  fxPanel.appendChild(fxInfo);
  root.appendChild(fxPanel);

  const status = document.createElement("div");
  Object.assign(status.style, {
    fontSize: "12px",
    color: "#9a9a9a",
    fontFamily: "ui-monospace, monospace",
  } satisfies Partial<CSSStyleDeclaration>);
  root.appendChild(status);

  return {
    fileInput,
    sourceList,
    headersCol,
    lanesScroll,
    lanesContent,
    lanesStack,
    selectionOverlay,
    inHandle,
    outHandle,
    playheadOverlay,
    addTrackBtn,
    deleteClipBtn,
    modeBeatsBtn,
    modeTimeBtn,
    bpmInput,
    beatsPerBarInput,
    beatUnitInput,
    zoomInBtn,
    zoomOutBtn,
    zoomFitBtn,
    playBtn,
    loopBtn,
    stopBtn,
    clearSelBtn,
    fxPanel,
    fxKindSelect,
    fxParamsRow,
    fxApplyBtn,
    fxUndoBtn,
    fxRedoBtn,
    fxInfo,
    status,
  };
}

function makeToolbarBtn(label: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  Object.assign(b.style, btnStyle(), {
    padding: "2px 10px",
    fontSize: "12px",
  } satisfies Partial<CSSStyleDeclaration>);
  return b;
}

function makeSep(): HTMLDivElement {
  const s = document.createElement("div");
  Object.assign(s.style, {
    width: "1px",
    alignSelf: "stretch",
    background: "#2a2a2a",
    margin: "0 2px",
  } satisfies Partial<CSSStyleDeclaration>);
  return s;
}

/** Paint a min/max peak strip into a clip's canvas. `peaks` is a flat
 *  Float32Array of `[min0, max0, min1, max1, ...]` for the full source;
 *  `[startCol, endCol)` is the column slice corresponding to the clip's
 *  source_in..source_out window. */
function paintClipWaveform(
  canvas: HTMLCanvasElement,
  peaks: Float32Array,
  startCol: number,
  endCol: number,
  color: string,
): void {
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  const w = canvas.width;
  const h = canvas.height;
  ctx.clearRect(0, 0, w, h);
  ctx.fillStyle = color;
  const totalCols = peaks.length / 2;
  const cols = Math.max(1, endCol - startCol);
  const yMid = h / 2;
  for (let x = 0; x < w; x++) {
    const c = startCol + Math.floor((x / w) * cols);
    const cClamped = Math.max(0, Math.min(totalCols - 1, c));
    const i = cClamped * 2;
    const min = peaks[i] ?? 0;
    const max = peaks[i + 1] ?? 0;
    const yMax = yMid - max * yMid;
    const yMin = yMid - min * yMid;
    const barH = Math.max(1, yMin - yMax);
    ctx.fillRect(x, yMax, 1, barH);
  }
}

function makeNumberInput(widthPx: number): HTMLInputElement {
  const i = document.createElement("input");
  i.type = "number";
  Object.assign(i.style, {
    width: `${widthPx}px`,
    padding: "2px 4px",
    background: "#0c0c0c",
    color: "#d8d8d8",
    border: "1px solid #3a3a3a",
    fontFamily: "ui-monospace, monospace",
    fontSize: "12px",
  } satisfies Partial<CSSStyleDeclaration>);
  return i;
}
