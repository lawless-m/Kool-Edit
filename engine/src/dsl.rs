//! DSL surface (textual project format).
//!
//! This module emits projects as `.keds` text per `04-dsl-grammar.md`. The
//! parser lives elsewhere (and isn't built yet); a fresh Project that
//! round-trips through emit-then-parse is the eventual goal.
//!
//! Scope of the emitter today: the structural project header, sources with
//! the destructive ops the engine currently understands (Silence, Gain,
//! Fade, Normalize, Reverse, DcRemove, Cut, Generate, TimeStretch,
//! PitchShift), tracks/clips/markers with simple per-clip parameters, and
//! master gain. Variants the doc-04 grammar covers but that this slice
//! doesn't cover yet (effect parameter blocks, spectral edits, clipboard
//! ops, noise reduction, track inserts/automation, clip envelopes,
//! envelopes, noise profiles) return [`EmitError::Unsupported`] so callers
//! see exactly what's missing.

use crate::effect::{CompParams, DelayParams, LimitParams};
use crate::op::{
    FadeDirection, FadeShape, GeneratorParams, NoiseColor, NormTarget, Op, ToneShape,
};
use crate::project::{Clip, MasterBus, Project, Track};
use crate::range::SampleRange;
use crate::source::Source;

#[derive(Debug)]
pub enum EmitError {
    Unsupported(&'static str),
}

impl std::fmt::Display for EmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(k) => write!(f, "DSL emit: unsupported feature `{k}`"),
        }
    }
}

impl std::error::Error for EmitError {}

/// Render a project as DSL text. The output is whitespace-formatted; it does
/// not preserve any user formatting from a previous parse.
pub fn project_to_dsl(project: &Project) -> Result<String, EmitError> {
    let mut e = Emitter::new(project);
    e.emit_project()?;
    Ok(e.out)
}

struct Emitter<'a> {
    project: &'a Project,
    out: String,
    indent: usize,
}

impl<'a> Emitter<'a> {
    fn new(project: &'a Project) -> Self {
        Self {
            project,
            out: String::new(),
            indent: 0,
        }
    }

