# Drum Machine — Feature Plan

Drawn from *Captain Pikant — Drum Machine 101*, mapped against `ui/src/drums.ts`
as it stands today (boolean 16-step grid, one sample per lane, Web-Audio preview,
bake-to-arranger).

## Where we are now

- 6 lanes, fixed 16 steps, fixed 4 steps-per-beat (one bar of 1/16ths in 4/4).
- Each step is a plain `boolean` — no velocity, no microtiming, no automation.
- Lanes hold one `sourceId` apiece; the live-preview path schedules one
  `BufferSource` per hit, the bake path drops one clip per hit on a new track.
- Tempo is read from the engine; pattern is one bar, no chaining, no song mode.

The book describes a great many features. Below is a triaged shortlist of the
ones that buy the most musical mileage for the smallest amount of code, in
roughly the order I'd tackle them.

---

## Tier 1 — cheap wins, big musical payoff

### 1. Per-step velocity (accents)
*Book chapter 3 — Accents.*

Replace `steps: boolean[]` with `steps: number[]` where 0 = off, 1 = quiet,
2 = normal, 3 = accent. Three clicks cycles off → soft → loud → accent → off
(909-style "press repeatedly" UX). Render with three brightness levels.

- Preview: set a `GainNode` per hit before `start()`. Map levels to e.g.
  `[0, 0.55, 0.85, 1.0]`.
- Bake: pass a gain through to `addClip` if/when the engine supports per-clip
  gain; otherwise stamp the velocity into the clip name for now and revisit
  once the engine has a hook.
- Optional follow-up: a global accent slider in the toolbar that scales the
  difference between normal and accent steps (Tier 2).

### 2. Adjustable step count (pattern length)
*Book chapter 6 — Pattern Length.*

Keep the visual at 16 cells per row but allow 1–64 steps via a "Last step"
button or a number input next to BPM. Page through with `< 1-16 | 17-32 >`
buttons when length > 16. Most beats live in 16 or 32 steps; this opens up
fills, breaks, and 5/4 / 3/4 patterns without a rewrite.

- Data: `lane.steps.length` becomes the source of truth; helper to grow/shrink
  preserving existing hits.
- Step indicator and `scheduleOnePass` already iterate `DEFAULT_STEPS`; swap
  for `pattern.steps`.

### 3. Swing (Linn-style 16th swing, per pattern)
*Book chapters 5–7.*

Single knob `0–100%` on the toolbar, neutral at 50%, presets at 54/58/63/67/71/75
to mirror the 909. On scheduling, even-indexed steps (1, 3, 5, … 0-indexed)
get nudged forward in time by `((swing - 50) / 50) * (stepDur / 2)`.

- Trivial in `scheduleOnePass`: compute `t` per step from a swung-time helper
  rather than `s * stepDur`.
- Bake path needs the same nudge applied to `positionFrame`.

### 4. Per-step microtiming (nudge ±)
*Book chapter 12.*

Once velocity is in, add a separate "nudge" mode where right-click (or a mode
toggle) cycles ±1/64 ±1/32 ±3/64 nudges on a step. Stored as
`steps[i].nudgeFrac ∈ [-0.5, +0.5]` of one step.

This is what unlocks flams (book chapter 12 — Flams), humanised hi-hats, and
the "selective swing" workaround. Same scheduling math as swing.

---

## Tier 2 — meaningful next layer

### 5. Substeps / ratchets / rolls
*Book chapter 10.*

Per step, a small numeric "1, 2, 3, 4, 6, 8" picker (default 1). At schedule
time, fire `n` evenly-spaced hits inside the step's duration. Optional velocity
ramp (chapter 10 explicitly notes how few sequencers offer this — easy
differentiator).

- Stick with the **subdivision** model from the book, not the
  "note-value + length" model. Simpler UI, covers ~100% of common needs.

### 6. Choke groups (HH/OH mutual exclusion)
*Book chapter 31, 32 — 909/808 guidelines.*

