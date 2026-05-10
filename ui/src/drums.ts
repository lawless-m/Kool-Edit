// Drum-pattern entry screen. Vintage step-sequencer layout: a column per
// step, a row per "lane" (a slot bound to a source from the library). Click
// cells to toggle hits, hit Play to audition through Web Audio (sample-
// accurate scheduling, no round-trip through the engine), hit Bake to
// materialise the pattern as clips on new arranger tracks.
//
// The pattern itself is just `lanes[i].steps[j] === true` for every active
// hit; the real work is keeping AudioBuffers cached per assigned source
// and turning the grid into either scheduled sources (preview) or addClip
// calls (bake). The engine exposes `querySamples`, which returns
// interleaved float samples — we wrap that into an AudioBuffer once per
// sourceId and keep it for the session.
//
// Live preview uses one BufferSource per scheduled hit so overlaps ring
// out naturally. There's no per-lane choke yet — drum samples are short
// enough that this matches the bake's behaviour and keeps the scheduler
// trivial.

import type { EngineClient, SourceInfo } from "./engine/client";

export interface DrumsOptions {
  onSourceImported?: () => void;
  onArrangerNeedsRefresh?: () => void;
}

// Step values:
//   0 = off
//   1 = primary source ("X" — white cell)
//   2 = secondary source ("x" — grey cell)
// Each lane has two source slots (sourceA / sourceB). Click cycles
// off → A → B → off so the user can layer two timbres on one lane —
// e.g. open + closed hat, soft + hard kick, ghost notes vs accents.
type StepValue = 0 | 1 | 2;

interface Lane {
  label: string;
  sourceA: string | null;
  sourceB: string | null;
  steps: StepValue[];
}

/** Wire format for a saved drum pattern. Stored as opaque JSON in the
 *  engine's `Project::patterns`. The arranger uses this to re-stamp a
 *  pattern at a new playhead position. `stepsPerBeat` is captured so the
 *  pattern survives changes to step resolution; the actual duration of one
 *  step is recomputed from the current project tempo on insert.
 *
 *  Older patterns saved before dual-source lanes use `sourceId` and
 *  boolean steps; loadSavedPattern reads both shapes. */
export interface SavedPatternGrid {
  lanes: Array<{
    label: string;
    sourceA?: string | null;
    sourceB?: string | null;
    sourceId?: string | null; // legacy
    steps: Array<number | boolean>;
  }>;
  stepCount: number;
  stepsPerBeat: number;
}

// Ordered low-to-high so the kit stacks from the deepest sound at the
// bottom (BD) up through the brighter ones (cymbals at the top), matching
// drum-kit ergonomics and most hardware drum machines' lane layout. Eight
// slots gives room for toms + cowbell out of the box; the user can add /
// remove more from the toolbar.
const DEFAULT_LANE_LABELS = ["CY", "OH", "CP", "HH", "CB", "TT", "SD", "BD"];
// One cell = one 16th note. 4 beats per bar × 4 sixteenths per beat = 16
// cells per bar (matches the Oberheim DMX layout). The + button extends
// the pattern by BARS_PER_EXTEND bars per click.
const STEPS_PER_GROUP = 4;
const STEPS_PER_BAR = STEPS_PER_GROUP * 4;
const BARS_PER_EXTEND = 1;
const DEFAULT_STEPS = STEPS_PER_BAR;

export interface DrumsHandle {
  refresh(): Promise<void>;
}