    fn pad(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn line(&mut self, s: &str) {
        self.pad();
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn open(&mut self, header: &str) {
        self.pad();
        self.out.push_str(header);
        self.out.push_str(" {\n");
        self.indent += 1;
    }

    fn close(&mut self) {
        self.indent -= 1;
        self.pad();
        self.out.push_str("}\n");
    }

    fn emit_project(&mut self) -> Result<(), EmitError> {
        let name = quote(&self.project.metadata.name);
        self.open(&format!("project {name}"));

        self.line(&format!("format_version: {}", self.project.format_version));
        self.line(&format!(
            "sample_rate: {}",
            fmt_int(self.project.sample_rate() as u64)
        ));
        if let Some(c) = &self.project.metadata.created_at {
            self.line(&format!("created: {}", quote(c)));
        }
        if let Some(m) = &self.project.metadata.modified_at {
            self.line(&format!("modified: {}", quote(m)));
        }
        self.out.push('\n');

        self.emit_sources()?;
        if !self.project.tracks.is_empty() {
            self.out.push('\n');
            self.emit_tracks()?;
        }
        self.out.push('\n');
        self.emit_master()?;
        if !self.project.markers.is_empty() {
            self.out.push('\n');
            self.emit_markers();
        }
        self.out.push('\n');
        self.emit_transport();
        self.out.push('\n');
        self.emit_view();

        if !self.project.noise_profiles.is_empty() {
            return Err(EmitError::Unsupported("noise_profiles"));
        }

        self.close();
        Ok(())
    }

    fn emit_sources(&mut self) -> Result<(), EmitError> {
        self.open("sources");
        for source in self.project.sources.values() {
            self.emit_source(source)?;
        }
        self.close();
        Ok(())
    }

    fn emit_source(&mut self, source: &Source) -> Result<(), EmitError> {
        let header = format!("{} {}", source.id, quote(&source.name));
        self.open(&header);
        self.line(&format!("channels: {}", source.channel_count));
        self.line(&format!(
            "sample_rate: {}",
            fmt_int(source.sample_rate as u64)
        ));
        self.line(&format!("base_file: {}", quote(source.base_file.as_str())));
        self.line(&format!("base_length: {}", fmt_int(source.base_length)));
        self.line(&format!("history_pointer: {}", source.edits.pointer()));

        if !source.edits.is_empty() {
            self.out.push('\n');
            self.open("ops");
            for op in source.edits.active() {
                self.emit_op(op, source.sample_rate)?;
            }
            self.close();
        }

        self.close();
        Ok(())
    }

    fn emit_op(&mut self, op: &Op, sample_rate: u32) -> Result<(), EmitError> {
        match op {
            Op::Silence { range } => self.line(&format!(
                "{}  silence",
                fmt_range(*range, sample_rate)
            )),
            Op::Gain { range, db } => self.line(&format!(
                "{}  gain {}",
                fmt_range(*range, sample_rate),
                fmt_db(*db)
            )),
            Op::Reverse { range } => self.line(&format!(
                "{}  reverse",
                fmt_range(*range, sample_rate)
            )),
            Op::DcRemove { range } => self.line(&format!(
                "{}  dc_remove",
                fmt_range(*range, sample_rate)
            )),
            Op::Fade {
                range,
                shape,
                direction,
            } => self.line(&format!(
                "{}  {} shape:{}",
                fmt_range(*range, sample_rate),
                fade_keyword(*direction),
                fade_shape_name(*shape)
            )),
            Op::Normalize {
                range,
                target,
                value_db,
            } => self.line(&format!(
                "{}  normalize target:{} value:{}",
                fmt_range(*range, sample_rate),
                norm_target_name(*target),
                fmt_db(*value_db)
            )),
            Op::Cut { range } => self.line(&format!(
                "{}  cut",
                fmt_range(*range, sample_rate)
            )),
            Op::TimeStretch { range, ratio } => self.line(&format!(
                "{}  time_stretch ratio:{}",
                fmt_range(*range, sample_rate),
                fmt_float(*ratio)
            )),
            Op::PitchShift { range, cents } => self.line(&format!(
                "{}  pitch_shift cents:{}",
                fmt_range(*range, sample_rate),
                fmt_float(*cents)
            )),
            Op::Generate { at, length, params } => {
                self.emit_generate(*at, *length, params, sample_rate)?
            }
            Op::Delay { range, params } => self.line(&format!(
                "{}  delay {}",
                fmt_range(*range, sample_rate),
                fmt_delay_params(params)
            )),
            Op::Compress { range, params } => self.line(&format!(
                "{}  compress {}",
                fmt_range(*range, sample_rate),
                fmt_comp_params(params)
            )),
            Op::Limit { range, params } => self.line(&format!(
                "{}  limit {}",
                fmt_range(*range, sample_rate),
                fmt_limit_params(params)
            )),

            Op::Insert { .. }
            | Op::PasteMix { .. }
            | Op::PasteOver { .. } => {
                return Err(EmitError::Unsupported("clipboard ops"));
            }
            Op::Eq { .. } | Op::Reverb { .. } => {
                return Err(EmitError::Unsupported("effect param blocks"));
            }
            Op::NoiseReduce { .. } => return Err(EmitError::Unsupported("noise_reduce")),
            Op::SpectralEdit { .. } => return Err(EmitError::Unsupported("spectral edit")),
        }
        Ok(())
    }

    fn emit_generate(
        &mut self,
        at: u64,
        length: u64,
        params: &GeneratorParams,
        sample_rate: u32,
    ) -> Result<(), EmitError> {
        let at_lit = fmt_at(at, sample_rate);
        let len_lit = fmt_duration(length, sample_rate);
        let body = match params {
            GeneratorParams::Silence => "kind:silence".to_string(),
            GeneratorParams::Tone {
                shape,
                frequency_hz,
                amplitude_db,
            } => format!(
                "kind:tone shape:{} freq:{} amplitude:{}",
                tone_shape_name(*shape),
                fmt_float(*frequency_hz),
                fmt_db(*amplitude_db)
            ),
            GeneratorParams::Noise {
                color,
                amplitude_db,
            } => format!(
                "kind:noise color:{} amplitude:{}",
                noise_color_name(*color),
                fmt_db(*amplitude_db)
            ),
            GeneratorParams::Dtmf { .. } => return Err(EmitError::Unsupported("dtmf")),
            GeneratorParams::Sweep { .. } => return Err(EmitError::Unsupported("sweep")),
        };
        self.line(&format!("generate at:{at_lit} length:{len_lit} {body}"));
        Ok(())
    }

    fn emit_tracks(&mut self) -> Result<(), EmitError> {
        self.open("tracks");
        for track in &self.project.tracks {
            self.emit_track(track)?;
        }
        self.close();
        Ok(())
    }

    fn emit_track(&mut self, track: &Track) -> Result<(), EmitError> {
        let name = quote(&track.name);
        self.open(&format!("track {name}"));
        self.line(&format!("height: {}", fmt_float(track.height)));
        self.line(&format!("gain: {}", fmt_db(track.gain_db)));
        self.line(&format!("pan: {}", fmt_float(track.pan)));
        if track.mute {
            self.line("mute: true");
        }
        if track.solo {
            self.line("solo: true");
        }
        if track.arm {
            self.line("arm: true");
        }

        if !track.inserts.is_empty() {
            return Err(EmitError::Unsupported("track inserts"));
        }
        if !track.automation.is_empty() {
            return Err(EmitError::Unsupported("track automation"));
        }

        if !track.clips.is_empty() {
            self.out.push('\n');
            self.open("clips");
            for clip in &track.clips {
                self.emit_clip(clip)?;
            }
            self.close();
        }

        self.close();
        Ok(())
    }

    fn emit_clip(&mut self, clip: &Clip) -> Result<(), EmitError> {
        let sr = self.project.sample_rate();
        self.open(&format!("clip from {}", clip.source_id));
        self.line(&format!("name: {}", quote(&clip.name)));
        self.line(&format!(
            "at: {}",
            fmt_at(clip.track_position.start(), sr)
        ));
        self.line(&format!("in: {}", fmt_at(clip.source_in, sr)));
        self.line(&format!("out: {}", fmt_at(clip.source_out, sr)));
        if clip.gain_db != 0.0 {
            self.line(&format!("gain: {}", fmt_db(clip.gain_db)));
        }
        if clip.pan != 0.0 {
            self.line(&format!("pan: {}", fmt_float(clip.pan)));
        }
        if clip.fade_in.duration_samples > 0 {
            self.line(&format!(
                "fade_in:  {{ duration: {}, shape: {} }}",
                fmt_duration(clip.fade_in.duration_samples, sr),
                fade_shape_name(clip.fade_in.shape)
            ));
        }
        if clip.fade_out.duration_samples > 0 {
            self.line(&format!(
                "fade_out: {{ duration: {}, shape: {} }}",
                fmt_duration(clip.fade_out.duration_samples, sr),
                fade_shape_name(clip.fade_out.shape)
            ));
        }
        if clip.time_stretch != 1.0 {
            self.line(&format!("time_stretch: {}", fmt_float(clip.time_stretch)));
        }
        if clip.pitch_shift_cents != 0.0 {
            self.line(&format!(
                "pitch_shift_cents: {}",
                fmt_float(clip.pitch_shift_cents)
            ));
        }
        if clip.locked {
            self.line("locked: true");
        }
        if !clip.envelopes.is_empty() {
            return Err(EmitError::Unsupported("clip envelopes"));
        }
        self.close();
        Ok(())
    }

    fn emit_master(&mut self) -> Result<(), EmitError> {
        let MasterBus { gain_db, inserts } = &self.project.master;
        if !inserts.is_empty() {
            return Err(EmitError::Unsupported("master inserts"));
        }
        self.open("master");
        self.line(&format!("gain: {}", fmt_db(*gain_db)));
        self.close();
        Ok(())
    }

    fn emit_markers(&mut self) {
        self.open("markers");
        let sr = self.project.sample_rate();
        for m in &self.project.markers {
            self.line(&format!("marker {} {}", quote(&m.name), fmt_at(m.time, sr)));
        }
        self.close();
    }

    fn emit_transport(&mut self) {
        self.open("transport");
        self.line(&format!(
            "playhead: {}",
            fmt_at(self.project.transport.playhead, self.project.sample_rate())
        ));
        self.line(&format!("loop: {}", self.project.transport.looping));
        self.close();
    }

    fn emit_view(&mut self) {
        self.open("view");
        self.line(&format!("zoom: {}", fmt_float(self.project.view.zoom)));
        self.line(&format!(
            "scroll: {}",
            fmt_at(self.project.view.scroll_samples, self.project.sample_rate())
        ));
        self.line(&format!(
            "active_view: {}",
            match self.project.view.active_view {
                crate::project::ActiveView::Waveform => "waveform",
                crate::project::ActiveView::Spectral => "spectral",
            }
        ));
        self.close();
    }
}

fn fmt_delay_params(p: &DelayParams) -> String {
    let mut s = format!(
        "time:{}ms feedback:{} mix:{}",
        fmt_float(p.time_ms),
        fmt_float(p.feedback),
        fmt_float(p.mix)
    );
    if p.ping_pong {
        s.push_str(" ping_pong:true");
    }
    if let Some(hz) = p.feedback_lp_hz {
        s.push_str(&format!(" feedback_lp:{}Hz", fmt_float(hz)));
    }
    s
}

fn fmt_comp_params(p: &CompParams) -> String {
    format!(
        "threshold:{} ratio:{} attack:{}ms release:{}ms makeup:{} knee:{}",
        fmt_db(p.threshold_db),
        fmt_float(p.ratio),
        fmt_float(p.attack_ms),
        fmt_float(p.release_ms),
        fmt_db(p.makeup_db),
        fmt_db(p.knee_db)
    )
}

fn fmt_limit_params(p: &LimitParams) -> String {
    format!(
        "ceiling:{} lookahead:{}ms release:{}ms",
        fmt_db(p.ceiling_db),
        fmt_float(p.lookahead_ms),
        fmt_float(p.release_ms)
    )
}

fn fade_keyword(d: FadeDirection) -> &'static str {
    match d {
        FadeDirection::In => "fade_in",
        FadeDirection::Out => "fade_out",
    }
}

fn fade_shape_name(s: FadeShape) -> &'static str {
    match s {
        FadeShape::Linear => "linear",
        FadeShape::Logarithmic => "log",
        FadeShape::Exponential => "exp",
        FadeShape::SCurve => "scurve",
    }
}

