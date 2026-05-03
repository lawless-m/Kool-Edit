# Kool-Edit v1 Feature Specification

## Product vision

A browser-based audio editor in the spirit of Cool Edit Pro 2. Combines a destructive waveform editor with a multitrack sequencer, plus a spectral view that supports time-frequency selection and editing. Targets people who want a serious audio editor in the browser and find Audacity inadequate.

Not a faithful pixel-for-pixel recreation. Spiritual homage with modernised technology and typography. Cool Edit's information density and directness, not its specific bevels.

## Working name

Kool-Edit. Public name to be revisited if and when the project is released.

## Target browser

Chromium-based only. Vivaldi is the primary development browser. Firefox and Safari are explicitly out of scope. This buys File System Access API, OPFS with synchronous access handles, reliable AudioWorklet, WebGL2, and (eventually) WebGPU.

## Internal audio format

- Sample rate: 96 kHz project-internal, decoupled from AudioContext sample rate. Resampled at output.
- Sample format: float32 throughout the pipeline.
- Channel layouts: mono and stereo for v1. Multi-channel deferred.

## The two editing surfaces

### Destructive waveform editor

Single audio file at a time. Edits commit to the working buffer (via the edit list — see data model document). Operations:

- **Selection:** click-drag on waveform, snap to zero crossings (toggleable), keyboard refinement (arrow keys, shift+arrow to extend), select-all, select-to-end, select-to-start, type-in numeric range.
- **Clipboard:** cut, copy, paste, paste-mix (sum into selection rather than replace), paste-overlap (crossfade), trim-to-selection, delete.
- **Sample-region operations:** silence, gain (dB or linear), normalize (peak or RMS or LUFS), fade in, fade out, fade types (linear, logarithmic, exponential, S-curve), reverse.
- **Effects:** parametric EQ (4-band minimum, sweepable freq, gain, Q, plus low-shelf and high-shelf), compressor (threshold, ratio, attack, release, makeup gain, knee), hard limiter, reverb, delay, noise reduction (see below), time stretch (independent), pitch shift (independent), DC offset removal, hard limiter.
- **Generators:** silence (insert), tone (sine, square, saw, triangle, with frequency and amplitude), noise (white, pink, brown), DTMF (digit string), sweep.
- **Analysis:** spectrum analyzer (FFT, configurable size and window), level meters (peak, RMS, LUFS-I, LUFS-S, LUFS-M), phase scope, frequency analysis of selection.

### Multitrack sequencer

Timeline with horizontal tracks, clips arranged in time. Non-destructive — clips reference source files with in-point, out-point, and per-clip parameters.

- **Track operations:** add, delete, rename, mute, solo, arm-for-record, route output bus, height adjustment.
- **Clip operations:** drag-to-move (with snap), trim from edges, split at playhead, duplicate, delete, crossfade with adjacent clip, lock, group.
- **Per-clip parameters:** gain (dB), pan, mute, fade-in, fade-out, fade type, time-stretch ratio, pitch-shift cents (independent of stretch).
- **Per-clip envelopes:** volume, pan. Breakpoint editing. Envelope rides on top of clip gain.
- **Per-track envelopes:** volume, pan, plus any insert effect parameter. Automation lanes shown below the track.
- **Track effects:** insert chain of effects per track. Same effects as destructive editor are available as track inserts when their parameters can be automated.
- **Routing:** master bus in v1. Send buses and group buses deferred.
- **Snap:** off, beats (with configurable BPM and signature), seconds, frames (configurable rate), samples.
- **Markers:** named time-points on the timeline, navigable via keyboard.

### Bridging the two

Cool Edit model. Double-click a clip → opens its source file in the destructive editor. Save in destructive editor → updates source file → all clips referencing it reflect the change. Right-click clip → "Make Unique" duplicates source so edits to that clip's source no longer affect other clips.

## Spectral view

Available in the destructive editor as an alternative to the waveform view, toggleable. Multitrack does not have a spectral view in v1.

### Display

- STFT-based, log or linear frequency axis, configurable window size (256 to 8192) and overlap (50%, 75%, 87.5%).
- Configurable colour map (default: black to dark blue to magenta to yellow to white, magnitude-mapped).
- Adjustable dB range for colour mapping (e.g. -120 dB to 0 dB).
- Smooth zoom across cached resolution levels.
- Frequency cursor with numeric readout.

### Selection

- Time-only marquee (matches waveform selection).
- Time-frequency rectangular marquee.
- Time-frequency lasso (free-form polygon).
- Magic wand (flood-fill bins above a threshold within tolerance, like image editors).
- Selection feathering (soft edges in time and frequency).

### Edits within spectral selection