Add a per-lane `chokeGroup: number | null`. When a hit fires on a lane in a
group, stop any scheduled future `BufferSource` from same group whose
`startTime > now - epsilon`. Defaults: HH and OH share group 1, no others.

Bake-side: emit clips with shorter durations when a same-group hit follows.

### 7. Per-track swing
*Book chapter 7 — Swing Per Track.*

Once global swing exists, give each lane an optional override slider (small
"S" knob in the lane header). Lets the user swing only the shaker/hat for
that classic OutKast/808 State feel. Cheap once Tier 1 #3 is in place.

---

## Tier 3 — bigger lifts, save for later

### 8. Probability per step
*Book chapter 33 — "Sequencer tools that introduce variation".*

Right-click a step (or shift-click in a "prob" mode) to set a 25/50/75/100%
play chance. Roll at schedule time. Cheapest variation-introducing feature
in the book; great for hi-hats.

### 9. Iteration dependence (every-Nth)
Optional follow-up to probability — "play only on loop 4 of 4". One small
dropdown per step, evaluated against a pattern-loop counter.

### 10. Note length / gating
*Book chapter 30.*

Per step, an optional "length" override that ends the sample early via the
`BufferSource`'s built-in `stop(t)`. Most useful for long open hats and
crashes. UI: a faint horizontal extent on the step cell when length < ∞.

### 11. Tempo multiplier / pattern scale
*Book chapter 11.*

Per-pattern `scale ∈ {1/2, 3/4, 1, 3/2, 2}`. Scales `stepDurationSec`. Mostly
redundant with substeps + variable step count for our use case, so this is
last on the list — only worth doing if a user actually asks for triplet bars.

### 12. Automation lanes (parameter locks)
*Book chapter 13.*

Beyond what's reasonable for a first pass — would need a per-lane parameter
model the engine doesn't currently expose. Park it.

---

## Tier 4 — explicitly NOT doing

- 808/909 MIDI map (chapter 31 — we're a web DAW, not a MIDI device)
- External sequencing / clock sync (chapter 33 — same reason)
- Song mode (the arranger is the song)
- Pattern chaining / multiple pattern slots — bake-to-tracks already lets the
  user lay multiple variations onto the arranger timeline, which is the better
  authoring surface for arrangement. The drum tab stays as a single-pattern
  scratchpad.
- "Variation chaining" with A/B/C/D/E/F/G/H per pattern — same reason
- Container/lock-trig steps (chapter 13) — only matters if we add automation

---

## Suggested data-model evolution

Today:

```ts
interface Lane { label: string; sourceId: string|null; steps: boolean[]; }
```

After Tier 1:

```ts
interface Step { vel: 0|1|2|3; nudgeFrac: number; }       // -0.5..+0.5
interface Lane { label: string; sourceId: string|null; steps: Step[]; }
interface Pattern { steps: number; lanes: Lane[]; swing: number; }
```

After Tier 2:

```ts
interface Step { vel: 0|1|2|3; nudgeFrac: number; subdiv: number; prob?: number; }
interface Lane { label: string; sourceId: string|null; steps: Step[];
                 chokeGroup?: number; swing?: number; }
interface Pattern { steps: number; lanes: Lane[]; swing: number; }
```

The migration each tier is small: `boolean[]` → `Step[]` is a one-time map,
and every later tier just adds optional fields. Still one pattern at a time —
arrangement happens in the arranger via bake-to-tracks.

---

## Order of attack I'd recommend

1. Velocity (Tier 1.1) — proves out the `Step` object refactor.
2. Pattern length (Tier 1.2) — unblocks any beat longer than 16.
3. Swing (Tier 1.3) — most asked-for "feel" feature.
4. Microtiming nudge (Tier 1.4) — small once swing is done; opens up flams.
5. Substeps (Tier 2.5) — trap hats and rolls.
6. Choke groups (Tier 2.6) — needed for any believable HH pattern.

Stopping at step 4 already gets us past most "toy drum machine" complaints.
Stopping at step 6 covers the genuinely musical features. Anything more
ambitious (multi-pattern, song mode) is the arranger's job, not the drum
tab's.
