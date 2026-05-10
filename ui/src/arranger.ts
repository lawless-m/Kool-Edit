// Multitrack arrangement tab. Sources column on the left, track lanes on the
// right with a shared horizontal scroll, beat/bar grid, and a ruler row.
// Clip positions are stored in frames in the engine; the UI snaps to beats
// when the grid mode is `beats`. Changing BPM or time-sig re-positions every
// clip so the beat alignment is preserved (a clip "at bar 3 beat 2" stays
// "at bar 3 beat 2" — the time changes, the beat doesn't).

import type {
  Breakpoint,
  ClipInfo,
  EngineClient,
  GroupInfo,
  PatternInfo,
  SourceInfo,
  TrackInfo,
} from "./engine/client";
import { btnStyle } from "./editor";
import type { SavedPatternGrid } from "./drums";
import { NON_ACCENT_DB } from "./drums";

const DEFAULT_PIXELS_PER_SECOND = 60;
const MIN_PIXELS_PER_SECOND = 5;
const MAX_PIXELS_PER_SECOND = 1000;
const ZOOM_FACTOR = 1.5;
const LANE_HEIGHT = 64;
const RULER_HEIGHT = 24;
const HEADER_COL_WIDTH = 180;
const MIN_LANE_PX = 800;

type GridMode = "beats" | "time";
type SnapDivision =
  | "off"
  | "bar"
  | "beat"
  | "1/2"
  | "1/3"
  | "1/4"
  | "1/8"
  | "1/16"
  | "1/32"
  | "1/48"
  | "1/64";

const SNAP_OPTIONS: { value: SnapDivision; label: string }[] = [
  { value: "off", label: "Off" },
  { value: "bar", label: "Bar" },
  { value: "beat", label: "Beat" },
  { value: "1/2", label: "1/2 beat" },
  { value: "1/3", label: "Triplet (1/3)" },
  { value: "1/4", label: "1/4 beat" },
  { value: "1/8", label: "1/8 beat" },
  { value: "1/16", label: "1/16 beat" },
  { value: "1/32", label: "1/32 beat" },
  { value: "1/48", label: "1/48 beat" },
  { value: "1/64", label: "1/64 beat" },
];

export interface ArrangerHandle {
  refresh(): Promise<void>;
}

export interface ArrangerOptions {
  /** Called when the arranger imports a new wav into the library so the
   *  editor's source list can refresh without a tab switch. */
  onSourceImported?: () => void;
}