- Silence (set bins to zero magnitude).
- Attenuate by dB amount.
- Amplify by dB amount.
- Spectral repair (interpolate magnitude across selection from time-adjacent frames; v1 uses linear interpolation; quality improvements deferred).

Spectral edits bake on apply (Cool Edit-style, not RX-style re-editable). The edit appears in the edit list as a single operation.

## Noise reduction

FFT-based spectral subtraction with gating. Two-step workflow:

1. **Capture noise profile:** select a region containing only noise. Compute averaged magnitude spectrum. Save as a named profile in the project.
2. **Apply reduction:** select target region (or whole file). For each STFT frame, compute per-bin gain as a function of (target magnitude, profile magnitude, oversubtraction factor, gain floor). Apply gain. ISTFT.

Parameters exposed: amount (dB of reduction), floor (minimum gain in dB), oversubtraction factor, attack, release, frequency smoothing, FFT size.

Noise profiles persist in the project file and can be reused across edits.

## Recording

- Source: any input device exposed by `getUserMedia`.
- Format: capture into AudioWorklet, write float32 to OPFS-backed source file.
- Modes: record into a new file (destructive editor), record onto an armed track at the playhead (multitrack), punch-in/punch-out by selection.
- Pre-roll and post-roll configurable.
- Monitoring: input passthrough toggle, input level meter.
- Sample rate conversion if input device differs from project rate.

## Project file format

JSON canonical. See data model document for schema. Saved as `.kep` (Kool-Edit Project) extension. Companion DSL surface for export, scripting, and inspection — see DSL grammar document.

Project export to portable archive: `.kepz` is a zip containing `project.json` and a `sources/` directory with all referenced source audio. Allows sharing a project without external file dependencies.

## Visual identity

- Dark waveform background (near-black). Waveform trace in green or yellow, configurable. Clipped peaks in red.
- Light grey panel chrome with subtle depth cues. No hard 1-pixel bevels.
- Compact toolbars with pictographic icons. Tooltips on hover for accessibility.
- Information-dense status bar across the bottom: sample position, time, selection length, sample rate, format, free OPFS quota.
- Floating effect dialogs (modal during edit, with preview button, A/B compare, preset dropdown, factory presets, user presets).
- Numeric entry fields wherever a parameter is set. Drag-to-adjust on the field with shift for fine and ctrl for coarse.
- System font for UI text, properly antialiased. DPI-scaled. Dark mode is the default; light mode optional.
- Scroll-wheel zoom on waveform and spectrogram. Trackpad pinch zoom. Shift+wheel for horizontal scroll.

## What is explicitly out of scope for v1

- MIDI.
- Surround / multi-channel beyond stereo.
- Send and group buses (master only).
- VST or AU plugins. WAM host deferred (effects are designed to be WAM-compatible in shape).
- Cloud sync, collaboration, sharing.
- Mobile / touch-first UI.
- Video.
- Sidechain routing.
- Tempo and time-signature changes within a project.
- AI-based effects (de-noise, de-reverb, source separation).
- Advanced spectral repair beyond linear interpolation.

## Effects v1 list (consolidated)

Built-in, all implemented in Rust:

1. Gain (dB or linear)
2. Normalize (peak / RMS / LUFS-I)
3. Fade in / fade out (linear, log, exp, S-curve)
4. Parametric EQ (4 bands + low-shelf + high-shelf)
5. Compressor (threshold, ratio, attack, release, makeup, knee)
6. Hard limiter (ceiling, lookahead, release)
7. Reverb (algorithmic, room/hall/plate model with size, damping, mix)
8. Delay (time, feedback, mix, optional ping-pong, optional LP filter on feedback)
9. Noise reduction (spectral subtraction with profile)
10. Time stretch (phase vocoder, independent of pitch)
11. Pitch shift (phase vocoder, independent of time)
12. DC offset removal
13. Reverse
14. Spectral attenuate / amplify / silence (within spectral selection)
15. Spectral repair (linear magnitude interpolation)

## Acceptance for v1

A user can:

1. Record a vocal take from a microphone.
2. Open it in the destructive editor.
3. Trim silence, normalize, apply EQ and compression.
4. Capture a noise profile from a quiet section and reduce noise across the file.
5. Switch to spectral view, identify a click as a vertical streak, marquee-select it, and silence it.
6. Save the file.
7. Create a multitrack session, import the cleaned vocal as a clip, add a music bed clip on a second track.
8. Adjust per-clip volume envelopes, add fades, set pan.
9. Add a reverb insert on the vocal track with automation on the mix parameter.
10. Mix down to a stereo WAV file.
11. Save the project as `.kep`.
12. Reopen the project, all state restored.
13. Export the same project as a `.kepz` archive and reopen it on a different machine.
