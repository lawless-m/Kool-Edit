# Kool-Edit Data Model and Edit List Semantics

## The two layers

Kool-Edit has two distinct data layers and they behave differently.

1. **Sources** — immutable-ish audio data on disk (in OPFS). Editable via the destructive editor through the **edit list** mechanism described below.
2. **Multitrack project** — declarative composition of clips on tracks, where clips reference sources and add non-destructive parameters on top.

The destructive editor edits sources. The multitrack sequencer edits the project's clip and track structure. They share the project file but the operations are different and the undo histories are separate.

## Sources

A source is a logical audio file. Each source has:

```
Source {
    id: SourceId              // content-derived, stable
    name: String              // user-visible
    channel_count: u16
    sample_rate: u32          // source's native rate (may differ from project rate)
    base_file: OpfsPath       // current "flattened" sample data
    base_length: u64          // sample frames in base
    edit_list: Vec<Op>        // operations applied since last flatten
    history_pointer: usize    // current position in edit_list (for undo/redo)
    peak_cache: PeakCacheRef  // for waveform rendering
    created_at: Timestamp
    modified_at: Timestamp
}
```

Sample rate conversion: if a source's native rate differs from the project rate, the engine resamples on read. The source itself stores samples at native rate so re-export at native rate is lossless.

### The edit list

An edit list is an ordered sequence of operations. The "current state" of the source is the result of applying operations `[0..history_pointer]` to the base file.

Operations are pure data. They describe an edit; they don't perform it. Applying an operation produces samples; the operation itself is just the description.

```
enum Op {
    Silence    { range: SampleRange }
    Gain       { range: SampleRange, db: f32 }
    Fade       { range: SampleRange, kind: FadeKind, direction: FadeDir }
    Normalize  { range: SampleRange, target: NormTarget, value: f32 }
    Reverse    { range: SampleRange }
    DcRemove   { range: SampleRange }

    Cut        { range: SampleRange }                  // removes range, samples after shift left
    Insert     { at: u64, samples_ref: ClipboardRef }  // inserts samples at position
    PasteMix   { at: u64, samples_ref: ClipboardRef }  // sums samples into existing
    PasteOver  { at: u64, samples_ref: ClipboardRef, crossfade: u64 }

    Eq         { range: SampleRange, params: EqParams }
    Compress   { range: SampleRange, params: CompParams }
    Limit      { range: SampleRange, params: LimitParams }
    Reverb     { range: SampleRange, params: ReverbParams }
    Delay      { range: SampleRange, params: DelayParams }

    TimeStretch { range: SampleRange, ratio: f32 }
    PitchShift  { range: SampleRange, cents: f32 }

    NoiseReduce { range: SampleRange, profile_id: ProfileId, params: NrParams }

    SpectralEdit {
        range: TimeFreqRegion,    // selection in STFT bins, not samples
        operation: SpectralOp,    // attenuate(db) | amplify(db) | silence | repair
        stft_params: StftParams,  // window size, hop, used for invertibility
    }

    Generate { at: u64, length: u64, kind: GeneratorKind, params: GeneratorParams }
}
```

`ClipboardRef` is a reference to a chunk of audio in OPFS, written when a copy/cut operation occurs. The clipboard is a content-addressed store; refs survive across sessions.

### Sample queries

When the UI needs samples (for rendering, for playback, for export):

```
fn query_samples(source: &Source, range: SampleRange) -> Vec<f32>
```

The engine starts from the base file, replays operations up to `history_pointer` that intersect `range`, and returns the result. For long edit lists this is potentially slow, which is why flattening exists.

Samples queried for playback are resampled to project rate if needed. Samples queried for rendering use peak data when zoom level permits.

### Flattening

Flatten renders the current state to a new base file and clears the edit list:

```
fn flatten(source: &mut Source) {
    let samples = query_samples(source, 0..source.length());
    let new_path = opfs.write_new_source_file(samples);
    source.base_file = new_path;
    source.base_length = samples.len();
    source.edit_list.clear();
    source.history_pointer = 0;
    source.peak_cache = regenerate_peaks(source);
}
```

Triggered automatically when:

- Edit list exceeds 100 operations.
- Edit list disk size exceeds 10% of base file size.
- User explicitly requests "consolidate history".
- Project save (optional, configurable).

Triggered explicitly when:

- User chooses "Consolidate History" from the menu.

After flatten, undo of operations prior to the flatten is no longer possible. The UI warns before automatic flatten if the user has an undo history that would be lost; auto-flatten only happens if the history pointer is at the end of the edit list.

### Undo and redo

Undo decrements `history_pointer`. Redo increments it. New operations applied while `history_pointer < edit_list.len()` truncate the redo branch.

Undo and redo are O(1) on the data structure but potentially expensive on the next sample query (because more replay is needed). Mitigated by caching recent query results.

### Peak cache

For each source, the engine maintains peak data at multiple resolutions:

- 1:1 (raw samples) — implicit, served from the source itself
- 1:64 (one min/max pair per 64 samples)
- 1:4096 (one min/max pair per 4096 samples)

Peak cache is invalidated for the affected sample range on every operation. Background regeneration. Waveform rendering picks the appropriate level for the current zoom and interpolates between levels for smooth zoom.

## Multitrack project

The project is the top-level data structure:

```
Project {
    format_version: u32
    metadata: ProjectMetadata
    sample_rate: u32              // project-internal, default 96000
    sources: Map<SourceId, Source>
    tracks: Vec<Track>
    master: MasterBus
    markers: Vec<Marker>
    transport: TransportState
    view: ViewState
    noise_profiles: Map<ProfileId, NoiseProfile>
}

Track {
    id: TrackId
    name: String
    height: f32                   // pixel height in UI
    mute: bool
    solo: bool
    arm: bool
    gain_db: f32
    pan: f32                      // -1.0 to 1.0
    inserts: Vec<EffectInstance>
    automation: Vec<AutomationLane>
    clips: Vec<Clip>
}

Clip {
    id: ClipId
    source_id: SourceId
    name: String
    track_position: SampleRange   // where on the timeline (project-rate samples)
    source_in: u64                // start position in source
    source_out: u64               // end position in source
    gain_db: f32
    pan: f32
    fade_in: Fade
    fade_out: Fade
    time_stretch: f32             // 1.0 = no stretch
    pitch_shift_cents: f32        // 0.0 = no shift
    envelopes: Vec<ClipEnvelope>  // volume, pan
    locked: bool
    group: Option<GroupId>
}

ClipEnvelope {
    parameter: EnvelopeParam      // Volume | Pan
    breakpoints: Vec<Breakpoint>  // sorted by time
}

Breakpoint {
    time: u64                     // samples from clip start
    value: f32
    curve: CurveKind              // Linear | Exponential | Hold | SCurve
}

AutomationLane {
    parameter_path: ParamPath     // identifies which parameter (track gain, insert N param M, etc.)
    breakpoints: Vec<Breakpoint>  // sorted by time
}

EffectInstance {
    id: EffectInstanceId
    effect_kind: EffectKind
    params: Map<ParamId, f32>     // current parameter values
    bypass: bool
}
```

### Clip semantics at playback

To produce playback samples for a clip at a given project time:

1. Compute the source range needed, accounting for `time_stretch`. If stretching, use the engine's time-stretch processor.
2. Read source samples (which itself replays the source's edit list).
3. Apply pitch shift if any.
4. Apply clip gain and pan.
5. Apply clip envelopes (volume, pan) by reading their values at the current time.
6. Apply fade-in/fade-out shapes.
7. Sum into the track's input buffer.

After all clips on a track are summed:

8. Apply track inserts in order, with parameter automation if present.
9. Apply track gain and pan, with their automation if present.
10. Sum into master.

After all tracks summed:

11. Apply master inserts.
12. Apply master gain.
13. Output.

Envelope and automation values are computed from breakpoints by interpolating according to the curve kind between adjacent points. Hold curves stay flat until the next breakpoint. Linear interpolates linearly. Exponential and S-curve use shaped interpolation.

### Make Unique

A clip references a source by ID, so multiple clips can share a source. Editing the source affects all clips that reference it. "Make Unique" duplicates the source:

```
fn make_unique(project: &mut Project, clip_id: ClipId) {
    let clip = project.find_clip(clip_id);
    let original_source = project.sources.get(clip.source_id);
    let new_source = original_source.clone_with_new_id();
    project.sources.insert(new_source.id, new_source);
    clip.source_id = new_source.id;
}
```

The new source has an independent edit list. Subsequent edits to either source no longer affect the other.

### Multitrack undo

The multitrack project has its own undo history, separate from per-source edit lists. Operations on the project structure (adding clips, moving clips, changing track parameters, editing envelopes) form a project-level edit list with the same semantics: append-only, history pointer, undo/redo.

Project-level operations are always small (no sample data), so flattening doesn't apply — the full history is preserved indefinitely.

When a destructive operation on a source is undone, only that source's edit list moves. The project structure is unaffected. When a project operation is undone, only the project structure changes; sources are unaffected.

Cross-cutting concern: deleting a source. Cannot be undone if any clip references the source. UI prevents the deletion in that case.

## Constraints and invariants

1. Source IDs are stable for the life of a project. A source is never renumbered.
2. Clip `source_in <= source_out` and both are within the source's current length.
3. Clip `track_position.end - track_position.start = (source_out - source_in) * time_stretch`. The engine enforces this when any of those values changes.
4. Envelope and automation breakpoints are sorted by time and have unique times.
5. Fade durations are non-negative and do not exceed the clip duration.
6. Effect instance IDs are unique within a track.
7. The project sample rate does not change after creation. (Source rates may differ; they're resampled.)
8. Peak caches are always consistent with the current edit-list state (or are being regenerated and the UI shows a "rebuilding" indicator).

## Persistence

The in-memory model serialises to JSON. See DSL grammar document for the textual surface; this section covers the JSON shape.

```json
{
  "format_version": 1,
  "metadata": { ... },
  "sample_rate": 96000,
  "sources": {
    "src_a4f2": {
      "name": "vocals_raw.wav",
      "channel_count": 1,
      "sample_rate": 48000,
      "base_file": "sources/src_a4f2/base.f32",
      "base_length": 14400000,
      "edit_list": [ ... ],
      "history_pointer": 12,
      "peak_cache": "sources/src_a4f2/peaks.bin",
      "created_at": "2026-05-03T12:00:00Z",
      "modified_at": "2026-05-03T14:23:11Z"
    }
  },
  "tracks": [ ... ],
  "master": { ... },
  "markers": [ ... ],
  "transport": { ... },
  "view": { ... },
  "noise_profiles": { ... }
}
```

Sample data (base files, peak caches, clipboard chunks) is in OPFS, referenced by path. The JSON is small and inspectable; the bulk data is binary.

For `.kepz` portable export, the JSON and all referenced OPFS files are bundled into a zip with relative paths.