fn norm_target_name(t: NormTarget) -> &'static str {
    match t {
        NormTarget::Peak => "peak",
        NormTarget::Rms => "rms",
        NormTarget::LufsIntegrated => "lufs",
    }
}

fn tone_shape_name(t: ToneShape) -> &'static str {
    match t {
        ToneShape::Sine => "sine",
        ToneShape::Square => "square",
        ToneShape::Saw => "saw",
        ToneShape::Triangle => "triangle",
    }
}

fn noise_color_name(c: NoiseColor) -> &'static str {
    match c {
        NoiseColor::White => "white",
        NoiseColor::Pink => "pink",
        NoiseColor::Brown => "brown",
    }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Format an integer with `_` separators every 3 digits when it gets long
/// enough that they help (matches doc 04 examples like `14_400_000`).
fn fmt_int(n: u64) -> String {
    if n < 10_000 {
        return n.to_string();
    }
    let s = n.to_string();
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, c) in bytes.iter().enumerate() {
        if i > 0 && (len - i).is_multiple_of(3) {
            out.push('_');
        }
        out.push(*c);
    }
    out
}

fn fmt_float(v: f32) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        // Trim trailing zeros to keep emit output stable.
        let s = format!("{v}");
        s
    }
}

fn fmt_db(v: f32) -> String {
    if v.is_infinite() && v.is_sign_negative() {
        return "-inf".into();
    }
    if v == 0.0 {
        return "0dB".into();
    }
    if v.fract() == 0.0 {
        format!("{:+}dB", v as i64)
    } else {
        let mut s = format!("{:+}", v);
        // Trim a trailing zero or two for stable output.
        while s.ends_with('0') && s.contains('.') {
            s.pop();
        }
        if s.ends_with('.') {
            s.push('0');
        }
        format!("{s}dB")
    }
}

