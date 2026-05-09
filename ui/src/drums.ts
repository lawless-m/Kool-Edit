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

interface Lane {
  label: string;
  sourceId: string | null;
  steps: boolean[];
}

/** Wire format for a saved drum pattern. Stored as opaque JSON in the
 *  engine's `Project::patterns`. The arranger uses this to re-stamp a
 *  pattern at a new playhead position. `stepsPerBeat` is captured so the
 *  pattern survives changes to step resolution; the actual duration of one
 *  step is recomputed from the current project tempo on insert. */
export interface SavedPatternGrid {
  lanes: Array<{
    label: string;
    sourceId: string | null;
    steps: boolean[];
  }>;
  stepCount: number;
  stepsPerBeat: number;
}

const DEFAULT_LANE_LABELS = ["BD", "SD", "HH", "OH", "CY", "CP"];
const DEFAULT_STEPS = 16;
const STEPS_PER_GROUP = 4;

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

  const lanes: Lane[] = DEFAULT_LANE_LABELS.map((label) => ({
    label,
    sourceId: null,
    steps: new Array<boolean>(DEFAULT_STEPS).fill(false),
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

    for (let li = 0; li < lanes.length; li++) {
      const lane = lanes[li]!;
      const row = document.createElement("div");
      Object.assign(row.style, {
        display: "flex",
        alignItems: "center",
        gap: "8px",
        marginBottom: "6px",
      } satisfies Partial<CSSStyleDeclaration>);

      // Editable label so the user can rename "HH" to "tom" etc. without a
      // separate dialog. Blur commits.
      const label = document.createElement("input");
      label.value = lane.label;
      Object.assign(label.style, {
        width: "44px",
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

      // Source picker for this lane. "—" means unassigned: cells still
      // toggle but preview/bake skip the row.
      const picker = document.createElement("select");
      Object.assign(picker.style, {
        width: "180px",
        background: "#1a1a1a",
        color: "#d8d8d8",
        border: "1px solid #333",
        padding: "2px 4px",
        fontSize: "12px",
      } satisfies Partial<CSSStyleDeclaration>);
      const noneOpt = document.createElement("option");
      noneOpt.value = "";
      noneOpt.textContent = "— assign source —";
      picker.appendChild(noneOpt);
      for (const s of sources) {
        const o = document.createElement("option");
        o.value = s.id;
        o.textContent = s.name;
        picker.appendChild(o);
      }
      picker.value = lane.sourceId ?? "";
      picker.addEventListener("change", () => {
        lane.sourceId = picker.value || null;
        // Drop the cached buffer so the new source gets reloaded next time
        // it's previewed.
        if (lane.sourceId) void prefetchBuffer(lane.sourceId);
      });
      row.appendChild(picker);

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
          cells.appendChild(gutter);
        }
        const cell = document.createElement("button");
        cell.type = "button";
        cell.dataset["lane"] = String(li);
        cell.dataset["step"] = String(s);
        const on = lane.steps[s];
        const isActiveStep = activeStep === s;
        Object.assign(cell.style, {
          width: "26px",
          height: "32px",
          margin: "0 2px",
          borderRadius: "3px",
          border: isActiveStep ? "1px solid #ffe066" : "1px solid #555",
          background: on ? "#e6e6e6" : "#0e0e0e",
          cursor: "pointer",
          padding: "0",
          flexShrink: "0",
        } satisfies Partial<CSSStyleDeclaration>);
        cell.addEventListener("click", () => {
          lane.steps[s] = !lane.steps[s];
          drawGrid();
          // Audition the cell on toggle-on so the user can hear what's
          // assigned without committing to a full pattern playback.
          if (lane.steps[s] && lane.sourceId) {
            void triggerOneShot(lane.sourceId);
          }
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

  const patternDurationSec = (): number => stepDurationSec() * DEFAULT_STEPS;

  /** Schedule one full pattern pass starting at `startCtxTime`. Returns
   *  the array of sources scheduled so callers can cancel mid-flight. */
  const scheduleOnePass = async (startCtxTime: number): Promise<AudioBufferSourceNode[]> => {
    const ctx = ensureCtx();
    const stepDur = stepDurationSec();
    const out: AudioBufferSourceNode[] = [];
    // Resolve every assigned source up-front so scheduling is synchronous
    // (otherwise async waits would push hits late).
    const lanesWithBufs: Array<{ lane: Lane; buf: AudioBuffer }> = [];
    for (const lane of lanes) {
      if (!lane.sourceId) continue;
      const buf = await prefetchBuffer(lane.sourceId);
      if (buf) lanesWithBufs.push({ lane, buf });
    }
    for (let s = 0; s < DEFAULT_STEPS; s++) {
      const t = startCtxTime + s * stepDur;
      for (const { lane, buf } of lanesWithBufs) {
        if (!lane.steps[s]) continue;
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
      const step = Math.max(0, Math.floor(elapsed / Math.max(0.0001, stepDur))) % DEFAULT_STEPS;
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
      if (!lane.sourceId) continue;
      const hits = lane.steps
        .map((on, idx) => (on ? idx : -1))
        .filter((idx) => idx >= 0);
      if (hits.length === 0) continue;
      const src = sources.find((s) => s.id === lane.sourceId);
      if (!src) continue;
      // One track per non-empty lane so the user can mix each drum line
      // independently in the arranger.
      const trackId = await client.addTrack(lane.label);
      tracksAdded++;
      for (const stepIdx of hits) {
        const positionFrame = stepIdx * stepFrames;
        try {
          const clipId = await client.addClip(trackId, lane.sourceId, positionFrame, 0, src.frames);
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
      sourceId: l.sourceId,
      steps: [...l.steps],
    })),
    stepCount: DEFAULT_STEPS,
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
    // Replace as much of the editor state as the loaded pattern provides.
    // Lane count is fixed (we don't grow/shrink) so we copy by index.
    for (let i = 0; i < lanes.length; i++) {
      const src = grid.lanes[i];
      if (!src) {
        lanes[i]!.steps.fill(false);
        lanes[i]!.sourceId = null;
        continue;
      }
      lanes[i]!.label = src.label;
      lanes[i]!.sourceId = src.sourceId;
      const steps = lanes[i]!.steps;
      for (let s = 0; s < steps.length; s++) steps[s] = !!src.steps[s];
    }
    nameInput.value = name;
    drawGrid();
    setStatus(`loaded "${name}"`);
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

  playBtn.addEventListener("click", () => {
    void startPreview();
  });
  stopBtn.addEventListener("click", () => {
    stopPreview();
  });
  clearBtn.addEventListener("click", () => {
    for (const lane of lanes) lane.steps.fill(false);
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
      if (lane.sourceId && !liveIds.has(lane.sourceId)) lane.sourceId = null;
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