export async function mountDrums(
  root: HTMLElement,
  client: EngineClient,
  opts: DrumsOptions = {},
): Promise<DrumsHandle> {
  root.style.display = "flex";
  root.style.flexDirection = "column";
  root.style.minHeight = "0";
  root.style.flex = "1 1 auto";
  root.style.overflow = "hidden";
  root.style.background = "#0e0e0e";
  root.style.color = "#d8d8d8";
  root.style.fontFamily = "system-ui, sans-serif";
  root.innerHTML = "";

  let sources: SourceInfo[] = [];
  let bpm = 120;
  let beatsPerBar = 4;
  let beatUnit = 4;
  // 16 steps = 1 bar at 16th-note resolution when beatsPerBar=4. We treat
  // a "step" as one 16th regardless of the project signature so the bar
  // count always works out cleanly for the user's 16-step pattern.
  const stepsPerBeat = 4;
  // Mutable so the + button can extend the pattern. Every lane carries
  // exactly this many step values; growStepCount keeps them in sync.
  let stepCount = DEFAULT_STEPS;

  const lanes: Lane[] = DEFAULT_LANE_LABELS.map((label) => ({
    label,
    sourceA: null,
    sourceB: null,
    steps: new Array<StepValue>(stepCount).fill(0),
  }));

  // AudioBuffer cache keyed by sourceId. Populated lazily on first preview
  // or pad-tap; invalidated when the assigned source changes.
  const bufferCache = new Map<string, AudioBuffer>();
  let audioCtx: AudioContext | null = null;
  // Active scheduled previews so Stop can cancel anything still queued.
  let scheduledNodes: AudioBufferSourceNode[] = [];
  let previewLoopTimer: number | null = null;
  let previewStartCtxTime = 0;
  // The step the playhead is currently lighting up. -1 when idle.
  let activeStep = -1;
  let stepRaf: number | null = null;

  // ---- chrome ----------------------------------------------------------

  const toolbar = document.createElement("div");
  Object.assign(toolbar.style, {
    display: "flex",
    alignItems: "center",
    gap: "8px",
    padding: "8px 12px",
    borderBottom: "1px solid #222",
    background: "#151515",
    flexShrink: "0",
  } satisfies Partial<CSSStyleDeclaration>);

  const partLabel = document.createElement("div");
  Object.assign(partLabel.style, {
    background: "#888",
    color: "#000",
    padding: "2px 10px",
    fontWeight: "600",
    fontSize: "12px",
    letterSpacing: "0.5px",
  } satisfies Partial<CSSStyleDeclaration>);
  partLabel.textContent = "PATTERN 1";
  toolbar.appendChild(partLabel);

  const bpmDisplay = document.createElement("div");
  bpmDisplay.style.fontSize = "13px";
  bpmDisplay.style.color = "#d8d8d8";
  bpmDisplay.textContent = "— BPM";
  toolbar.appendChild(bpmDisplay);

  // Live readout of the current pattern length in bars. Driven by
  // updateLengthDisplay() whenever stepCount changes.
  const lengthDisplay = document.createElement("div");
  lengthDisplay.style.fontSize = "12px";
  lengthDisplay.style.color = "#9a9a9a";
  toolbar.appendChild(lengthDisplay);

  const addBarsBtn = makeBtn(`+ ${BARS_PER_EXTEND} bar${BARS_PER_EXTEND === 1 ? "" : "s"}`);
  addBarsBtn.title = `Extend the pattern by ${BARS_PER_EXTEND} bar${BARS_PER_EXTEND === 1 ? "" : "s"}`;
  toolbar.appendChild(addBarsBtn);

  const addLaneBtn = makeBtn("+ lane");
  addLaneBtn.title = "Append an empty lane to the bottom of the kit";
  toolbar.appendChild(addLaneBtn);

  const spacer = document.createElement("div");
  spacer.style.flex = "1";
  toolbar.appendChild(spacer);

  // Pattern name — used as the group name on bake and as the storage key
  // when saving to the project's pattern library.
  const nameInput = document.createElement("input");
  nameInput.type = "text";
  nameInput.placeholder = "name";
  nameInput.value = "Part 1";
  Object.assign(nameInput.style, {
    width: "100px",
    background: "#1a1a1a",
    color: "#d8d8d8",
    border: "1px solid #333",
    padding: "3px 6px",
    fontSize: "12px",
    borderRadius: "3px",
  } satisfies Partial<CSSStyleDeclaration>);
  toolbar.appendChild(nameInput);

  // Pattern picker for loading saved patterns back into the editor.
  const patternPicker = document.createElement("select");
  Object.assign(patternPicker.style, {
    background: "#1a1a1a",
    color: "#d8d8d8",
    border: "1px solid #333",
    padding: "3px 6px",
    fontSize: "12px",
    borderRadius: "3px",
  } satisfies Partial<CSSStyleDeclaration>);
  toolbar.appendChild(patternPicker);
  const loadBtn = makeBtn("Load");
  toolbar.appendChild(loadBtn);

  const playBtn = makeBtn("Play");
  const stopBtn = makeBtn("Stop");
  stopBtn.disabled = true;
  const clearBtn = makeBtn("Clear");
  const saveBtn = makeBtn("Save");
  const bakeBtn = makeBtn("Bake to tracks");
  toolbar.appendChild(playBtn);
  toolbar.appendChild(stopBtn);
  toolbar.appendChild(clearBtn);
  toolbar.appendChild(saveBtn);
  toolbar.appendChild(bakeBtn);

  root.appendChild(toolbar);

  const status = document.createElement("div");
  Object.assign(status.style, {
    padding: "4px 12px",
    fontSize: "11px",
    color: "#9a9a9a",
    background: "#0c0c0c",
    borderBottom: "1px solid #1c1c1c",
    minHeight: "20px",
    flexShrink: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  root.appendChild(status);

  const setStatus = (msg: string): void => {
    status.textContent = msg;
  };

  // ---- grid ------------------------------------------------------------

  const grid = document.createElement("div");
  Object.assign(grid.style, {
    flex: "1 1 auto",
    overflow: "auto",
    padding: "16px",
    minHeight: "0",
  } satisfies Partial<CSSStyleDeclaration>);
  root.appendChild(grid);

  const drawGrid = (): void => {
    grid.innerHTML = "";

    // Beat labels are anchored DIRECTLY to the cell buttons of the first
    // lane (position: absolute children of each downbeat cell). They live
    // in the cell's own positioning context, so whatever flex / overflow /
    // scroll behaviour places the cell also places the label — they
    // cannot drift apart, even when the row scrolls off-screen.

    for (let li = 0; li < lanes.length; li++) {
      const lane = lanes[li]!;
      const row = document.createElement("div");
      Object.assign(row.style, {
        display: "flex",
        alignItems: "center",
        gap: "8px",
        marginBottom: "6px",
        // First row gets headroom so the per-cell labels can render above
        // the cells without overlapping the toolbar / status row.
        marginTop: li === 0 ? "16px" : "0",
      } satisfies Partial<CSSStyleDeclaration>);

      // Editable label so the user can rename "HH" to "tom" etc. without a
      // separate dialog. Blur commits.
      const label = document.createElement("input");
      label.value = lane.label;
      Object.assign(label.style, {
        width: "44px",
        // border-box keeps the actual layout width at 44px regardless of
        // padding+border; otherwise the input would be 10px wider than the
        // header's 44px spacer and shove every cell out of alignment.
        boxSizing: "border-box",
        background: "transparent",
        color: "#d8d8d8",
        border: "1px solid transparent",
        fontSize: "13px",
        fontWeight: "600",
        textAlign: "right",
        padding: "2px 4px",
      } satisfies Partial<CSSStyleDeclaration>);
      label.addEventListener("change", () => {
        lane.label = label.value.trim() || lane.label;
      });
      row.appendChild(label);

      // Two source pickers per lane (A = white "X" cells, B = grey "x"
      // cells). Each cell cycles off → A → B → off so the lane can layer
      // two timbres on the same time grid (e.g. open vs closed hat,
      // accents vs ghost notes). Either picker can stay unassigned —
      // preview/bake just skip cells whose source is missing.
      const makePicker = (
        which: "A" | "B",
        currentValue: string | null,
        onChange: (id: string | null) => void,
      ): HTMLSelectElement => {
        const sel = document.createElement("select");
        Object.assign(sel.style, {
          width: "120px",
          boxSizing: "border-box",
          background: "#1a1a1a",
          // The select chip echoes the cell colour so the user can tell
          // which slot drives the white vs grey cells at a glance.
          color: which === "A" ? "#e6e6e6" : "#aaaaaa",
          border: which === "A" ? "1px solid #555" : "1px solid #3a3a3a",
          padding: "2px 4px",
          fontSize: "12px",
        } satisfies Partial<CSSStyleDeclaration>);
        const noneOpt = document.createElement("option");
        noneOpt.value = "";
        noneOpt.textContent = which === "A" ? "— X —" : "— x —";
        sel.appendChild(noneOpt);
        for (const s of sources) {
          const o = document.createElement("option");
          o.value = s.id;
          o.textContent = s.name;
          sel.appendChild(o);
        }
        sel.value = currentValue ?? "";
        sel.addEventListener("change", () => {
          const v = sel.value || null;
          onChange(v);
          if (v) void prefetchBuffer(v);
        });
        return sel;
      };
      row.appendChild(makePicker("A", lane.sourceA, (id) => (lane.sourceA = id)));
      row.appendChild(makePicker("B", lane.sourceB, (id) => (lane.sourceB = id)));

      // Per-lane remove button. Disabled when there's only one lane left
      // so the user can't accidentally empty the kit.
      const removeBtn = document.createElement("button");
      removeBtn.type = "button";
      removeBtn.textContent = "×";
      removeBtn.title = "Remove this lane";
      Object.assign(removeBtn.style, {
        width: "20px",
        height: "20px",
        boxSizing: "border-box",
        background: "transparent",
        color: "#7a7a7a",
        border: "1px solid #333",
        borderRadius: "3px",
        cursor: lanes.length > 1 ? "pointer" : "default",
        fontSize: "14px",
        lineHeight: "16px",
        padding: "0",
        flexShrink: "0",
        opacity: lanes.length > 1 ? "1" : "0.3",
      } satisfies Partial<CSSStyleDeclaration>);
      removeBtn.disabled = lanes.length <= 1;
      removeBtn.addEventListener("click", () => {
        if (lanes.length <= 1) return;
        if (previewLoopTimer !== null) stopPreview();
        lanes.splice(li, 1);
        drawGrid();
        setStatus(`removed lane (${lanes.length} remaining)`);
      });
      row.appendChild(removeBtn);

      // Step cells, broken into groups of 4 with a vertical gutter so the
      // user can count beats by eye (matches the Oberheim DMX layout).
      const cells = document.createElement("div");
      Object.assign(cells.style, {
        display: "flex",
        gap: "0",
        flex: "1",
      } satisfies Partial<CSSStyleDeclaration>);
      for (let s = 0; s < lane.steps.length; s++) {
        if (s > 0 && s % STEPS_PER_GROUP === 0) {
          const gutter = document.createElement("div");
          gutter.style.width = "10px";
          gutter.style.flexShrink = "0";
          cells.appendChild(gutter);
        }
        const cell = document.createElement("button");
        cell.type = "button";
        cell.dataset["lane"] = String(li);
        cell.dataset["step"] = String(s);
        const value: StepValue = lane.steps[s] ?? 0;
        const isActiveStep = activeStep === s;
        // Three-state colouring: off = dark, X = white, x = grey.
        const cellBg =
          value === 1 ? "#ffffff" : value === 2 ? "#7a7a7a" : "#0e0e0e";
        Object.assign(cell.style, {
          width: "26px",
          height: "32px",
          // border-box so the 1px border doesn't push each cell to 28px and
          // drift the row out of alignment with the bar.beat header.
          boxSizing: "border-box",
          margin: "0 2px",
          borderRadius: "3px",
          border: isActiveStep ? "1px solid #ffe066" : "1px solid #555",
          background: cellBg,
          cursor: "pointer",
          padding: "0",
          flexShrink: "0",
          // Establishes the positioning context for the bar.beat label
          // attached to first-row downbeat cells.
          position: "relative",
        } satisfies Partial<CSSStyleDeclaration>);
        // Anchor the bar.beat label to the first lane's downbeat cells so
        // the label and cell share a positioning context — they move
        // together no matter what flex/scroll does to the row.
        if (li === 0 && s % STEPS_PER_GROUP === 0) {
          const bar = Math.floor(s / STEPS_PER_BAR) + 1;
          const beat = Math.floor((s % STEPS_PER_BAR) / STEPS_PER_GROUP) + 1;
          const labelOnCell = document.createElement("span");
          labelOnCell.textContent = `${bar}.${beat}`;
          Object.assign(labelOnCell.style, {
            position: "absolute",
            top: "-14px",
            left: "0",
            fontSize: "10px",
            color: beat === 1 ? "#d8d8d8" : "#7a7a7a",
            fontWeight: beat === 1 ? "600" : "400",
            pointerEvents: "none",
            whiteSpace: "nowrap",
            fontFamily: "inherit",
          } satisfies Partial<CSSStyleDeclaration>);
          cell.appendChild(labelOnCell);
        }
        cell.addEventListener("click", () => {
          // Cycle through off → A → B → off so a single click advances and
          // a third click clears the cell.
          const next: StepValue = (((lane.steps[s] ?? 0) + 1) % 3) as StepValue;
          lane.steps[s] = next;
          drawGrid();
          // Audition the cell on the way up so the user hears what they
          // just dialled in.
          const pickedSource =
            next === 1 ? lane.sourceA : next === 2 ? lane.sourceB : null;
          if (pickedSource) void triggerOneShot(pickedSource);
        });
        cells.appendChild(cell);
      }
      row.appendChild(cells);
      grid.appendChild(row);
    }
  };

  // ---- buffer cache + preview -----------------------------------------

  const ensureCtx = (): AudioContext => {
    if (!audioCtx) audioCtx = new AudioContext();
    if (audioCtx.state === "suspended") void audioCtx.resume();
    return audioCtx;
  };

  /** Load a source into an AudioBuffer once and remember it. Returns
   *  null if the source isn't known to the engine (e.g. just deleted). */
  const prefetchBuffer = async (sourceId: string): Promise<AudioBuffer | null> => {
    const cached = bufferCache.get(sourceId);
    if (cached) return cached;
    const src = sources.find((s) => s.id === sourceId);
    if (!src) return null;
    const { samples, channels } = await client.querySamples(sourceId, 0, src.frames);
    const ctx = ensureCtx();
    // querySamples returns interleaved planar — frame-major, channel-minor
    // for multi-channel sources. AudioBuffer wants per-channel arrays, so
    // we de-interleave. Mono sources stream straight through.
    const buf = ctx.createBuffer(channels, src.frames, src.sampleRate);
    if (channels === 1) {
      buf.getChannelData(0).set(samples);
    } else {
      for (let c = 0; c < channels; c++) {
        const out = buf.getChannelData(c);
        for (let i = 0; i < src.frames; i++) out[i] = samples[i * channels + c]!;
      }
    }
    bufferCache.set(sourceId, buf);
    return buf;
  };

  const triggerOneShot = async (sourceId: string): Promise<void> => {
    const buf = await prefetchBuffer(sourceId);
    if (!buf) return;
    const ctx = ensureCtx();
    const node = ctx.createBufferSource();
    node.buffer = buf;
    node.connect(ctx.destination);
    node.start();
  };

  const stepDurationSec = (): number => {
    // beatUnit-aware: at beatUnit=4 (quarter), one beat = 60/bpm sec, so
    // a step (16th) = 60/bpm/4 sec. With other denominators, we keep the
    // "16th-note" interpretation by scaling against a quarter-note baseline.
    const quarterDurSec = 60 / Math.max(1, bpm);
    const beatDurSec = quarterDurSec * (4 / Math.max(1, beatUnit));
    return beatDurSec / stepsPerBeat;
  };

  const patternDurationSec = (): number => stepDurationSec() * stepCount;

  /** Schedule one full pattern pass starting at `startCtxTime`. Returns
   *  the array of sources scheduled so callers can cancel mid-flight. */
  const scheduleOnePass = async (startCtxTime: number): Promise<AudioBufferSourceNode[]> => {
    const ctx = ensureCtx();
    const stepDur = stepDurationSec();
    const out: AudioBufferSourceNode[] = [];
    // Resolve both sources up-front per lane so scheduling is synchronous
    // (otherwise async waits would push hits late). Either slot can be
    // null; cells whose slot is unassigned are silent.
    const lanesWithBufs: Array<{
      lane: Lane;
      bufA: AudioBuffer | null;
      bufB: AudioBuffer | null;
    }> = [];
    for (const lane of lanes) {
      const bufA = lane.sourceA ? await prefetchBuffer(lane.sourceA) : null;
      const bufB = lane.sourceB ? await prefetchBuffer(lane.sourceB) : null;
      if (bufA || bufB) lanesWithBufs.push({ lane, bufA, bufB });
    }
    for (let s = 0; s < stepCount; s++) {
      const t = startCtxTime + s * stepDur;
      for (const { lane, bufA, bufB } of lanesWithBufs) {
        const v = lane.steps[s] ?? 0;
        const buf = v === 1 ? bufA : v === 2 ? bufB : null;
        if (!buf) continue;
        const node = ctx.createBufferSource();
        node.buffer = buf;
        node.connect(ctx.destination);
        node.start(t);
        out.push(node);
      }
    }
    return out;
  };

  const startPreview = async (): Promise<void> => {
    if (previewLoopTimer !== null) return;
    const ctx = ensureCtx();
    const startAt = ctx.currentTime + 0.05;
    previewStartCtxTime = startAt;
    scheduledNodes = await scheduleOnePass(startAt);
    // Reschedule each pattern so loops are gapless. We rearm slightly
    // before the previous pass ends so the next batch is queued before
    // the audio clock catches up.
    const reschedule = async (): Promise<void> => {
      previewStartCtxTime += patternDurationSec();
      const nextNodes = await scheduleOnePass(previewStartCtxTime);
      scheduledNodes = scheduledNodes.concat(nextNodes);
    };
    const passMs = patternDurationSec() * 1000;
    previewLoopTimer = window.setInterval(() => {
      void reschedule();
      // Drop nodes that have already finished playing — keeps the array
      // from growing unboundedly during long previews.
      const cutoff = ctx.currentTime;
      scheduledNodes = scheduledNodes.filter((n) => (n.buffer?.duration ?? 0) + 0.5 > cutoff - previewStartCtxTime);
    }, Math.max(50, passMs - 100));
    playBtn.disabled = true;
    stopBtn.disabled = false;
    setStatus(`previewing @ ${bpm} BPM (16 × 16th-notes)`);
    runStepIndicator();
  };

  const stopPreview = (): void => {
    if (previewLoopTimer !== null) {
      clearInterval(previewLoopTimer);
      previewLoopTimer = null;
    }
    for (const n of scheduledNodes) {
      try {
        n.stop();
      } catch {
        // node may already have fired-and-finished
      }
    }
    scheduledNodes = [];
    playBtn.disabled = false;
    stopBtn.disabled = true;
    if (stepRaf !== null) cancelAnimationFrame(stepRaf);
    stepRaf = null;
    activeStep = -1;
    drawGrid();
    setStatus("stopped");
  };

  const runStepIndicator = (): void => {
    const tick = (): void => {
      if (previewLoopTimer === null || !audioCtx) {
        stepRaf = null;
        return;
      }
      const elapsed = audioCtx.currentTime - previewStartCtxTime;
      const stepDur = stepDurationSec();
      // elapsed can be negative for ~50ms while we're waiting on startAt
      const step = Math.max(0, Math.floor(elapsed / Math.max(0.0001, stepDur))) % stepCount;
      if (step !== activeStep) {
        activeStep = step;
        drawGrid();
      }
      stepRaf = requestAnimationFrame(tick);
    };
    stepRaf = requestAnimationFrame(tick);
  };

  // ---- bake -----------------------------------------------------------

  const bakeToTracks = async (): Promise<void> => {
    const projectSr = await client.projectSampleRate();
    if (projectSr === 0) {
      setStatus("can't bake — engine has no sample rate yet");
      return;
    }
    const stepFrames = Math.round(stepDurationSec() * projectSr);
    // Mint a fresh group id by scanning every existing clip in the project so
    // baked patterns can be moved around the arranger as one block. The
    // engine doesn't expose a "next group id" call, so we compute it here.
    let groupId = 1;
    {
      const tracks = await client.listTracks();
      let max = 0;
      for (const t of tracks) {
        const clips = await client.listClips(t.id);
        for (const c of clips) {
          if (c.group > max) max = c.group;
        }
      }
      groupId = max + 1;
    }
    let tracksAdded = 0;
    let clipsAdded = 0;
    for (const lane of lanes) {
      // Resolve both source records so we can stamp clips for whichever
      // step value they were assigned to. A lane with neither slot
      // assigned (or with hits only on an unassigned slot) bakes nothing.
      const srcA = lane.sourceA ? sources.find((s) => s.id === lane.sourceA) : null;
      const srcB = lane.sourceB ? sources.find((s) => s.id === lane.sourceB) : null;
      const hits: Array<{ stepIdx: number; sourceId: string; src: SourceInfo }> = [];
      for (let s = 0; s < lane.steps.length; s++) {
        const v = lane.steps[s] ?? 0;
        if (v === 1 && srcA) hits.push({ stepIdx: s, sourceId: srcA.id, src: srcA });
        else if (v === 2 && srcB) hits.push({ stepIdx: s, sourceId: srcB.id, src: srcB });
      }
      if (hits.length === 0) continue;
      // One track per non-empty lane so the user can mix each drum line
      // independently in the arranger; clips on the same lane reference
      // either source A or B based on which step value triggered them.
      const trackId = await client.addTrack(lane.label);
      tracksAdded++;
      for (const hit of hits) {
        const positionFrame = hit.stepIdx * stepFrames;
        try {
          const clipId = await client.addClip(
            trackId,
            hit.sourceId,
            positionFrame,
            0,
            hit.src.frames,
          );
          await client.setClipGroup(trackId, clipId, groupId);
          clipsAdded++;
        } catch (err) {
          console.warn("bake: addClip failed", err);
        }
      }
    }
    if (tracksAdded === 0) {
      setStatus("nothing to bake — assign a source and toggle some steps");
      return;
    }
    // Name the group after the pattern name field, and save the grid back
    // to the project's pattern library so the user can re-insert it from
    // the arranger or load it back into the editor later.
    const name = (nameInput.value.trim() || `Group ${groupId}`);
    await client.setGroupName(groupId, name);
    await client.savePattern(name, JSON.stringify(currentGrid()));
    setStatus(`baked "${name}" — ${clipsAdded} hits across ${tracksAdded} new track${tracksAdded === 1 ? "" : "s"}`);
    await refreshPatternList();
    opts.onArrangerNeedsRefresh?.();
  };

  /** Snapshot the current grid into the wire format used by setPattern. */
  const currentGrid = (): SavedPatternGrid => ({
    lanes: lanes.map((l) => ({
      label: l.label,
      sourceA: l.sourceA,
      sourceB: l.sourceB,
      steps: [...l.steps],
    })),
    stepCount,
    stepsPerBeat,
  });

  /** Rebuild the load picker from the engine's saved patterns. */
  const refreshPatternList = async (): Promise<void> => {
    let patterns: { name: string; gridJson: string }[] = [];
    try {
      patterns = await client.listPatterns();
    } catch {
      // pre-patterns engine — ignore
    }
    const prev = patternPicker.value;
    patternPicker.innerHTML = "";
    if (patterns.length === 0) {
      const opt = document.createElement("option");
      opt.value = "";
      opt.textContent = "(no saved patterns)";
      patternPicker.appendChild(opt);
      loadBtn.disabled = true;
      return;
    }
    patterns.sort((a, b) =>
      a.name.localeCompare(b.name, undefined, { sensitivity: "base" }),
    );
    for (const p of patterns) {
      const opt = document.createElement("option");
      opt.value = p.name;
      opt.textContent = p.name;
      patternPicker.appendChild(opt);
    }
    if (patterns.some((p) => p.name === prev)) patternPicker.value = prev;
    loadBtn.disabled = false;
  };

  /** Load a saved pattern by name back into the editor grid. */
  const loadSavedPattern = async (): Promise<void> => {
    const name = patternPicker.value;
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
    // Adopt the saved pattern's length so + extensions on the previous
    // pattern don't bleed into a freshly-loaded one. Falls back to 16 for
    // older patterns that didn't capture stepCount.
    stepCount = Math.max(STEPS_PER_BAR, grid.stepCount || DEFAULT_STEPS);
    // Resize lanes array to match the saved pattern; older patterns may
    // have fewer lanes than the new default kit (or more, for users who
    // expanded after the dual-source upgrade). Read both legacy and new
    // formats for source slots and step values.
    lanes.length = 0;
    for (const savedLane of grid.lanes) {
      const sourceA = savedLane.sourceA ?? savedLane.sourceId ?? null;
      const sourceB = savedLane.sourceB ?? null;
      const steps = new Array<StepValue>(stepCount).fill(0);
      const limit = Math.min(steps.length, savedLane.steps.length);
      for (let s = 0; s < limit; s++) {
        const v = savedLane.steps[s];
        // Legacy boolean steps map to "X" (1); numeric values clamp to 0..2.
        steps[s] =
          typeof v === "boolean"
            ? v
              ? 1
              : 0
            : ((Math.max(0, Math.min(2, Number(v) || 0)) as 0 | 1 | 2));
      }
      lanes.push({ label: savedLane.label, sourceA, sourceB, steps });
    }
    nameInput.value = name;
    updateLengthDisplay();
    drawGrid();
    setStatus(
      `loaded "${name}" (${stepCount / STEPS_PER_BAR} bars · ${lanes.length} lanes)`,
    );
  };

  /** Save the current grid under the name in the textbox, replacing any
   *  existing pattern with the same name. */
  const saveCurrentPattern = async (): Promise<void> => {
    const name = nameInput.value.trim();
    if (!name) {
      setStatus("name the pattern first");
      return;
    }
    try {
      await client.savePattern(name, JSON.stringify(currentGrid()));
      setStatus(`saved "${name}"`);
      await refreshPatternList();
    } catch (err) {
      setStatus(`save failed: ${String(err)}`);
    }
  };

  // ---- toolbar wiring -------------------------------------------------

  // Refresh the toolbar's bar-count chip from the current stepCount. Cheap
  // enough to call from anywhere that touches stepCount.
  const updateLengthDisplay = (): void => {
    const bars = stepCount / STEPS_PER_BAR;
    lengthDisplay.textContent = `${bars} bar${bars === 1 ? "" : "s"} · ${stepCount} steps`;
  };
  updateLengthDisplay();

  // Stop preview before extending — the scheduler captures stepCount at
  // schedule time, so live-mutating it under a running preview would leave
  // a half-pattern dangling on the audio clock.
  addBarsBtn.addEventListener("click", () => {
    if (previewLoopTimer !== null) stopPreview();
    const added = BARS_PER_EXTEND * STEPS_PER_BAR;
    stepCount += added;
    for (const lane of lanes) {
      lane.steps.push(...new Array<StepValue>(added).fill(0));
    }
    updateLengthDisplay();
    drawGrid();
    setStatus(`extended to ${stepCount / STEPS_PER_BAR} bars`);
  });

  addLaneBtn.addEventListener("click", () => {
    if (previewLoopTimer !== null) stopPreview();
    lanes.push({
      label: `L${lanes.length + 1}`,
      sourceA: null,
      sourceB: null,
      steps: new Array<StepValue>(stepCount).fill(0),
    });
    drawGrid();
    setStatus(`added lane (${lanes.length} total)`);
  });

  playBtn.addEventListener("click", () => {
    void startPreview();
  });
  stopBtn.addEventListener("click", () => {
    stopPreview();
  });
  clearBtn.addEventListener("click", () => {
    for (const lane of lanes) lane.steps.fill(0);
    drawGrid();
    setStatus("pattern cleared");
  });
  bakeBtn.addEventListener("click", () => {
    void bakeToTracks();
  });
  saveBtn.addEventListener("click", () => {
    void saveCurrentPattern();
  });
  loadBtn.addEventListener("click", () => {
    void loadSavedPattern();
  });

  // ---- refresh: pull sources + tempo from engine ----------------------

  const refresh = async (): Promise<void> => {
    try {
      sources = await client.listSources();
    } catch (err) {
      setStatus(`listSources failed: ${String(err)}`);
      sources = [];
    }
    // Sort by display name (case-insensitive, locale-aware) so the source
    // pickers read alphabetically. Tie-break on id for deterministic order
    // when two sources share a name.
    sources.sort(
      (a, b) =>
        a.name.localeCompare(b.name, undefined, { sensitivity: "base" }) ||
        a.id.localeCompare(b.id),
    );
    try {
      const t = await client.getTempo();
      bpm = t.bpm;
      beatsPerBar = t.beatsPerBar;
      beatUnit = t.beatUnit;
    } catch {
      // older engines without getTempo — keep defaults
    }
    void beatsPerBar; // currently informational only
    bpmDisplay.textContent = `${bpm.toFixed(1)} BPM · 16 × 1/16`;
    // Drop cached buffers for sources that no longer exist (e.g. user
    // deleted them in the editor between visits). Lane assignments to
    // those vanish too so the picker can't end up showing a ghost id.
    const liveIds = new Set(sources.map((s) => s.id));
    for (const id of [...bufferCache.keys()]) {
      if (!liveIds.has(id)) bufferCache.delete(id);
    }
    for (const lane of lanes) {
      if (lane.sourceA && !liveIds.has(lane.sourceA)) lane.sourceA = null;
      if (lane.sourceB && !liveIds.has(lane.sourceB)) lane.sourceB = null;
    }
    await refreshPatternList();
    drawGrid();
    setStatus(
      sources.length === 0
        ? "no sources in library — import some via Editor or Arranger"
        : `${sources.length} source${sources.length === 1 ? "" : "s"} available`,
    );
  };

  await refresh();
  void opts.onSourceImported; // referenced for symmetry with other tabs
  return { refresh };
}

function makeBtn(label: string): HTMLButtonElement {
  const b = document.createElement("button");
  b.type = "button";
  b.textContent = label;
  Object.assign(b.style, {
    background: "#1f1f1f",
    color: "#d8d8d8",
    border: "1px solid #333",
    padding: "4px 12px",
    fontSize: "12px",
    cursor: "pointer",
    borderRadius: "3px",
  } satisfies Partial<CSSStyleDeclaration>);
  return b;
}