/// `@HH:MM:SS.sss` form for a sample frame at `sample_rate`.
fn fmt_at(samples: u64, sample_rate: u32) -> String {
    format!("@{}", fmt_hms(samples, sample_rate))
}

/// Time range form used by source ops: `@HH:MM:SS.sss - @HH:MM:SS.sss`.
fn fmt_range(range: SampleRange, sample_rate: u32) -> String {
    format!(
        "{} - {}",
        fmt_at(range.start(), sample_rate),
        fmt_at(range.end(), sample_rate)
    )
}

/// Duration (no `@`). Sub-second values in ms; longer values in HMS.
fn fmt_duration(samples: u64, sample_rate: u32) -> String {
    let secs = samples as f64 / sample_rate as f64;
    if secs < 1.0 {
        let ms = secs * 1000.0;
        if ms.fract() == 0.0 {
            format!("{}ms", ms as u64)
        } else {
            format!("{ms}ms")
        }
    } else {
        fmt_hms(samples, sample_rate)
    }
}

fn fmt_hms(samples: u64, sample_rate: u32) -> String {
    let secs_total = samples as f64 / sample_rate as f64;
    let h = (secs_total / 3600.0).floor() as u32;
    let after_h = secs_total - h as f64 * 3600.0;
    let m = (after_h / 60.0).floor() as u32;
    let s = after_h - m as f64 * 60.0;
    let mut out = format!("{h:02}:{m:02}:{s:06.3}");
    // Strip trailing zeros from the fractional part if present, but always
    // keep three digits (matches doc 04's `@00:00:34.100` shape).
    if !out.contains('.') {
        out.push_str(".000");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::edit_list::EditList;
    use crate::ids::{ClipId, EffectInstanceId, SourceId, TrackId};
    use crate::project::{Clip, Fade, Marker, Project};
    use crate::range::SampleRange;
    use crate::source::{Source, StoragePath, Timestamp};

    fn fixture_source(id: &str, frames: u64, native_rate: u32) -> Source {
        let id = SourceId::new(id);
        Source::new(
            id.clone(),
            "vocals_raw.wav",
            1,
            native_rate,
            StoragePath::new(format!("sources/{id}/base.f32")),
            frames,
            Timestamp("2026-05-03T12:00:00Z".into()),
        )
    }

    #[test]
    fn empty_project_emits_minimal_shell() {
        let p = Project::new(96_000);
        let dsl = project_to_dsl(&p).unwrap();
        // Header + sources + master + transport + view blocks at minimum.
        assert!(dsl.starts_with("project \"\""));
        assert!(dsl.contains("format_version: 1"));
        assert!(dsl.contains("sample_rate: 96_000"));
        assert!(dsl.contains("master {"));
        assert!(dsl.contains("transport {"));
        assert!(dsl.contains("view {"));
    }

    #[test]
    fn emits_source_block_with_metadata() {
        let mut p = Project::new(96_000);
        let s = fixture_source("src_a4f2", 14_400_000, 48_000);
        p.sources.insert(s.id.clone(), s);
        let dsl = project_to_dsl(&p).unwrap();
        assert!(dsl.contains("src_a4f2 \"vocals_raw.wav\""));
        assert!(dsl.contains("channels: 1"));
        assert!(dsl.contains("sample_rate: 48_000"));
        assert!(dsl.contains("base_length: 14_400_000"));
        assert!(dsl.contains("history_pointer: 0"));
    }

    #[test]
    fn emits_silence_and_gain_ops_in_hms_form() {
        let mut p = Project::new(96_000);
        let mut s = fixture_source("src_a4f2", 96_000 * 5, 96_000);
        s.edits = EditList::new();
        s.edits.apply(Op::Silence {
            range: SampleRange::new(96_000, 96_000 * 2).unwrap(),
        });
        s.edits.apply(Op::Gain {
            range: SampleRange::new(96_000 * 3, 96_000 * 4).unwrap(),
            db: -3.0,
        });
        p.sources.insert(s.id.clone(), s);
        let dsl = project_to_dsl(&p).unwrap();
        assert!(dsl.contains("@00:00:01.000 - @00:00:02.000  silence"), "got:\n{dsl}");
        assert!(dsl.contains("@00:00:03.000 - @00:00:04.000  gain -3dB"), "got:\n{dsl}");
    }

    #[test]
    fn emits_normalize_with_target_and_db_value() {
        let mut p = Project::new(96_000);
        let mut s = fixture_source("src_a", 100, 96_000);
        s.edits.apply(Op::Normalize {
            range: SampleRange::new(0, 100).unwrap(),
            target: NormTarget::Peak,
            value_db: -1.0,
        });
        p.sources.insert(s.id.clone(), s);
        let dsl = project_to_dsl(&p).unwrap();
        assert!(dsl.contains("normalize target:peak value:-1dB"), "got:\n{dsl}");
    }

    #[test]
    fn emits_generate_tone() {
        let mut p = Project::new(96_000);
        let mut s = fixture_source("src_a", 100, 96_000);
        s.edits.apply(Op::Generate {
            at: 0,
            length: 96_000 / 2, // 500ms at 96k
            params: GeneratorParams::Tone {
                shape: ToneShape::Sine,
                frequency_hz: 440.0,
                amplitude_db: -12.0,
            },
        });
        p.sources.insert(s.id.clone(), s);
        let dsl = project_to_dsl(&p).unwrap();
        assert!(
            dsl.contains("generate at:@00:00:00.000 length:500ms kind:tone shape:sine freq:440 amplitude:-12dB"),
            "got:\n{dsl}"
        );
    }

    #[test]
    fn emits_track_with_clip_referencing_source() {
        let mut p = Project::new(96_000);
        let s = fixture_source("src_a", 96_000 * 10, 96_000);
        let src_id = s.id.clone();
        p.sources.insert(src_id.clone(), s);

        let track = Track {
            id: TrackId(1),
            name: "Vocal".into(),
            height: 80.0,
            mute: false,
            solo: false,
            arm: false,
            gain_db: 0.0,
            pan: 0.0,
            inserts: Vec::new(),
            automation: Vec::new(),
            clips: vec![Clip {
                id: ClipId(1),
                source_id: src_id,
                name: "Take 1".into(),
                track_position: SampleRange::new(0, 96_000 * 4).unwrap(),
                source_in: 0,
                source_out: 96_000 * 4,
                gain_db: 0.0,
                pan: 0.0,
                fade_in: Fade {
                    duration_samples: 96 * 50,
                    shape: FadeShape::Linear,
                },
                fade_out: Fade::none(),
                time_stretch: 1.0,
                pitch_shift_cents: 0.0,
                envelopes: Vec::new(),
                locked: false,
                group: None,
            }],
        };
        p.tracks.push(track);

        let dsl = project_to_dsl(&p).unwrap();
        assert!(dsl.contains("track \"Vocal\""), "got:\n{dsl}");
        assert!(dsl.contains("clip from src_a"), "got:\n{dsl}");
        assert!(dsl.contains("at: @00:00:00.000"), "got:\n{dsl}");
        assert!(dsl.contains("out: @00:00:04.000"), "got:\n{dsl}");
        assert!(dsl.contains("fade_in:  { duration: 50ms, shape: linear }"), "got:\n{dsl}");
    }

    #[test]
    fn emits_markers() {
        let mut p = Project::new(96_000);
        p.markers.push(Marker {
            name: "Verse 1".into(),
            time: 0,
        });
        p.markers.push(Marker {
            name: "Chorus".into(),
            time: 96_000 * 48,
        });
        let dsl = project_to_dsl(&p).unwrap();
        assert!(dsl.contains("marker \"Verse 1\" @00:00:00.000"), "got:\n{dsl}");
        assert!(dsl.contains("marker \"Chorus\" @00:00:48.000"), "got:\n{dsl}");
    }

    #[test]
    fn emits_delay_compress_limit_ops() {
        use crate::effect::{CompParams, DelayParams, LimitParams};
        let mut p = Project::new(96_000);
        let mut s = fixture_source("src_a", 96_000, 96_000);
        s.edits.apply(Op::Delay {
            range: SampleRange::new(0, 96_000).unwrap(),
            params: DelayParams {
                time_ms: 250.0,
                feedback: 0.4,
                mix: 0.3,
                ping_pong: true,
                feedback_lp_hz: None,
            },
        });
        s.edits.apply(Op::Compress {
            range: SampleRange::new(0, 96_000).unwrap(),
            params: CompParams {
                threshold_db: -18.0,
                ratio: 3.0,
                attack_ms: 5.0,
                release_ms: 80.0,
                makeup_db: 3.0,
                knee_db: 6.0,
            },
        });
        s.edits.apply(Op::Limit {
            range: SampleRange::new(0, 96_000).unwrap(),
            params: LimitParams {
                ceiling_db: -0.3,
                lookahead_ms: 5.0,
                release_ms: 50.0,
            },
        });
        p.sources.insert(s.id.clone(), s);
        let dsl = project_to_dsl(&p).unwrap();
        assert!(
            dsl.contains("delay time:250ms feedback:0.4 mix:0.3 ping_pong:true"),
            "delay missing\n{dsl}"
        );
        assert!(
            dsl.contains("compress threshold:-18dB ratio:3 attack:5ms release:80ms makeup:+3dB knee:+6dB"),
            "compress missing\n{dsl}"
        );
        assert!(
            dsl.contains("limit ceiling:-0.3dB lookahead:5ms release:50ms"),
            "limit missing\n{dsl}"
        );
    }

    #[test]
    fn unsupported_features_report_clear_errors() {
        use crate::effect::EqParams;
        let mut p = Project::new(96_000);
        let mut s = fixture_source("src_a", 100, 96_000);
        s.edits.apply(Op::Eq {
            range: SampleRange::new(0, 100).unwrap(),
            params: EqParams::default(),
        });
        p.sources.insert(s.id.clone(), s);
        let err = project_to_dsl(&p).unwrap_err();
        assert!(matches!(err, EmitError::Unsupported(_)));
    }

    #[test]
    fn track_inserts_unsupported_for_now() {
        use crate::effect::EffectKind;
        use crate::project::EffectInstance;
        use std::collections::BTreeMap;

        let mut p = Project::new(96_000);
        let mut t = Track {
            id: TrackId(1),
            name: "T".into(),
            height: 80.0,
            mute: false,
            solo: false,
            arm: false,
            gain_db: 0.0,
            pan: 0.0,
            inserts: Vec::new(),
            automation: Vec::new(),
            clips: Vec::new(),
        };
        t.inserts.push(EffectInstance {
            id: EffectInstanceId(1),
            kind: EffectKind::Eq,
            bypass: false,
            params: BTreeMap::new(),
        });
        p.tracks.push(t);
        let err = project_to_dsl(&p).unwrap_err();
        assert!(matches!(err, EmitError::Unsupported("track inserts")));
    }

    #[test]
    fn integer_underscores_kick_in_after_four_digits() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(9_999), "9999");
        assert_eq!(fmt_int(10_000), "10_000");
        assert_eq!(fmt_int(1_234_567), "1_234_567");
    }

    #[test]
    fn db_formatting_handles_signs_and_inf() {
        assert_eq!(fmt_db(0.0), "0dB");
        assert_eq!(fmt_db(-3.0), "-3dB");
        assert_eq!(fmt_db(6.0), "+6dB");
        assert_eq!(fmt_db(-1.5), "-1.5dB");
        assert_eq!(fmt_db(f32::NEG_INFINITY), "-inf");
    }

    #[test]
    fn hms_formatting_matches_doc_04_shape() {
        assert_eq!(fmt_hms(0, 96_000), "00:00:00.000");
        assert_eq!(fmt_hms(96_000, 96_000), "00:00:01.000");
        assert_eq!(fmt_hms(96_000 * 60, 96_000), "00:01:00.000");
        assert_eq!(fmt_hms(96_000 * 3600, 96_000), "01:00:00.000");
    }
}
