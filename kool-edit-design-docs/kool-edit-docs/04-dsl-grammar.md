# Kool-Edit DSL Grammar

## Purpose

A textual representation of Kool-Edit projects and operations. Two modes share one vocabulary:

- **Declarative** — a project file. Describes state: sources, edit lists, tracks, clips, envelopes.
- **Imperative** — a script. Describes a sequence of operations to apply to a target.

JSON is the canonical project format. The DSL is a surface for export, import, scripting, inspection, and diffing. Round-trip JSON ↔ DSL is value-preserving but not formatting-preserving (comments and whitespace are lost on import).

## File extensions

- `.kep` — JSON project file
- `.kepz` — zip archive containing JSON project + referenced source files
- `.keds` — DSL project (declarative)
- `.keda` — DSL action script (imperative)

## Lexical structure

- UTF-8 throughout.
- Line comments: `# comment to end of line`.
- Block comments: `/* ... */`, do not nest.
- Whitespace is generally insignificant except inside strings.
- Strings: double-quoted, `\n \t \\ \"` escapes, `\u{NNNN}` for unicode.
- Numbers: decimal, integer or float. Underscores allowed for readability: `96_000`.
- Times can be written as samples (`@123456s`), seconds (`@1.5sec`), or HMS (`@00:01:23.456`).
- dB values written with suffix: `-3.0dB`, `+6dB`, `-inf`.
- Identifiers: `[A-Za-z_][A-Za-z0-9_]*`.

## Top-level: project file (declarative)

```
project "My Session" {
    format_version: 1
    sample_rate: 96000
    created: "2026-05-03T12:00:00Z"
    modified: "2026-05-03T14:23:11Z"

    sources {
        src_a4f2 "vocals_raw.wav" {
            channels: 1
            sample_rate: 48000
            base_file: "sources/src_a4f2/base.f32"
            base_length: 14_400_000

            history_pointer: 3

            ops {
                @00:00:12.450 - @00:00:12.890  silence
                @00:00:34.100 - @00:00:36.700  gain -3.0dB
                @00:01:02.000 - @00:01:04.500  fade_out shape:linear
                @00:02:15.330 - @00:02:15.380  spectral attenuate band:2400-3800 amount:-18dB stft:default
            }
        }

        src_b1c8 "music_bed.wav" {
            channels: 2
            sample_rate: 96000
            base_file: "sources/src_b1c8/base.f32"
            base_length: 21_600_000
            history_pointer: 0
        }
    }

    noise_profiles {
        np_001 "Air Conditioner" {
            captured_from: src_a4f2
            range: @00:00:00 - @00:00:02
            stft: default
        }
    }

    tracks {
        track "Lead Vocal" {
            height: 80
            gain: 0dB
            pan: 0
            inserts {
                eq {
                    band 1 { freq: 80,    type: highpass, q: 0.7 }
                    band 2 { freq: 250,   type: peak,     gain: -2dB, q: 1.4 }
                    band 3 { freq: 3000,  type: peak,     gain: +2dB, q: 1.0 }
                    band 4 { freq: 12000, type: highshelf, gain: +1dB }
                }
                compressor {
                    threshold: -18dB
                    ratio: 3
                    attack: 5ms
                    release: 80ms
                    makeup: 3dB
                    knee: 6dB
                }
            }
            automation {
                lane on:"insert.1.threshold" {
                    @00:00:00  -18dB linear
                    @00:01:30  -22dB linear
                    @00:03:00  -18dB linear
                }
            }
            clips {
                clip from src_a4f2 {
                    name: "Vocal Take 3"
                    at: @00:00:00
                    in: @00:00:00
                    out: @00:04:30
                    gain: 0dB
                    pan: 0
                    fade_in:  { duration: 50ms,  shape: linear }
                    fade_out: { duration: 200ms, shape: scurve }
                    envelope volume {
                        @00:00:00  0dB linear
                        @00:00:02  0dB linear
                        @00:04:28  0dB linear
                        @00:04:30  -inf linear
                    }
                }
            }
        }

        track "Music Bed" {
            height: 60
            gain: -6dB
            pan: 0
            clips {
                clip from src_b1c8 {
                    at: @00:00:00
                    in: @00:00:00
                    out: @00:04:30
                }
            }
        }
    }

    master {
        gain: 0dB
        inserts {
            limiter { ceiling: -0.3dB, lookahead: 5ms, release: 50ms }
        }
    }

    markers {
        marker "Verse 1"   @00:00:00
        marker "Chorus"    @00:00:48
        marker "Verse 2"   @00:01:36
    }

    transport {
        playhead: @00:00:00
        loop: false
    }

    view {
        zoom: 1.0
        scroll: @00:00:00
        active_view: waveform
    }
}
```

## Top-level: action script (imperative)

```
script {
    target source "src_a4f2"

    # remove a breath
    select @00:00:12.450 - @00:00:12.890
    silence

    # tame a loud word
    select @00:00:34.100 - @00:00:36.700
    gain -3.0dB

    # remove a click using spectral edit
    select_spectral time:@00:02:15.330-@00:02:15.380 freq:2400-3800
    attenuate -18dB

    # apply noise reduction
    select all
    noise_reduce profile:np_001 amount:12dB floor:-40dB
}
```

A script can also target the project for multitrack operations:

```
script {
    target project

    add_track "Vocal Doubler"
    on_track "Vocal Doubler" {
        add_clip from:src_a4f2 at:@00:00:00 in:@00:00:00 out:@00:04:30
        set_gain -6dB
        set_pan 0.3
    }
}
```

## Operation reference

Every operation usable in destructive `ops { ... }` blocks is also usable in scripts. Range is implicit in `ops` blocks (the `@from - @to` prefix); explicit in scripts (via `select`).

### Sample-region operations

```
silence
gain <db>
fade_in shape:<linear|log|exp|scurve>
fade_out shape:<linear|log|exp|scurve>
normalize target:<peak|rms|lufs> value:<num>
reverse
dc_remove
```

### Clipboard operations

```
cut
copy                                # source-only, doesn't modify
paste at:<time> from:<clipboard_ref>
paste_mix at:<time> from:<clipboard_ref>
paste_over at:<time> from:<clipboard_ref> crossfade:<duration>
```

### Effect operations

```
eq <eq_params>
compress <comp_params>
limit <limit_params>
reverb <reverb_params>
delay <delay_params>
time_stretch ratio:<num>
pitch_shift cents:<num>
noise_reduce profile:<id> amount:<db> floor:<db> [oversub:<num>] [smoothing:<num>]
```

Effect parameter blocks use the same syntax as track inserts:

```
eq {
    band 1 { freq: 80, type: highpass, q: 0.7 }
    ...
}
```

### Spectral operations

Spectral operations have a 2D selection (time × frequency) rather than a 1D sample range:

```
select_spectral time:<range> freq:<low>-<high>           # rectangular
select_spectral_lasso <list of (time, freq) points>      # polygon
select_spectral_wand at:(<time>,<freq>) threshold:<db> tolerance:<db>

attenuate <db>
amplify <db>
silence_spectral
repair                                                    # linear interp
```

### Generators

```
generate at:<time> length:<duration> kind:<silence|tone|noise|dtmf|sweep> <params>
```

Examples:

```
generate at:@00:00:00 length:2sec kind:tone freq:440 amplitude:-12dB shape:sine
generate at:@end length:5sec kind:noise color:pink amplitude:-18dB
generate at:@cursor length:200ms kind:dtmf digits:"555-1234"
```

### Project operations (script-only)

```
add_track <name>
remove_track <name|index>
on_track <name|index> { <track operations> }

add_clip from:<source_id> at:<time> in:<time> out:<time>
remove_clip <id>
move_clip <id> to:<time>
trim_clip <id> in:<time> out:<time>
split_clip <id> at:<time>

add_marker <name> <time>
remove_marker <name>

set_gain <db>                # within on_track or on_clip block
set_pan <num>
set_mute <bool>
set_solo <bool>

add_insert <effect_kind> { <params> }
remove_insert <index>

add_envelope <param> { <breakpoints> }
add_automation on:"<param_path>" { <breakpoints> }
```

## Time literals

| Form              | Meaning                                          |
|-------------------|--------------------------------------------------|
| `@123456s`        | sample 123456 (project rate)                     |
| `@1.5sec`         | 1.5 seconds                                      |
| `@250ms`          | 250 milliseconds                                 |
| `@00:01:23.456`   | 1 minute 23.456 seconds                          |
| `@cursor`         | current playhead position (script context only)  |
| `@start`          | beginning of target                              |
| `@end`            | end of target                                    |
| `@selection.in`   | start of current selection                       |
| `@selection.out`  | end of current selection                         |

Durations use the same forms but without the `@`: `2sec`, `250ms`, `00:00:01.000`.

## dB literals

```
0dB         # unity
-3dB        # attenuation
+6dB        # boost (the + is optional)
-inf        # silence
-inf dB     # also silence (equivalent)
```

## Curve literals

For envelope and automation breakpoints:

```
linear      # straight line
exp         # exponential
log         # logarithmic
hold        # constant until next breakpoint
scurve      # ease-in-ease-out
```

## Identifier conventions

- `src_xxxx` — sources, where xxxx is the first 4 hex digits of the content hash
- `np_NNN` — noise profiles, sequentially numbered
- `t_NNN`, `c_NNN`, `e_NNN` — tracks, clips, effects, internal IDs (not usually written by hand)

## Versioning

The DSL has a version pinned to the JSON `format_version`. Every project file starts with `format_version: N`. The parser refuses to load a file with a version it doesn't understand. Migration from older versions is performed at JSON load time, not at DSL parse time — DSL is parsed to JSON, JSON is migrated, JSON becomes the in-memory model.

## Parser implementation notes

- Hand-written recursive descent in Rust. PEG and parser-combinator approaches are both viable; recursive descent gives the clearest error messages and the language is small enough.
- Errors include line, column, and a span of the offending text.
- The parser produces a JSON-equivalent AST. The actual JSON serialisation is then derived from the AST. This avoids two paths through the data.
- The emitter (in-memory → DSL) is a separate pass. It pretty-prints with consistent formatting; it is not symmetric with the parser (does not preserve user formatting).

## Future syntactic extensions (not v1)

- Macros / functions: `define remove_breath(at) { select at-50ms - at+50ms; silence }` then `remove_breath(@00:00:12.450)`
- Conditional blocks for batch scripts
- Loops over selections or markers
- Variables for parameter values reused across operations
- Importing other DSL files

These are kept in mind during v1 grammar design so they slot in without breaking existing files.