export async function mountArranger(
  root: HTMLElement,
  client: EngineClient,
  opts: ArrangerOptions = {},
): Promise<ArrangerHandle> {
  const ui = buildUi(root);

  let projectSr = await client.projectSampleRate();
  let sources: SourceInfo[] = [];
  let tracks: TrackInfo[] = [];
  const clipsByTrack = new Map<number, ClipInfo[]>();
  // Named groups, refreshed alongside clips. Map for O(1) lookup by id.
  const groupNames = new Map<number, string>();
  // Saved patterns, refreshed on every pattern-list change. Names are unique.
  let savedPatterns: PatternInfo[] = [];
  // Pre-baked peak summaries (one Float32Array per source, length = 2 × COLS)
  // so clip rendering doesn't pay a round trip per redraw. Refreshed on every
  // refresh() call so destructive edits in the editor tab show up here.
  const sourcePeaks = new Map<string, Float32Array>();
  const PEAK_COLS = 2048;
  let stagedSourceId: string | null = null;
  // Multi-selection. Keys are `${trackId}:${clipId}`. Plain click replaces
  // the selection with just the clicked clip; Ctrl/Cmd-click toggles a clip
  // in/out of the set. Drag moves every selected clip together.
  const selectedClips = new Set<string>();
  const clipKey = (trackId: number, clipId: number): string => `${trackId}:${clipId}`;
  const isClipSelected = (trackId: number, clipId: number): boolean =>
    selectedClips.has(clipKey(trackId, clipId));

  // ---- groups ----------------------------------------------------------
  // Clips with the same non-zero `group` id move and select as one. Per-clip
  // parameters (trim, gain, pan, envelopes) stay independent — groups bind
  // position only.

  /** Find every clip in the project sharing the given non-zero group id,
   *  returned as `${trackId}:${clipId}` keys. */
  const groupMembers = (groupId: number): string[] => {
    if (groupId === 0) return [];
    const out: string[] = [];
    for (const [trackId, list] of clipsByTrack) {
      for (const c of list) {
        if (c.group === groupId) out.push(clipKey(trackId, c.id));
      }
    }
    return out;
  };

  /** Look up a clip by its `${trackId}:${clipId}` key. */
  const clipByKey = (key: string): { trackId: number; clip: ClipInfo } | null => {
    const [tIdStr, cIdStr] = key.split(":");
    const tId = Number(tIdStr);
    const cId = Number(cIdStr);
    const list = clipsByTrack.get(tId);
    const clip = list?.find((c) => c.id === cId);
    return clip ? { trackId: tId, clip } : null;
  };

  /** Pull all group siblings of any grouped clip in `set` into the set, in
   *  place. Cheap: a single sweep through the project clips. */
  const expandSelectionToGroups = (set: Set<string>): void => {
    const seenGroups = new Set<number>();
    for (const key of set) {
      const found = clipByKey(key);
      if (found && found.clip.group > 0) seenGroups.add(found.clip.group);
    }
    for (const g of seenGroups) {
      for (const k of groupMembers(g)) set.add(k);
    }
  };

  /** Mint a fresh group id by finding the max existing one + 1. */
  const nextGroupId = (): number => {
    let max = 0;
    for (const list of clipsByTrack.values()) {
      for (const c of list) {
        if (c.group > max) max = c.group;
      }
    }
    for (const id of groupNames.keys()) {
      if (id > max) max = id;
    }
    return max + 1;
  };

  /** Custom name from `groupNames`, or default "Group N" fallback. */
  const groupDisplayName = (groupId: number): string =>
    groupNames.get(groupId) ?? `Group ${groupId}`;

  /** Deterministic colour from a group id using the golden-angle hue trick.
   *  Returns an HSL string usable as both background tint and border. */
  const groupHue = (groupId: number): number => (groupId * 137.508) % 360;
  const groupBorderColor = (groupId: number): string =>
    `hsl(${groupHue(groupId).toFixed(1)}, 65%, 65%)`;
  const groupFillColor = (groupId: number, selected: boolean): string =>
    `hsl(${groupHue(groupId).toFixed(1)}, ${selected ? 50 : 35}%, ${selected ? 30 : 22}%)`;

  /** UI-side clipboard for "Copy Sel" / "Insert ×N". Each entry is one
   *  clip-portion: cropped to fit inside the original selection so the
   *  unit of repetition is exactly one selection length. `offset` is the
   *  frame distance from the selection in-point to where the clip should
   *  land in each pasted copy. */
  type ClipboardEntry = {
    trackId: number;
    sourceId: string;
    sourceIn: number;
    sourceOut: number;
    offset: number;
  };
  let clipboardEntries: ClipboardEntry[] = [];
  let clipboardLength = 0;
  let audioCtx: AudioContext | null = null;
  let currentBufferSrc: AudioBufferSourceNode | null = null;

  // Volume envelope edit mode. While on, every clip gets a polyline +
  // breakpoint dot overlay; clicking on a clip adds a breakpoint, dragging
  // a dot moves it, double-clicking deletes. Values are dB; visual y maps
  // [DB_MIN, DB_MAX] across the clip's lane height.
  // Folders the user has collapsed in the Sources panel. Persists across
  // refreshes within a session; folders default to expanded.
  const collapsedSourceFolders = new Set<string>();

  let envelopeMode = false;
  const ENV_DB_MIN = -24;
  const ENV_DB_MAX = 12;
  // Pixel-Y → dB and vice versa, inside a clip lane of height `h`.
  const yToDb = (y: number, h: number): number => {
    const t = Math.max(0, Math.min(1, 1 - y / Math.max(1, h)));
    return ENV_DB_MIN + t * (ENV_DB_MAX - ENV_DB_MIN);
  };
  const dbToY = (db: number, h: number): number => {
    const clamped = Math.max(ENV_DB_MIN, Math.min(ENV_DB_MAX, db));
    const t = (clamped - ENV_DB_MIN) / (ENV_DB_MAX - ENV_DB_MIN);
    return (1 - t) * h;
  };
  let envelopeDrag: {
    trackId: number;
    clipId: number;
    bpIndex: number;
    clipDiv: HTMLElement;
    clipFrames: number;
  } | null = null;

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
  type DraggedClip = {
    trackId: number;
    clipId: number;
    elt: HTMLElement;
    startPositionFrame: number;
  };
  // The clip the user grabbed is the anchor — its snap drives the group's
  // delta, and every other selected clip shifts by the same frames so the
  // relative spacing of the group is preserved.
  let clipDragState: {
    anchor: DraggedClip;
    others: DraggedClip[];
    startMouseX: number;
    currentDelta: number;
    moved: boolean;
  } | null = null;
  let edgeDragState: {
    edge: "in" | "out";
    startMouseX: number;
    startFrame: number;
  } | null = null;
  // Box / marquee selection. Starts on mousedown over empty lane space and
  // grows as the user drags. The element is appended to lanesContent (a
  // sibling of lanesStack) so drawTracks's rebuild doesn't nuke it. Hit
  // testing uses each clip element's bounding rect against viewport
  // coordinates so the math is independent of which container we live in.
  let marqueeState: {
    startClientX: number;
    startClientY: number;
    additive: boolean;
    baseSelection: Set<string>;
    elt: HTMLDivElement;
    moved: boolean;
  } | null = null;
  let binHovered = false;

  // Grid state. BPM means quarter-note BPM regardless of time-sig denominator
  // (the universal convention). secondsPerBeat() factors in the denominator.
  let bpm = 120;
  let beatsPerBar = 4;
  let beatUnit = 4; // 4 = quarter, 8 = eighth, etc.
  let gridMode: GridMode = "beats";
  let snapDivision: SnapDivision = "beat";
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

  /** Snap step in *beats*. Returns null when snap is off or grid mode is
   *  time. The bar option is resolved against the current time-sig so a
   *  later beats-per-bar change picks up automatically. */
  const snapStepBeats = (): number | null => {
    if (gridMode !== "beats" || snapDivision === "off") return null;
    switch (snapDivision) {
      case "bar":
        return beatsPerBar;
      case "beat":
        return 1;
      case "1/2":
        return 0.5;
      case "1/3":
        return 1 / 3;
      case "1/4":
        return 0.25;
      case "1/8":
        return 0.125;
      case "1/16":
        return 1 / 16;
      case "1/32":
        return 1 / 32;
      case "1/48":
        return 1 / 48;
      case "1/64":
        return 1 / 64;
    }
  };

  const snapFrames = (frames: number): number => {
    const step = snapStepBeats();
    if (step === null) return frames;
    const beats = framesToBeats(frames);
    return beatsToFrames(Math.round(beats / step) * step);
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
    // Refresh group names + saved patterns. Older engine builds without
    // these may throw; in that case we just leave the maps empty.
    groupNames.clear();
    try {
      const gs: GroupInfo[] = await client.listGroups();
      for (const g of gs) groupNames.set(g.id, g.name);
    } catch {
      // pre-groups engine — ignore
    }
    try {
      savedPatterns = await client.listPatterns();
    } catch {
      savedPatterns = [];
    }
    refreshPatternPicker();
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
    // Prune the selection set to clips that still exist after the refresh.
    const live = new Set<string>();
    for (const [trackId, list] of clipsByTrack) {
      for (const c of list) live.add(clipKey(trackId, c.id));
    }
    for (const k of [...selectedClips]) {
      if (!live.has(k)) selectedClips.delete(k);
    }
    drawSources();
    drawTracks();
    syncToolbar();
  };

  const syncToolbar = (): void => {
    // The bin is enabled when there's a selection (so clicking it works) AND
    // during clip drag (so it's a visible drop target). The drag path forces
    // it on directly; this covers the click-to-bin path.
    ui.deleteClipBtn.disabled = selectedClips.size === 0 && clipDragState === null;
    const noProject = projectLengthFrames() === 0;
    ui.playBtn.disabled = noProject;
    ui.loopBtn.disabled = noProject;
    // Group needs at least 2 clips to make sense. Ungroup is enabled if any
    // selected clip is currently in a group.
    ui.groupBtn.disabled = selectedClips.size < 2;
    ui.ungroupBtn.disabled = ![...selectedClips].some((k) => {
      const f = clipByKey(k);
      return f !== null && f.clip.group > 0;
    });
    // Rename only makes sense when the selection is exactly one group (every
    // selected clip shares the same non-zero group id).
    ui.renameGroupBtn.disabled = singleSelectedGroup() === null;
    // Insert needs both a pattern and a destination position. We use the
    // current arranger selection in-point if there is one, else the playhead.
    ui.insertPatternBtn.disabled =
      savedPatterns.length === 0 || ui.patternPicker.value === "";
  };

  /** Returns the group id when every selected clip belongs to the same
   *  non-zero group; null otherwise (mixed, ungrouped, or empty). */
  const singleSelectedGroup = (): number | null => {
    if (selectedClips.size === 0) return null;
    let g: number | null = null;
    for (const k of selectedClips) {
      const f = clipByKey(k);
      if (!f || f.clip.group === 0) return null;
      if (g === null) g = f.clip.group;
      else if (g !== f.clip.group) return null;
    }
    return g;
  };

  const refreshPatternPicker = (): void => {
    const prev = ui.patternPicker.value;
    ui.patternPicker.innerHTML = "";
    if (savedPatterns.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "(no saved patterns)";
      ui.patternPicker.appendChild(opt);
    } else {
      for (const p of savedPatterns) {
        const opt = document.createElement("option");
        opt.value = p.name;
        opt.textContent = p.name;
        ui.patternPicker.appendChild(opt);
      }
      // Preserve the previous selection if still valid.
      if (savedPatterns.some((p) => p.name === prev)) ui.patternPicker.value = prev;
    }
  };

  const setBinHover = (b: boolean): void => {
    if (binHovered === b) return;
    binHovered = b;
    ui.deleteClipBtn.style.background = b ? "#a83030" : "#2a2a2a";
    ui.deleteClipBtn.style.borderColor = b ? "#ff6060" : "#3a3a3a";
    ui.deleteClipBtn.style.color = b ? "#ffffff" : "#d8d8d8";
  };

  // Cheap restyle of every clip element to reflect the current selection.
  // Used during marquee drag so we don't pay a full drawTracks rebuild on
  // every mousemove (which would also destroy the marquee element). The
  // waveform colour stays put — it gets refreshed on the next full redraw.
  const updateClipSelectionStyles = (): void => {
    const elts = ui.lanesStack.querySelectorAll<HTMLElement>("[data-clip-key]");
    for (const elt of elts) {
      const key = elt.dataset["clipKey"];
      if (!key) continue;
      const sel = selectedClips.has(key);
      const groupIdStr = elt.dataset["groupId"];
      const groupId = groupIdStr ? Number(groupIdStr) : 0;
      if (groupId > 0) {
        elt.style.background = groupFillColor(groupId, sel);
        elt.style.border = `${sel ? 2 : 1}px solid ${groupBorderColor(groupId)}`;
      } else {
        elt.style.background = sel ? "#2f4a2f" : "#1f2f1f";
        elt.style.border = `${sel ? 2 : 1}px solid ${sel ? "#b6e6b6" : "#4a6a4a"}`;
      }
    }
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
    // Same grouping/sort as the editor library: every source under a
    // folder header, with "/" as the default folder for sources with no
    // folder set so the whole list is collapsible.
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
    const renderRow = (s: SourceInfo, indent: boolean): void => {
      const row = document.createElement("div");
      const staged = stagedSourceId === s.id;
      Object.assign(row.style, {
        padding: indent ? "6px 8px 6px 20px" : "6px 8px",
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
    };
    const folderNames = Array.from(byFolder.keys()).sort((a, b) =>
      a.localeCompare(b, undefined, { sensitivity: "base" }),
    );
    for (const folder of folderNames) {
      const collapsed = collapsedSourceFolders.has(folder);
      const hdr = document.createElement("div");
      hdr.textContent = `${collapsed ? "▸" : "▾"} ${folder}`;
      hdr.title = "Click to collapse/expand";
      Object.assign(hdr.style, {
        padding: "4px 8px",
        background: "#1a1a1a",
        borderBottom: "1px solid #222",
        fontFamily: "var(--ff-mono)",
        fontSize: "11px",
        color: "var(--text-2)",
        letterSpacing: "0.04em",
        cursor: "pointer",
      } satisfies Partial<CSSStyleDeclaration>);
      hdr.addEventListener("click", () => {
        if (collapsedSourceFolders.has(folder)) collapsedSourceFolders.delete(folder);
        else collapsedSourceFolders.add(folder);
        drawSources();
      });
      ui.sourceList.appendChild(hdr);
      if (collapsed) continue;
      for (const s of byFolder.get(folder)!) renderRow(s, true);
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

  /** Build the CSS for a lane's background grid. Layers stack from
   *  faintest to strongest so the strong lines paint on top: sub-beat
   *  (only when snap is finer than 1 beat), beat, bar. In time mode
   *  there's just one tick per second. */
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
    const step = snapStepBeats();
    const layers: string[] = [];
    const sizes: string[] = [];
    // Sub-beat layer: only when the snap step is finer than a beat. Even
    // finer than ~6 px between lines turns into mush, so skip drawing it
    // in that case (mathematically still snaps, just no visual aid).
    if (step !== null && step < 1) {
      const subPx = beatPx * step;
      if (subPx >= 6) {
        layers.push(
          `repeating-linear-gradient(to right,
            transparent 0,
            transparent ${subPx - 1}px,
            rgba(120,120,120,0.10) ${subPx - 1}px,
            rgba(120,120,120,0.10) ${subPx}px)`,
        );
        sizes.push(`${subPx}px 100%`);
      }
    }
    layers.push(
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
    );
    sizes.push(`${beatPx}px 100%`, `${barPx}px 100%`);
    return {
      backgroundImage: layers.join(", "),
      backgroundSize: sizes.join(", "),
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
      const step = snapStepBeats();
      const totalBars = Math.ceil(laneWidth / barPx);
      // Sub-beat ticks first (faintest), so beat / bar ticks paint over.
      // Only draw sub-beat ticks when (a) the snap step is finer than
      // a beat and (b) there's enough px between them to be useful.
      if (step !== null && step < 1) {
        const subPx = beatPx * step;
        if (subPx >= 6) {
          const totalSubs = Math.ceil(laneWidth / subPx);
          for (let i = 0; i <= totalSubs; i++) {
            // Skip subdivisions that fall on a beat or bar line — those
            // get their own (stronger) tick below.
            const beatsAt = i * step;
            if (Math.abs(beatsAt - Math.round(beatsAt)) < 1e-6) continue;
            const xSub = i * subPx;
            if (xSub > laneWidth) break;
            const tick = document.createElement("div");
            Object.assign(tick.style, {
              position: "absolute",
              left: `${xSub}px`,
              top: "8px",
              bottom: "0",
              width: "1px",
              background: "rgba(120,120,120,0.25)",
            } satisfies Partial<CSSStyleDeclaration>);
            ruler.appendChild(tick);
          }
        }
      }
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
    const has = hasSelection();
    ui.renderToClipBtn.disabled = !has;
    ui.copySelBtn.disabled = !has;
    ui.insertCopiesBtn.disabled = !has || clipboardEntries.length === 0;
    if (!has) {
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

  /** Build the SVG overlay that shows + edits a clip's volume envelope.
   *  Drawn in clip-local coordinates. Click on empty space adds a
   *  breakpoint, mousedown on a dot drags it (clamped to the clip's
   *  bounds), double-click on a dot deletes. Persists via setClipEnvelope
   *  and triggers a redraw. */
  const renderEnvelopeOverlay = (
    trackId: number,
    clip: ClipInfo,
    width: number,
    height: number,
  ): SVGSVGElement => {
    const SVG = "http://www.w3.org/2000/svg";
    const svg = document.createElementNS(SVG, "svg") as SVGSVGElement;
    svg.setAttribute("width", String(width));
    svg.setAttribute("height", String(height));
    Object.assign(svg.style, {
      position: "absolute",
      top: "0",
      left: "0",
      width: `${width}px`,
      height: `${height}px`,
      cursor: "crosshair",
    } satisfies Partial<CSSStyleDeclaration>);

    const clipFrames = Math.max(1, clip.sourceOut - clip.sourceIn);
    const env = clip.volumeEnvelope ?? [];

    // 0 dB reference line so it's clear where unity is.
    const zeroLine = document.createElementNS(SVG, "line");
    zeroLine.setAttribute("x1", "0");
    zeroLine.setAttribute("x2", String(width));
    zeroLine.setAttribute("y1", String(dbToY(0, height)));
    zeroLine.setAttribute("y2", String(dbToY(0, height)));
    zeroLine.setAttribute("stroke", "rgba(255,255,255,0.18)");
    zeroLine.setAttribute("stroke-dasharray", "2 3");
    zeroLine.setAttribute("pointer-events", "none");
    svg.appendChild(zeroLine);

    const frameToX = (frame: number): number =>
      Math.max(0, Math.min(width, (frame / clipFrames) * width));

    // Polyline through all breakpoints, extended horizontally past the
    // first/last so the implicit "hold" before/after is visible. With
    // zero breakpoints we draw nothing — the click handler still works
    // and the user gets a fresh start.
    if (env.length > 0) {
      const pts: Array<[number, number]> = [];
      const firstY = dbToY(env[0]!.value, height);
      pts.push([0, firstY]);
      for (const bp of env) {
        pts.push([frameToX(bp.time), dbToY(bp.value, height)]);
      }
      const lastY = dbToY(env[env.length - 1]!.value, height);
      pts.push([width, lastY]);
      const poly = document.createElementNS(SVG, "polyline");
      poly.setAttribute("points", pts.map(([x, y]) => `${x},${y}`).join(" "));
      poly.setAttribute("fill", "none");
      poly.setAttribute("stroke", "var(--accent)");
      poly.setAttribute("stroke-width", "1.5");
      poly.setAttribute("pointer-events", "none");
      svg.appendChild(poly);
    }

    // Background hit-area for "click to add". Below the dots so dots
    // intercept their own events first.
    const bg = document.createElementNS(SVG, "rect");
    bg.setAttribute("x", "0");
    bg.setAttribute("y", "0");
    bg.setAttribute("width", String(width));
    bg.setAttribute("height", String(height));
    bg.setAttribute("fill", "transparent");
    bg.addEventListener("mousedown", (ev) => {
      if (ev.button !== 0) return;
      ev.stopPropagation();
      ev.preventDefault();
      const rect = svg.getBoundingClientRect();
      const x = ev.clientX - rect.left;
      const y = ev.clientY - rect.top;
      const time = Math.max(0, Math.min(clipFrames - 1, Math.round((x / width) * clipFrames)));
      const value = yToDb(y, height);
      // Insert sorted by time; if a breakpoint already exists at this
      // exact frame, skip (the engine rejects duplicate times).
      const next = (clip.volumeEnvelope ?? []).slice();
      if (next.some((bp) => bp.time === time)) return;
      next.push({ time, value, curve: "Linear" });
      next.sort((a, b) => a.time - b.time);
      void persistEnvelope(trackId, clip.id, next);
    });
    svg.appendChild(bg);

    // Dots + per-dot interaction.
    for (let i = 0; i < env.length; i++) {
      const bp = env[i]!;
      const cx = frameToX(bp.time);
      const cy = dbToY(bp.value, height);
      const dot = document.createElementNS(SVG, "circle");
      dot.setAttribute("cx", String(cx));
      dot.setAttribute("cy", String(cy));
      dot.setAttribute("r", "4");
      dot.setAttribute("fill", "var(--accent)");
      dot.setAttribute("stroke", "var(--bg-0)");
      dot.setAttribute("stroke-width", "1.5");
      dot.style.cursor = "grab";
      dot.addEventListener("mousedown", (ev) => {
        if (ev.button !== 0) return;
        ev.stopPropagation();
        ev.preventDefault();
        envelopeDrag = {
          trackId,
          clipId: clip.id,
          bpIndex: i,
          clipDiv: svg.parentElement as HTMLElement,
          clipFrames,
        };
        dot.style.cursor = "grabbing";
      });
      dot.addEventListener("dblclick", (ev) => {
        ev.stopPropagation();
        ev.preventDefault();
        const next = (clip.volumeEnvelope ?? []).slice();
        next.splice(i, 1);
        void persistEnvelope(trackId, clip.id, next);
      });
      svg.appendChild(dot);
    }

    return svg;
  };

  const persistEnvelope = async (
    trackId: number,
    clipId: number,
    breakpoints: Breakpoint[],
  ): Promise<void> => {
    try {
      await client.setClipEnvelope(trackId, clipId, "volume", breakpoints);
      // Update local cache so the redraw shows the new state without
      // round-tripping through listClips. refresh() would also work but
      // this keeps the interaction snappy.
      const list = clipsByTrack.get(trackId) ?? [];
      const clip = list.find((c) => c.id === clipId);
      if (clip) clip.volumeEnvelope = breakpoints;
      drawTracks();
      rerenderIfPlaying();
    } catch (err) {
      setStatus(`envelope persist failed: ${String(err)}`);
    }
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
      const name = document.createElement("input");
      name.type = "text";
      name.value = track.name;
      name.title = "Track name — click to rename";
      Object.assign(name.style, {
        flex: "1",
        minWidth: "0",
        fontWeight: "500",
        fontSize: "12px",
        fontFamily: "inherit",
        background: "transparent",
        color: "#d8d8d8",
        border: "1px solid transparent",
        borderRadius: "2px",
        padding: "0 2px",
        outline: "none",
      } satisfies Partial<CSSStyleDeclaration>);
      name.addEventListener("focus", () => {
        name.style.background = "#0c0c0c";
        name.style.borderColor = "#3a3a3a";
      });
      const commitName = async (): Promise<void> => {
        name.style.background = "transparent";
        name.style.borderColor = "transparent";
        const next = name.value.trim();
        if (next === "" || next === track.name) {
          name.value = track.name;
          return;
        }
        track.name = next;
        await client.setTrackName(track.id, next);
      };
      name.addEventListener("blur", () => {
        void commitName();
      });
      name.addEventListener("keydown", (ev) => {
        if (ev.key === "Enter") {
          ev.preventDefault();
          name.blur();
        } else if (ev.key === "Escape") {
          ev.preventDefault();
          name.value = track.name;
          name.blur();
        }
      });

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

      // Choke: trim each clip's source_out so it ends where the next clip on
      // this track begins. For drum tracks where every kick is longer than a
      // beat, this stops the tails summing additively into distortion. If
      // any clips on this track are in the current selection, the trim is
      // restricted to just those — useful for choking a region without
      // disturbing tails elsewhere on the same track.
      const chokeBtn = document.createElement("button");
      chokeBtn.type = "button";
      chokeBtn.textContent = "Choke";
      chokeBtn.title =
        "Trim clips on this track to end where the next clip starts " +
        "(restricted to selected clips when any are selected on this track)";
      Object.assign(chokeBtn.style, {
        padding: "0 5px",
        height: "16px",
        fontSize: "10px",
        lineHeight: "14px",
        background: "#2a2a2a",
        color: "#d8d8d8",
        border: "1px solid #3a3a3a",
        borderRadius: "2px",
        cursor: "pointer",
      } satisfies Partial<CSSStyleDeclaration>);
      chokeBtn.addEventListener("click", async () => {
        const list = (clipsByTrack.get(track.id) ?? [])
          .slice()
          .sort((a, b) => a.position - b.position);
        // Restrict to the user's selection on this track when any clip on
        // this track is selected; otherwise choke the whole track.
        const selectedHere = new Set(
          list.filter((c) => isClipSelected(track.id, c.id)).map((c) => c.id),
        );
        const restrictToSelection = selectedHere.size > 0;
        let trimmed = 0;
        for (let i = 0; i < list.length - 1; i++) {
          const cur = list[i];
          if (restrictToSelection && !selectedHere.has(cur.id)) continue;
          const next = list[i + 1];
          if (cur.endPosition <= next.position) continue;
          const overlap = next.position - cur.position;
          if (overlap <= 0) continue;
          const newSourceOut = cur.sourceIn + overlap;
          if (newSourceOut <= cur.sourceIn) continue;
          await client.setClipSourceRange(track.id, cur.id, cur.sourceIn, newSourceOut);
          trimmed++;
        }
        setStatus(
          restrictToSelection
            ? `choked ${trimmed} selected clip${trimmed === 1 ? "" : "s"} on "${track.name}"`
            : `choked ${trimmed} clip${trimmed === 1 ? "" : "s"} on "${track.name}"`,
        );
        await refresh();
        rerenderIfPlaying();
      });

      bottomRow.appendChild(gainInput);
      bottomRow.appendChild(spinnerCol);
      bottomRow.appendChild(dbLabel);
      bottomRow.appendChild(chokeBtn);
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
      lane.addEventListener("mousedown", (ev) => {
        if (ev.button !== 0) return;
        if (stagedSourceId) return; // staged-source placement is handled on click
        if ((ev.target as HTMLElement) !== lane) return;
        ev.preventDefault();
        const additive = ev.ctrlKey || ev.metaKey || ev.shiftKey;
        if (!additive) selectedClips.clear();
        const baseSelection = new Set(selectedClips);
        // Marquee element is parented to lanesContent (a sibling of
        // lanesStack) so it survives drawTracks rebuilds. Coords are
        // therefore in lanesContent space.
        const containerRect = ui.lanesContent.getBoundingClientRect();
        const startX = ev.clientX - containerRect.left;
        const startY = ev.clientY - containerRect.top;
        const elt = document.createElement("div");
        Object.assign(elt.style, {
          position: "absolute",
          left: `${startX}px`,
          top: `${startY}px`,
          width: "0px",
          height: "0px",
          border: "1px dashed #b6e6b6",
          background: "rgba(124, 209, 124, 0.12)",
          pointerEvents: "none",
          zIndex: "5",
        } satisfies Partial<CSSStyleDeclaration>);
        ui.lanesContent.appendChild(elt);
        marqueeState = {
          startClientX: ev.clientX,
          startClientY: ev.clientY,
          additive,
          baseSelection,
          elt,
          moved: false,
        };
        updateClipSelectionStyles();
        syncToolbar();
      });
      lane.addEventListener("click", async (ev) => {
        if (!stagedSourceId) return; // empty-space clear is handled by marquee mousedown
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
        elt.dataset["clipKey"] = clipKey(track.id, c.id);
        const left = framesToPx(c.position);
        const width = Math.max(2, framesToPx(c.endPosition - c.position));
        const innerH = LANE_HEIGHT - 8;
        const isSelected = isClipSelected(track.id, c.id);
        // Grouped clips get a deterministic hue; ungrouped keep the original
        // green palette. Selection bumps the saturation/lightness either way.
        const grouped = c.group > 0;
        const bg = grouped
          ? groupFillColor(c.group, isSelected)
          : isSelected ? "#2f4a2f" : "#1f2f1f";
        const borderCol = grouped
          ? groupBorderColor(c.group)
          : isSelected ? "#b6e6b6" : "#4a6a4a";
        Object.assign(elt.style, {
          position: "absolute",
          left: `${left}px`,
          top: "4px",
          height: `${innerH}px`,
          width: `${width}px`,
          background: bg,
          border: `${isSelected ? 2 : 1}px solid ${borderCol}`,
          borderRadius: "3px",
          boxSizing: "border-box",
          overflow: "hidden",
          cursor: "pointer",
          userSelect: "none",
        } satisfies Partial<CSSStyleDeclaration>);
        elt.dataset["groupId"] = String(c.group);
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

        if (envelopeMode) {
          const overlay = renderEnvelopeOverlay(track.id, c, width, innerH);
          elt.appendChild(overlay);
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

        // Group-name badge. Sits in the bottom-left so it doesn't fight with
        // the source-name label up top. Only rendered when the clip is in a
        // group and the clip is wide enough to show it.
        if (grouped && width > 40) {
          const badge = document.createElement("div");
          badge.textContent = groupDisplayName(c.group);
          Object.assign(badge.style, {
            position: "absolute",
            bottom: "1px",
            left: "4px",
            fontSize: "10px",
            color: "#000",
            background: groupBorderColor(c.group),
            padding: "0 4px",
            borderRadius: "2px",
            fontWeight: "600",
            pointerEvents: "none",
            maxWidth: `${Math.max(20, width - 8)}px`,
            whiteSpace: "nowrap",
            overflow: "hidden",
            textOverflow: "ellipsis",
          } satisfies Partial<CSSStyleDeclaration>);
          elt.appendChild(badge);
        }

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
          const key = clipKey(track.id, c.id);
          // Ctrl/Cmd-click toggles this clip in/out of the selection without
          // starting a drag — matches the standard "extend selection" gesture.
          // For grouped clips the toggle fans out to every member so the group
          // moves and unselects as one.
          if (ev.ctrlKey || ev.metaKey) {
            const peers = c.group > 0 ? groupMembers(c.group) : [key];
            const allIn = peers.every((k) => selectedClips.has(k));
            for (const k of peers) {
              if (allIn) selectedClips.delete(k);
              else selectedClips.add(k);
            }
            drawTracks();
            syncToolbar();
            return;
          }
          // Plain click on an unselected clip replaces the selection so the
          // user can drag a single clip out of a multi-selection naturally.
          // Clicking a clip that's already part of the selection keeps the
          // group intact so the whole selection moves together.
          if (!selectedClips.has(key)) {
            selectedClips.clear();
            selectedClips.add(key);
            expandSelectionToGroups(selectedClips);
            drawTracks();
            syncToolbar();
          }
          // Build the drag cohort: the grabbed clip is the anchor, and every
          // other selected clip rides along. We capture each one's element
          // and start position so mousemove only has to apply a delta.
          const anchor: DraggedClip = {
            trackId: track.id,
            clipId: c.id,
            elt,
            startPositionFrame: c.position,
          };
          const others: DraggedClip[] = [];
          for (const otherKey of selectedClips) {
            if (otherKey === key) continue;
            const [tIdStr, cIdStr] = otherKey.split(":");
            const tId = Number(tIdStr);
            const cId = Number(cIdStr);
            const list = clipsByTrack.get(tId);
            const clip = list?.find((x) => x.id === cId);
            if (!clip) continue;
            const otherElt = ui.lanesStack.querySelector<HTMLElement>(
              `[data-clip-key="${otherKey}"]`,
            );
            if (!otherElt) continue;
            others.push({
              trackId: tId,
              clipId: cId,
              elt: otherElt,
              startPositionFrame: clip.position,
            });
          }
          clipDragState = {
            anchor,
            others,
            startMouseX: ev.clientX,
            currentDelta: 0,
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
    const files = Array.from(ui.fileInput.files ?? []);
    if (files.length === 0) return;
    let succeeded = 0;
    const failures: string[] = [];
    for (const file of files) {
      setStatus(
        files.length > 1
          ? `importing ${file.name} (${succeeded + failures.length + 1}/${files.length})…`
          : `importing ${file.name}…`,
      );
      try {
        const buf = await file.arrayBuffer();
        await client.importWav(file.name, new Uint8Array(buf));
        succeeded++;
      } catch (err) {
        failures.push(`${file.name}: ${String(err)}`);
      }
    }
    ui.fileInput.value = "";
    await refresh();
    opts.onSourceImported?.();
    if (failures.length === 0) {
      setStatus(succeeded === 1 ? `imported ${files[0]!.name}` : `imported ${succeeded} files`);
    } else if (succeeded === 0) {
      setStatus(`import failed: ${failures.join("; ")}`);
    } else {
      setStatus(`imported ${succeeded}, failed ${failures.length}: ${failures.join("; ")}`);
    }
  });

  // ---- track / clip controls -------------------------------------------

  ui.addTrackBtn.addEventListener("click", async () => {
    const n = tracks.length + 1;
    await client.addTrack(`Track ${n}`);
    await refresh();
  });

  ui.deleteClipBtn.addEventListener("click", async () => {
    if (selectedClips.size === 0) return;
    const targets = [...selectedClips].map((k) => {
      const [t, c] = k.split(":");
      return { trackId: Number(t), clipId: Number(c) };
    });
    selectedClips.clear();
    for (const t of targets) {
      try {
        await client.removeClip(t.trackId, t.clipId);
      } catch (err) {
        setStatus(`delete failed: ${String(err)}`);
      }
    }
    await refresh();
  });

  // ---- group / ungroup -------------------------------------------------

  /** Stamp every selected clip with a fresh group id so they move and select
   *  together. If the selection is already part of one or more groups those
   *  are merged into the new id. */
  const groupSelection = async (): Promise<void> => {
    if (selectedClips.size < 2) return;
    const newGroup = nextGroupId();
    const targets = [...selectedClips].map((k) => {
      const [t, c] = k.split(":");
      return { trackId: Number(t), clipId: Number(c) };
    });
    for (const t of targets) {
      try {
        await client.setClipGroup(t.trackId, t.clipId, newGroup);
      } catch (err) {
        setStatus(`group failed: ${String(err)}`);
      }
    }
    setStatus(`grouped ${targets.length} clips`);
    await refresh();
  };

  /** Clear the group id on every selected clip. Members of the same group
   *  not in the selection stay grouped — only the selection is freed. */
  const ungroupSelection = async (): Promise<void> => {
    if (selectedClips.size === 0) return;
    const targets = [...selectedClips].map((k) => {
      const [t, c] = k.split(":");
      return { trackId: Number(t), clipId: Number(c) };
    });
    let n = 0;
    for (const t of targets) {
      try {
        await client.setClipGroup(t.trackId, t.clipId, 0);
        n++;
      } catch (err) {
        setStatus(`ungroup failed: ${String(err)}`);
      }
    }
    setStatus(`ungrouped ${n} clip${n === 1 ? "" : "s"}`);
    await refresh();
  };

  ui.groupBtn.addEventListener("click", () => void groupSelection());
  ui.ungroupBtn.addEventListener("click", () => void ungroupSelection());

  /** Prompt for a new name for the single selected group and persist it. */
  const renameSelectedGroup = async (): Promise<void> => {
    const id = singleSelectedGroup();
    if (id === null) return;
    const current = groupDisplayName(id);
    const next = window.prompt("Rename group", current);
    if (next === null) return;
    const trimmed = next.trim();
    if (trimmed === "" || trimmed === current) return;
    try {
      await client.setGroupName(id, trimmed);
      setStatus(`renamed group to "${trimmed}"`);
      await refresh();
    } catch (err) {
      setStatus(`rename failed: ${String(err)}`);
    }
  };
  ui.renameGroupBtn.addEventListener("click", () => void renameSelectedGroup());

  ui.patternPicker.addEventListener("change", () => syncToolbar());

  /** Stamp a saved pattern at the current playhead (or selection in-point)
   *  as a new named group. Tracks named after each lane are reused if they
   *  already exist; otherwise new tracks are added so re-inserting the same
   *  pattern doesn't keep multiplying drum lanes. */
  const insertSavedPattern = async (): Promise<void> => {
    const name = ui.patternPicker.value;
    if (!name) return;
    const json = await client.loadPattern(name);
    if (!json) {
      setStatus(`pattern "${name}" not found`);
      return;
    }
    let grid: SavedPatternGrid;
    try {
      grid = JSON.parse(json) as SavedPatternGrid;
    } catch (err) {
      setStatus(`pattern parse failed: ${String(err)}`);
      return;
    }
    if (projectSr === 0) {
      setStatus("can't insert — engine has no sample rate yet");
      return;
    }
    const startFrame = inFrame ?? playheadFrame ?? 0;
    // One step is one 16th-note at the *current* project tempo, so changing
    // BPM/time-sig later still gives you a musically aligned insert.
    const stepDurSec = secondsPerBeat() / Math.max(1, grid.stepsPerBeat ?? 4);
    const stepFrames = Math.round(stepDurSec * projectSr);
    // Apply the saved pattern's swing (50–75 = straight..dotted) to odd-
    // indexed steps. Patterns without swing default to straight.
    const swingPct = Math.max(50, Math.min(75, Number(grid.swing ?? 50) || 50));
    const swingFramesFor = (s: number): number => {
      if (s % 2 === 0) return 0;
      const sec = ((swingPct - 50) / 50) * (stepDurSec / 2);
      return Math.round(sec * projectSr);
    };
    const newGroup = nextGroupId();
    await client.setGroupName(newGroup, name);
    let clipCount = 0;
    // Pattern-level shared accent row. Optional in the saved format —
    // older patterns saved before the AC row default to all-off, which
    // means every clip gets the non-accent envelope.
    const accents: boolean[] = Array.isArray(grid.accents) ? grid.accents : [];
    for (const lane of grid.lanes) {
      // New patterns store sourceA/sourceB; legacy patterns store sourceId
      // (and boolean steps). Treat legacy as sourceA so old saves still load.
      const sourceA = lane.sourceA ?? lane.sourceId ?? null;
      const sourceB = lane.sourceB ?? null;
      // Step values: legacy boolean true / numeric 1 → sourceA; numeric 2 →
      // sourceB. Anything else is a rest.
      const hits: Array<{ stepIdx: number; sourceId: string }> = [];
      for (let i = 0; i < lane.steps.length; i++) {
        const v = lane.steps[i];
        const slot = typeof v === "boolean" ? (v ? 1 : 0) : Number(v) || 0;
        if (slot === 1 && sourceA) hits.push({ stepIdx: i, sourceId: sourceA });
        else if (slot === 2 && sourceB) hits.push({ stepIdx: i, sourceId: sourceB });
      }
      if (hits.length === 0) continue;
      // Find or create a track named after this lane. Re-using lets repeat
      // inserts of the same pattern stack onto the same drum tracks rather
      // than fanning out to N copies of "BD".
      const existing = tracks.find((t) => t.name === lane.label);
      const trackId = existing ? existing.id : await client.addTrack(lane.label);
      if (!existing) tracks = await client.listTracks();
      // Per-step microtiming, optional in the saved shape. Clamped to
      // [-0.5, +0.5] of one step so a corrupt save can't fling clips
      // half a bar out of place.
      const laneNudges: number[] = Array.isArray(lane.nudges) ? lane.nudges : [];
      for (const hit of hits) {
        const src = sources.find((s) => s.id === hit.sourceId);
        if (!src) continue;
        const rawNudge = Number(laneNudges[hit.stepIdx] ?? 0) || 0;
        const nudge = Math.max(-0.5, Math.min(0.5, rawNudge));
        const nudgeFrames = Math.round(nudge * stepFrames);
        const positionFrame = Math.max(
          0,
          startFrame +
            hit.stepIdx * stepFrames +
            swingFramesFor(hit.stepIdx) +
            nudgeFrames,
        );
        try {
          const clipId = await client.addClip(trackId, hit.sourceId, positionFrame, 0, src.frames);
          await client.setClipGroup(trackId, clipId, newGroup);
          // Mirror the bake path: non-accented hits get a constant volume
          // envelope at NON_ACCENT_DB; accented hits stay at unity (no
          // envelope) so they stand out in the column.
          if (!(accents[hit.stepIdx] ?? false)) {
            await client.setClipEnvelope(trackId, clipId, "volume", [
              { time: 0, value: NON_ACCENT_DB, curve: "Linear" },
            ]);
          }
          clipCount++;
        } catch (err) {
          console.warn("insertPattern: addClip failed", err);
        }
      }
    }
    setStatus(`inserted "${name}" — ${clipCount} hits`);
    await refresh();
  };
  ui.insertPatternBtn.addEventListener("click", () => void insertSavedPattern());

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

  const setEnvelopeMode = (on: boolean): void => {
    envelopeMode = on;
    ui.envelopeBtn.style.background = on ? "var(--accent-deep)" : "";
    ui.envelopeBtn.style.color = on ? "var(--accent)" : "";
    drawTracks();
  };
  ui.envelopeBtn.addEventListener("click", () => setEnvelopeMode(!envelopeMode));

  // Populate the snap dropdown and wire it. Changing the snap value
  // redraws tracks so the lane grid + ruler reflect the new resolution.
  for (const opt of SNAP_OPTIONS) {
    const o = document.createElement("option");
    o.value = opt.value;
    o.textContent = opt.label;
    ui.snapSelect.appendChild(o);
  }
  ui.snapSelect.value = snapDivision;
  ui.snapSelect.addEventListener("change", () => {
    snapDivision = ui.snapSelect.value as SnapDivision;
    drawTracks();
  });

  const persistTempo = (): Promise<void> =>
    client.setTempo(bpm, beatsPerBar, beatUnit);

  // Persist the new tempo to the engine *before* reflow's refresh() pulls it
  // back in — otherwise refresh() reads the stale value and clobbers the
  // user's edit.
  ui.bpmInput.addEventListener("change", async () => {
    const v = parseFloat(ui.bpmInput.value);
    if (!Number.isFinite(v) || v <= 0) {
      ui.bpmInput.value = String(bpm);
      return;
    }
    const oldSPB = secondsPerBeat();
    const oldBPB = beatsPerBar;
    bpm = v;
    await persistTempo();
    setStatus(`bpm ${bpm}`);
    await reflowForGridChange(oldSPB, oldBPB);
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
    await persistTempo();
    setStatus(`time signature ${beatsPerBar}/${beatUnit}`);
    await reflowForGridChange(oldSPB, oldBPB);
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
    await persistTempo();
    setStatus(`time signature ${beatsPerBar}/${beatUnit}`);
    await reflowForGridChange(oldSPB, oldBPB);
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
    // Marquee drag: grow the rectangle and recompute which clips overlap.
    // baseSelection is the snapshot taken at mousedown — additive mode
    // (Ctrl/Cmd/Shift) keeps it intact and adds intersections on top;
    // replace mode starts empty so the marquee defines the whole selection.
    if (marqueeState) {
      const ms = marqueeState;
      const containerRect = ui.lanesContent.getBoundingClientRect();
      const startX = ms.startClientX - containerRect.left;
      const startY = ms.startClientY - containerRect.top;
      const curX = ev.clientX - containerRect.left;
      const curY = ev.clientY - containerRect.top;
      const left = Math.min(startX, curX);
      const top = Math.min(startY, curY);
      const width = Math.abs(curX - startX);
      const height = Math.abs(curY - startY);
      ms.elt.style.left = `${left}px`;
      ms.elt.style.top = `${top}px`;
      ms.elt.style.width = `${width}px`;
      ms.elt.style.height = `${height}px`;
      if (width + height > 4) ms.moved = true;
      // Hit-test in viewport coords — independent of which container holds
      // the marquee or the clips.
      const rL = Math.min(ms.startClientX, ev.clientX);
      const rT = Math.min(ms.startClientY, ev.clientY);
      const rR = Math.max(ms.startClientX, ev.clientX);
      const rB = Math.max(ms.startClientY, ev.clientY);
      const next = new Set(ms.baseSelection);
      const elts = ui.lanesStack.querySelectorAll<HTMLElement>("[data-clip-key]");
      for (const elt of elts) {
        const key = elt.dataset["clipKey"];
        if (!key) continue;
        const r = elt.getBoundingClientRect();
        const intersects = !(r.right < rL || r.left > rR || r.bottom < rT || r.top > rB);
        if (intersects) next.add(key);
      }
      // If the marquee touched any grouped clip, pull in the rest of the
      // group so the selection always treats a group as one unit.
      expandSelectionToGroups(next);
      // Mutate selectedClips in place so other code reading it sees the
      // live result.
      selectedClips.clear();
      for (const k of next) selectedClips.add(k);
      updateClipSelectionStyles();
      syncToolbar();
      return;
    }
    // Envelope-breakpoint drag: live-update the breakpoint's time + value,
    // pin the dot visually, leave persistence to mouseup.
    if (envelopeDrag) {
      const list = clipsByTrack.get(envelopeDrag.trackId) ?? [];
      const clip = list.find((c) => c.id === envelopeDrag!.clipId);
      const bp = clip?.volumeEnvelope?.[envelopeDrag.bpIndex];
      if (!clip || !bp) {
        envelopeDrag = null;
        return;
      }
      const rect = envelopeDrag.clipDiv.getBoundingClientRect();
      const x = ev.clientX - rect.left;
      const y = ev.clientY - rect.top;
      const w = rect.width;
      const h = rect.height;
      const time = Math.max(
        0,
        Math.min(envelopeDrag.clipFrames - 1, Math.round((x / w) * envelopeDrag.clipFrames)),
      );
      const value = yToDb(y, h);
      // Don't let two breakpoints share a time.
      const env = clip.volumeEnvelope ?? [];
      const collides = env.some(
        (other, idx) => idx !== envelopeDrag!.bpIndex && other.time === time,
      );
      if (collides) {
        bp.value = value;
      } else {
        bp.time = time;
        bp.value = value;
      }
      drawTracks();
      return;
    }
    // Clip drag takes precedence — its mousedown stops propagation, but the
    // window-level mousemove still fires, so we branch on which drag is live.
    if (clipDragState) {
      const ds = clipDragState;
      const deltaPx = ev.clientX - ds.startMouseX;
      const rawDelta = Math.round((deltaPx / pixelsPerSecond) * projectSr);
      // Snap the anchor's destination first; the resulting delta drives the
      // whole group so spacing within the selection is preserved exactly.
      let anchorNew = ds.anchor.startPositionFrame + rawDelta;
      if (!ev.shiftKey) anchorNew = snapFrames(anchorNew);
      anchorNew = Math.max(0, anchorNew);
      let actualDelta = anchorNew - ds.anchor.startPositionFrame;
      // Clamp so the leftmost clip in the cohort can't slide past frame 0.
      let minStart = ds.anchor.startPositionFrame;
      for (const o of ds.others) {
        if (o.startPositionFrame < minStart) minStart = o.startPositionFrame;
      }
      if (minStart + actualDelta < 0) actualDelta = -minStart;
      if (Math.abs(deltaPx) > 3) ds.moved = true;
      ds.currentDelta = actualDelta;
      ds.anchor.elt.style.left = `${framesToPx(ds.anchor.startPositionFrame + actualDelta)}px`;
      for (const o of ds.others) {
        o.elt.style.left = `${framesToPx(o.startPositionFrame + actualDelta)}px`;
      }
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
    if (marqueeState) {
      const ms = marqueeState;
      marqueeState = null;
      ms.elt.remove();
      // Final styling pass — drawTracks would also recolor waveforms, but
      // it's heavy. The lighter restyle keeps borders/backgrounds in sync;
      // a future drawTracks (e.g. on the next add/move) will catch up the
      // waveform tint.
      updateClipSelectionStyles();
      syncToolbar();
      const status =
        selectedClips.size === 0
          ? "selection cleared"
          : selectedClips.size === 1
            ? "1 clip selected"
            : `${selectedClips.size} clips selected`;
      setStatus(status);
      return;
    }
    if (envelopeDrag) {
      const ed = envelopeDrag;
      envelopeDrag = null;
      const list = clipsByTrack.get(ed.trackId) ?? [];
      const clip = list.find((c) => c.id === ed.clipId);
      if (clip) {
        // Re-sort because the dragged breakpoint may have crossed others
        // in time. The engine requires strictly-increasing times.
        const env = (clip.volumeEnvelope ?? []).slice().sort((a, b) => a.time - b.time);
        await persistEnvelope(ed.trackId, ed.clipId, env);
      }
      return;
    }
    if (clipDragState) {
      const ds = clipDragState;
      clipDragState = null;
      const droppedOnBin = binHovered;
      setBinHover(false);
      const cohort = [ds.anchor, ...ds.others];
      if (droppedOnBin) {
        let removed = 0;
        for (const d of cohort) {
          try {
            await client.removeClip(d.trackId, d.clipId);
            selectedClips.delete(clipKey(d.trackId, d.clipId));
            removed++;
          } catch (err) {
            setStatus(`delete failed: ${String(err)}`);
          }
        }
        setStatus(removed === 1 ? "clip binned" : `${removed} clips binned`);
        await refresh();
        return;
      }
      if (ds.moved) {
        let moved = 0;
        for (const d of cohort) {
          const newPos = d.startPositionFrame + ds.currentDelta;
          try {
            await client.moveClip(d.trackId, d.clipId, newPos);
            moved++;
          } catch (err) {
            setStatus(`move clip failed: ${String(err)}`);
          }
        }
        const anchorPos = ds.anchor.startPositionFrame + ds.currentDelta;
        const beats = anchorPos / Math.max(1, projectSr) / Math.max(1e-6, secondsPerBeat());
        const where =
          gridMode === "beats"
            ? `bar ${Math.floor(beats / beatsPerBar) + 1} beat ${(beats % beatsPerBar) + 1}`
            : `${(anchorPos / projectSr).toFixed(3)}s`;
        setStatus(moved === 1 ? `moved to ${where}` : `moved ${moved} clips (anchor → ${where})`);
        await refresh();
      }
      // No-move case: selection was already updated on mousedown.
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

  ui.renderToClipBtn.addEventListener("click", async () => {
    if (!hasSelection()) return;
    const start = inFrame!;
    const end = outFrame!;
    ui.renderToClipBtn.disabled = true;
    setStatus("rendering selection…");
    try {
      const newId = await client.renderRangeToSource(start, end, "Render.wav");
      const seconds = (end - start) / Math.max(1, projectSr);
      setStatus(`rendered ${seconds.toFixed(2)}s → ${newId}`);
      await refresh();
      opts.onSourceImported?.();
    } catch (err) {
      setStatus(`render failed: ${String(err)}`);
    } finally {
      ui.renderToClipBtn.disabled = !hasSelection();
    }
  });

  ui.copySelBtn.addEventListener("click", () => {
    if (!hasSelection()) return;
    const selStart = inFrame!;
    const selEnd = outFrame!;
    const entries: ClipboardEntry[] = [];
    for (const [trackId, list] of clipsByTrack) {
      for (const c of list) {
        const overlapStart = Math.max(c.position, selStart);
        const overlapEnd = Math.min(c.endPosition, selEnd);
        if (overlapEnd <= overlapStart) continue;
        // Map the overlap back into the source's frame range. Sources are
        // resampled to the project rate at import time and time_stretch
        // defaults to 1, so 1 project frame == 1 source frame here.
        const sourceIn = c.sourceIn + (overlapStart - c.position);
        const sourceOut = c.sourceIn + (overlapEnd - c.position);
        entries.push({
          trackId,
          sourceId: c.sourceId,
          sourceIn,
          sourceOut,
          offset: overlapStart - selStart,
        });
      }
    }
    clipboardEntries = entries;
    clipboardLength = selEnd - selStart;
    ui.insertCopiesBtn.disabled = entries.length === 0 || !hasSelection();
    setStatus(
      entries.length === 0
        ? "copy: selection contains no clips"
        : `copied ${entries.length} clip${entries.length === 1 ? "" : "s"} (${(clipboardLength / Math.max(1, projectSr)).toFixed(2)}s)`,
    );
  });

  ui.insertCopiesBtn.addEventListener("click", async () => {
    if (!hasSelection() || clipboardEntries.length === 0 || clipboardLength <= 0) return;
    const at = inFrame!;
    const n = Math.max(1, Math.min(256, Math.floor(parseFloat(ui.insertCountInput.value) || 1)));
    ui.insertCopiesBtn.disabled = true;
    setStatus(`inserting ${n} cop${n === 1 ? "y" : "ies"}…`);
    try {
      let added = 0;
      for (let k = 0; k < n; k++) {
        for (const e of clipboardEntries) {
          // Skip if the source no longer exists (e.g. user deleted it
          // between copy and insert).
          if (!sources.find((s) => s.id === e.sourceId)) continue;
          const position = at + k * clipboardLength + e.offset;
          try {
            await client.addClip(e.trackId, e.sourceId, position, e.sourceIn, e.sourceOut);
            added++;
          } catch (err) {
            // Track may have been removed; carry on with remaining entries.
            console.warn("insert ×N: addClip failed", err);
          }
        }
      }
      await refresh();
      setStatus(`inserted ${added} clip${added === 1 ? "" : "s"} (${n}× ${clipboardEntries.length})`);
    } catch (err) {
      setStatus(`insert failed: ${String(err)}`);
    } finally {
      ui.insertCopiesBtn.disabled = clipboardEntries.length === 0 || !hasSelection();
    }
  });

  // Delete (not Backspace — browsers map that to navigate-back) removes every
  // selected clip. Skip when typing in inputs so digits can be edited freely.
  // G groups the selection, Shift+G ungroups it.
  window.addEventListener("keydown", (ev) => {
    const t = ev.target as HTMLElement | null;
    const inEditableField =
      t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable);
    if (inEditableField) return;
    if (ev.key === "g" || ev.key === "G") {
      if (selectedClips.size === 0) return;
      ev.preventDefault();
      void (ev.shiftKey ? ungroupSelection() : groupSelection());
      return;
    }
    if (ev.key !== "Delete") return;
    if (selectedClips.size === 0) return;
    ev.preventDefault();
    const targets = [...selectedClips].map((k) => {
      const [tid, cid] = k.split(":");
      return { trackId: Number(tid), clipId: Number(cid) };
    });
    selectedClips.clear();
    void (async () => {
      let removed = 0;
      for (const target of targets) {
        try {
          await client.removeClip(target.trackId, target.clipId);
          removed++;
        } catch (err) {
          setStatus(`delete failed: ${String(err)}`);
        }
      }
      await refresh();
      setStatus(removed === 1 ? "clip deleted" : `${removed} clips deleted`);
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
  groupBtn: HTMLButtonElement;
  ungroupBtn: HTMLButtonElement;
  renameGroupBtn: HTMLButtonElement;
  patternPicker: HTMLSelectElement;
  insertPatternBtn: HTMLButtonElement;
  modeBeatsBtn: HTMLButtonElement;
  modeTimeBtn: HTMLButtonElement;
  snapSelect: HTMLSelectElement;
  bpmInput: HTMLInputElement;
  beatsPerBarInput: HTMLInputElement;
  beatUnitInput: HTMLInputElement;
  zoomInBtn: HTMLButtonElement;
  zoomOutBtn: HTMLButtonElement;
  zoomFitBtn: HTMLButtonElement;
  envelopeBtn: HTMLButtonElement;
  playBtn: HTMLButtonElement;
  loopBtn: HTMLButtonElement;
  stopBtn: HTMLButtonElement;
  clearSelBtn: HTMLButtonElement;
  renderToClipBtn: HTMLButtonElement;
  copySelBtn: HTMLButtonElement;
  insertCountInput: HTMLInputElement;
  insertCopiesBtn: HTMLButtonElement;
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
  fileInput.multiple = true;
  fileRow.appendChild(fileInput);
  root.appendChild(fileRow);

  // Two-pane: narrow Sources column + tracks pane.
  // flex: 1 + minHeight: 0 lets the split absorb the arranger's vertical
  // space; without those, a long source list pushes the transport bar
  // and status line off the bottom of the viewport.
  const split = document.createElement("div");
  Object.assign(split.style, {
    display: "flex",
    gap: "12px",
    flex: "1 1 auto",
    minHeight: "0",
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
  deleteClipBtn.title =
    "Click to bin selected clips — or drag a clip here. Ctrl/Cmd-click clips to multi-select.";
  const groupBtn = makeToolbarBtn("Group");
  groupBtn.disabled = true;
  groupBtn.title = "Group selected clips so they move and select together (G)";
  const ungroupBtn = makeToolbarBtn("Ungroup");
  ungroupBtn.disabled = true;
  ungroupBtn.title = "Remove selected clips from their group (Shift+G)";
  const renameGroupBtn = makeToolbarBtn("Rename");
  renameGroupBtn.disabled = true;
  renameGroupBtn.title = "Rename the selected group";

  const patternPicker = document.createElement("select");
  Object.assign(patternPicker.style, {
    padding: "4px 6px",
    background: "var(--bg-sunken)",
    color: "var(--text-2)",
    border: "1px solid var(--line-2)",
    borderRadius: "var(--r-2)",
    fontSize: "12px",
    fontFamily: "inherit",
  } satisfies Partial<CSSStyleDeclaration>);
  patternPicker.title = "Saved drum patterns";
  const insertPatternBtn = makeToolbarBtn("Insert pattern");
  insertPatternBtn.disabled = true;
  insertPatternBtn.title = "Stamp the selected pattern at the playhead as a new group";

  const sep1 = makeSep();
  const gridLabel = document.createElement("span");
  gridLabel.textContent = "Grid:";
  gridLabel.style.color = "#aaa";
  const modeBeatsBtn = makeToolbarBtn("Beats");
  const modeTimeBtn = makeToolbarBtn("Time");

  const sepSnap = makeSep();
  const snapLabel = document.createElement("span");
  snapLabel.textContent = "Snap:";
  snapLabel.style.color = "#aaa";
  const snapSelect = document.createElement("select");
  Object.assign(snapSelect.style, {
    padding: "4px 6px",
    background: "var(--bg-sunken)",
    color: "var(--text-2)",
    border: "1px solid var(--line-2)",
    borderRadius: "var(--r-2)",
    fontSize: "12px",
    fontFamily: "inherit",
  } satisfies Partial<CSSStyleDeclaration>);

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

  const sepEnv = makeSep();
  const envelopeBtn = makeToolbarBtn("Envelopes");
  envelopeBtn.title =
    "Toggle envelope edit mode. Click on a clip to add a volume breakpoint, drag to move, double-click to delete.";

  toolbar.appendChild(tracksTitle);
  toolbar.appendChild(addTrackBtn);
  toolbar.appendChild(deleteClipBtn);
  toolbar.appendChild(groupBtn);
  toolbar.appendChild(ungroupBtn);
  toolbar.appendChild(renameGroupBtn);
  toolbar.appendChild(patternPicker);
  toolbar.appendChild(insertPatternBtn);
  toolbar.appendChild(sep1);
  toolbar.appendChild(gridLabel);
  toolbar.appendChild(modeBeatsBtn);
  toolbar.appendChild(modeTimeBtn);
  toolbar.appendChild(sepSnap);
  toolbar.appendChild(snapLabel);
  toolbar.appendChild(snapSelect);
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
  toolbar.appendChild(sepEnv);
  toolbar.appendChild(envelopeBtn);
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

  const renderToClipBtn = document.createElement("button");
  renderToClipBtn.textContent = "Render to clip";
  renderToClipBtn.type = "button";
  renderToClipBtn.title =
    "Mix down the selection and add the result as a new source in the library";
  renderToClipBtn.disabled = true;
  Object.assign(renderToClipBtn.style, btnStyle());
  transport.appendChild(renderToClipBtn);

  // Copy / paste-N controls — capture the clips inside the selection and
  // stamp them N times at the in-point as a quick loop tool.
  const copySelBtn = document.createElement("button");
  copySelBtn.textContent = "Copy Sel";
  copySelBtn.type = "button";
  copySelBtn.title =
    "Capture every clip overlapping the selection (cropped to the selection bounds)";
  copySelBtn.disabled = true;
  Object.assign(copySelBtn.style, btnStyle());
  transport.appendChild(copySelBtn);

  const insertCountInput = document.createElement("input");
  insertCountInput.type = "number";
  insertCountInput.min = "1";
  insertCountInput.max = "256";
  insertCountInput.step = "1";
  insertCountInput.value = "4";
  insertCountInput.title = "Number of copies to stamp at the in-point";
  Object.assign(insertCountInput.style, {
    width: "56px",
    padding: "4px 6px",
    background: "var(--bg-sunken)",
    color: "var(--text-2)",
    border: "1px solid var(--line-2)",
    borderRadius: "var(--r-2)",
    fontFamily: "var(--ff-mono)",
    fontSize: "12px",
  } satisfies Partial<CSSStyleDeclaration>);
  transport.appendChild(insertCountInput);

  const insertCopiesBtn = document.createElement("button");
  insertCopiesBtn.textContent = "Insert ×N";
  insertCopiesBtn.type = "button";
  insertCopiesBtn.title =
    "Stamp the captured selection N times starting at the current in-point";
  insertCopiesBtn.disabled = true;
  Object.assign(insertCopiesBtn.style, btnStyle());
  transport.appendChild(insertCopiesBtn);

  const hint = document.createElement("span");
  hint.textContent =
    "(drag the ruler to make a selection, then drag its green edges to refine — Shift bypasses snap)";
  hint.style.color = "#9a9a9a";
  hint.style.fontSize = "12px";
  transport.appendChild(hint);
  root.appendChild(transport);

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
    groupBtn,
    ungroupBtn,
    renameGroupBtn,
    patternPicker,
    insertPatternBtn,
    modeBeatsBtn,
    modeTimeBtn,
    snapSelect,
    bpmInput,
    beatsPerBarInput,
    beatUnitInput,
    zoomInBtn,
    zoomOutBtn,
    zoomFitBtn,
    envelopeBtn,
    playBtn,
    loopBtn,
    stopBtn,
    clearSelBtn,
    renderToClipBtn,
    copySelBtn,
    insertCountInput,
    insertCopiesBtn,
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
