//! Recursive-descent parser for `.keds`.
//!
//! Mirrors the emitter's coverage: project header, sources (with the
//! sample-region ops the emitter writes), tracks with simple clip
//! parameters, markers, master gain, transport, view. Effect insert
//! parameter blocks, envelopes, automation, generators, spectral edits,
//! noise reduction, and clipboard ops return [`ParseError::Unsupported`].

use std::collections::BTreeMap;
use std::fmt;

use crate::edit_list::EditList;
use crate::ids::{ClipId, SourceId, TrackId};
use crate::op::{FadeDirection, FadeShape, NormTarget, Op};
use crate::project::{ActiveView, Clip, Fade, Marker, Project, Track};
use crate::range::SampleRange;
use crate::source::{Source, StoragePath, Timestamp};

use super::lex::{lex, LexError, Spanned, TimeForm, Token};

#[derive(Debug)]
pub enum ParseError {
    Lex(LexError),
    Unexpected {
        line: u32,
        col: u32,
        message: String,
    },
    Unsupported(&'static str),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Lex(e) => write!(f, "{e}"),
            Self::Unexpected { line, col, message } => {
                write!(f, "parse error at {line}:{col}: {message}")
            }
            Self::Unsupported(k) => write!(f, "DSL parse: feature `{k}` not yet supported"),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self::Lex(e)
    }
}

pub fn parse_project(input: &str) -> Result<Project, ParseError> {
    let tokens = lex(input)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        sample_rate: 96_000,
    };
    p.parse_project()
}

struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
    /// Project sample rate, learned from `sample_rate: N` and used to
    /// convert subsequent time literals into sample frames.
    sample_rate: u32,
}

impl Parser {
    fn cur(&self) -> &Spanned {
        &self.tokens[self.pos]
    }

    fn cur_tok(&self) -> &Token {
        &self.cur().token
    }

    fn bump(&mut self) -> Spanned {
        let s = self.tokens[self.pos].clone();
        if !matches!(s.token, Token::Eof) {
            self.pos += 1;
        }
        s
    }

    fn err(&self, message: impl Into<String>) -> ParseError {
        ParseError::Unexpected {
            line: self.cur().line,
            col: self.cur().col,
            message: message.into(),
        }
    }

    fn expect(&mut self, expected: &Token) -> Result<(), ParseError> {
        if self.cur_tok() == expected {
            self.bump();
            Ok(())
        } else {
            Err(self.err(format!("expected {expected:?}, got {:?}", self.cur_tok())))
        }
    }

    fn expect_ident(&mut self, name: &str) -> Result<(), ParseError> {
        match self.cur_tok() {
            Token::Ident(s) if s == name => {
                self.bump();
                Ok(())
            }
            other => Err(self.err(format!("expected `{name}`, got {other:?}"))),
        }
    }


    fn parse_project(&mut self) -> Result<Project, ParseError> {
        self.expect_ident("project")?;
        let name = self.parse_string()?;
        self.expect(&Token::LBrace)?;

        let mut format_version: Option<u32> = None;
        let mut sample_rate: Option<u32> = None;
        let mut created: Option<String> = None;
        let mut modified: Option<String> = None;
        let mut sources: BTreeMap<SourceId, Source> = BTreeMap::new();
        let mut tracks: Vec<Track> = Vec::new();
        let mut markers: Vec<Marker> = Vec::new();
        let mut master_gain_db: f32 = 0.0;
        let mut playhead: u64 = 0;
        let mut looping = false;
        let mut zoom: f32 = 1.0;
        let mut scroll: u64 = 0;
        let mut active_view = ActiveView::Waveform;

        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            match self.cur_tok() {
                Token::Ident(s) => match s.as_str() {
                    "format_version" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        format_version = Some(self.parse_uint()? as u32);
                    }
                    "sample_rate" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        let v = self.parse_uint()? as u32;
                        sample_rate = Some(v);
                        self.sample_rate = v;
                    }
                    "created" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        created = Some(self.parse_string()?);
                    }
                    "modified" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        modified = Some(self.parse_string()?);
                    }
                    "sources" => {
                        self.bump();
                        sources = self.parse_sources_block()?;
                    }
                    "tracks" => {
                        self.bump();
                        tracks = self.parse_tracks_block()?;
                    }
                    "master" => {
                        self.bump();
                        master_gain_db = self.parse_master_block()?;
                    }
                    "markers" => {
                        self.bump();
                        markers = self.parse_markers_block()?;
                    }
                    "transport" => {
                        self.bump();
                        let (ph, lp) = self.parse_transport_block()?;
                        playhead = ph;
                        looping = lp;
                    }
                    "view" => {
                        self.bump();
                        let (z, sc, av) = self.parse_view_block()?;
                        zoom = z;
                        scroll = sc;
                        active_view = av;
                    }
                    other => {
                        return Err(self.err(format!("unexpected key `{other}` in project")));
                    }
                },
                other => {
                    return Err(self.err(format!("unexpected token {other:?} in project")));
                }
            }
        }
        self.expect(&Token::RBrace)?;

        let format_version = format_version.unwrap_or(crate::FORMAT_VERSION);
        if format_version != crate::FORMAT_VERSION {
            return Err(self.err(format!(
                "format_version {format_version} not supported (this build expects {})",
                crate::FORMAT_VERSION
            )));
        }
        let sample_rate = sample_rate.unwrap_or(crate::DEFAULT_PROJECT_SAMPLE_RATE);

        let mut project = Project::new(sample_rate);
        project.format_version = format_version;
        project.metadata.name = name;
        project.metadata.created_at = created;
        project.metadata.modified_at = modified;
        project.sources = sources;
        project.tracks = tracks;
        project.master.gain_db = master_gain_db;
        project.markers = markers;
        project.transport.playhead = playhead;
        project.transport.looping = looping;
        project.view.zoom = zoom;
        project.view.scroll_samples = scroll;
        project.view.active_view = active_view;
        Ok(project)
    }

    fn parse_sources_block(&mut self) -> Result<BTreeMap<SourceId, Source>, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut sources = BTreeMap::new();
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            let source = self.parse_source()?;
            sources.insert(source.id.clone(), source);
        }
        self.expect(&Token::RBrace)?;
        Ok(sources)
    }

    fn parse_source(&mut self) -> Result<Source, ParseError> {
        let id = self.parse_ident()?;
        let id = SourceId::new(id);
        let name = self.parse_string()?;
        self.expect(&Token::LBrace)?;

        let mut channels: u16 = 1;
        let mut sample_rate: u32 = 96_000;
        let mut base_file = String::new();
        let mut base_length: u64 = 0;
        let mut history_pointer: usize = 0;
        let mut ops: Vec<Op> = Vec::new();

        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            match self.cur_tok() {
                Token::Ident(s) => match s.as_str() {
                    "channels" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        channels = self.parse_uint()? as u16;
                    }
                    "sample_rate" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        sample_rate = self.parse_uint()? as u32;
                    }
                    "base_file" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        base_file = self.parse_string()?;
                    }
                    "base_length" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        base_length = self.parse_uint()?;
                    }
                    "history_pointer" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        history_pointer = self.parse_uint()? as usize;
                    }
                    "ops" => {
                        self.bump();
                        ops = self.parse_ops_block(sample_rate)?;
                    }
                    other => {
                        return Err(self.err(format!("unexpected key `{other}` in source")));
                    }
                },
                other => return Err(self.err(format!("unexpected token in source: {other:?}"))),
            }
        }
        self.expect(&Token::RBrace)?;

        let mut source = Source::new(
            id,
            name,
            channels,
            sample_rate,
            StoragePath::new(base_file),
            base_length,
            Timestamp(String::new()),
        );
        let mut edits = EditList::new();
        for op in ops {
            edits.apply(op);
        }
        // Walk back to the requested history pointer.
        while edits.pointer() > history_pointer {
            edits.undo();
        }
        source.edits = edits;
        Ok(source)
    }

    fn parse_ops_block(&mut self, source_rate: u32) -> Result<Vec<Op>, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut ops = Vec::new();
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            ops.push(self.parse_one_op(source_rate)?);
        }
        self.expect(&Token::RBrace)?;
        Ok(ops)
    }

    fn parse_one_op(&mut self, rate: u32) -> Result<Op, ParseError> {
        // Either `@TIME - @TIME  KEYWORD ...` (range op) or
        // `generate at:@TIME length:DUR kind:NAME ...`.
        match self.cur_tok() {
            Token::Ident(s) if s == "generate" => self.parse_generate_op(rate),
            Token::TimeLit(_) => self.parse_range_op(rate),
            other => Err(self.err(format!("unexpected op start {other:?}"))),
        }
    }

    fn parse_range_op(&mut self, rate: u32) -> Result<Op, ParseError> {
        let from = self.parse_time(rate)?;
        self.expect(&Token::Dash)?;
        let to = self.parse_time(rate)?;
        let range = SampleRange::new(from, to)
            .map_err(|e| self.err(format!("bad range: {e}")))?;
        let name = self.parse_ident()?;
        let op = match name.as_str() {
            "silence" => Op::Silence { range },
            "reverse" => Op::Reverse { range },
            "dc_remove" => Op::DcRemove { range },
            "cut" => Op::Cut { range },
            "gain" => Op::Gain {
                range,
                db: self.parse_db()?,
            },
            "fade_in" | "fade_out" => {
                let direction = if name == "fade_in" {
                    FadeDirection::In
                } else {
                    FadeDirection::Out
                };
                self.expect_ident("shape")?;
                self.expect(&Token::Colon)?;
                let shape = self.parse_fade_shape()?;
                Op::Fade {
                    range,
                    shape,
                    direction,
                }
            }
            "normalize" => {
                self.expect_ident("target")?;
                self.expect(&Token::Colon)?;
                let target = self.parse_norm_target()?;
                self.expect_ident("value")?;
                self.expect(&Token::Colon)?;
                let value_db = self.parse_db()?;
                Op::Normalize {
                    range,
                    target,
                    value_db,
                }
            }
            "time_stretch" => {
                self.expect_ident("ratio")?;
                self.expect(&Token::Colon)?;
                Op::TimeStretch {
                    range,
                    ratio: self.parse_float()?,
                }
            }
            "pitch_shift" => {
                self.expect_ident("cents")?;
                self.expect(&Token::Colon)?;
                Op::PitchShift {
                    range,
                    cents: self.parse_float()?,
                }
            }
            other => return Err(ParseError::Unsupported(static_op_name(other))),
        };
        Ok(op)
    }

    fn parse_generate_op(&mut self, rate: u32) -> Result<Op, ParseError> {
        self.expect_ident("generate")?;
        self.expect_ident("at")?;
        self.expect(&Token::Colon)?;
        let at = self.parse_time(rate)?;
        self.expect_ident("length")?;
        self.expect(&Token::Colon)?;
        let length = self.parse_duration(rate)?;
        self.expect_ident("kind")?;
        self.expect(&Token::Colon)?;
        let kind = self.parse_ident()?;
        let params = match kind.as_str() {
            "silence" => crate::op::GeneratorParams::Silence,
            "tone" => {
                self.expect_ident("shape")?;
                self.expect(&Token::Colon)?;
                let shape = match self.parse_ident()?.as_str() {
                    "sine" => crate::op::ToneShape::Sine,
                    "square" => crate::op::ToneShape::Square,
                    "saw" => crate::op::ToneShape::Saw,
                    "triangle" => crate::op::ToneShape::Triangle,
                    other => {
                        return Err(self.err(format!("unknown tone shape `{other}`")));
                    }
                };
                self.expect_ident("freq")?;
                self.expect(&Token::Colon)?;
                let frequency_hz = self.parse_float()?;
                self.expect_ident("amplitude")?;
                self.expect(&Token::Colon)?;
                let amplitude_db = self.parse_db()?;
                crate::op::GeneratorParams::Tone {
                    shape,
                    frequency_hz,
                    amplitude_db,
                }
            }
            "noise" => {
                self.expect_ident("color")?;
                self.expect(&Token::Colon)?;
                let color = match self.parse_ident()?.as_str() {
                    "white" => crate::op::NoiseColor::White,
                    "pink" => crate::op::NoiseColor::Pink,
                    "brown" => crate::op::NoiseColor::Brown,
                    other => {
                        return Err(self.err(format!("unknown noise color `{other}`")));
                    }
                };
                self.expect_ident("amplitude")?;
                self.expect(&Token::Colon)?;
                let amplitude_db = self.parse_db()?;
                crate::op::GeneratorParams::Noise {
                    color,
                    amplitude_db,
                }
            }
            other => {
                return Err(ParseError::Unsupported(static_op_name(other)));
            }
        };
        Ok(Op::Generate { at, length, params })
    }

    fn parse_tracks_block(&mut self) -> Result<Vec<Track>, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut tracks = Vec::new();
        let mut next_id = 1u64;
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            tracks.push(self.parse_track(TrackId(next_id))?);
            next_id += 1;
        }
        self.expect(&Token::RBrace)?;
        Ok(tracks)
    }

    fn parse_track(&mut self, id: TrackId) -> Result<Track, ParseError> {
        self.expect_ident("track")?;
        let name = self.parse_string()?;
        self.expect(&Token::LBrace)?;

        let mut height = 80.0_f32;
        let mut gain_db = 0.0_f32;
        let mut pan = 0.0_f32;
        let mut mute = false;
        let mut solo = false;
        let mut arm = false;
        let mut clips: Vec<Clip> = Vec::new();
        let mut next_clip_id = 1u64;

        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            match self.cur_tok() {
                Token::Ident(s) => match s.as_str() {
                    "height" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        height = self.parse_float()?;
                    }
                    "gain" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        gain_db = self.parse_db()?;
                    }
                    "pan" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        pan = self.parse_float()?;
                    }
                    "mute" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        mute = self.parse_bool()?;
                    }
                    "solo" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        solo = self.parse_bool()?;
                    }
                    "arm" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        arm = self.parse_bool()?;
                    }
                    "clips" => {
                        self.bump();
                        clips = self.parse_clips_block(&mut next_clip_id)?;
                    }
                    "inserts" => return Err(ParseError::Unsupported("inserts")),
                    "automation" => return Err(ParseError::Unsupported("automation")),
                    other => {
                        return Err(self.err(format!("unexpected key `{other}` in track")));
                    }
                },
                other => return Err(self.err(format!("unexpected token in track: {other:?}"))),
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Track {
            id,
            name,
            height,
            mute,
            solo,
            arm,
            gain_db,
            pan,
            inserts: Vec::new(),
            automation: Vec::new(),
            clips,
        })
    }

    fn parse_clips_block(
        &mut self,
        next_id: &mut u64,
    ) -> Result<Vec<Clip>, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut clips = Vec::new();
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            clips.push(self.parse_clip(ClipId(*next_id))?);
            *next_id += 1;
        }
        self.expect(&Token::RBrace)?;
        Ok(clips)
    }

    fn parse_clip(&mut self, id: ClipId) -> Result<Clip, ParseError> {
        self.expect_ident("clip")?;
        self.expect_ident("from")?;
        let source_id = SourceId::new(self.parse_ident()?);
        self.expect(&Token::LBrace)?;

        let mut name = String::new();
        let mut at: u64 = 0;
        let mut s_in: u64 = 0;
        let mut s_out: u64 = 0;
        let mut gain_db = 0.0_f32;
        let mut pan = 0.0_f32;
        let mut fade_in = Fade::none();
        let mut fade_out = Fade::none();

        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            match self.cur_tok() {
                Token::Ident(s) => match s.as_str() {
                    "name" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        name = self.parse_string()?;
                    }
                    "at" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        at = self.parse_time(self.sample_rate)?;
                    }
                    "in" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        s_in = self.parse_time(self.sample_rate)?;
                    }
                    "out" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        s_out = self.parse_time(self.sample_rate)?;
                    }
                    "gain" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        gain_db = self.parse_db()?;
                    }
                    "pan" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        pan = self.parse_float()?;
                    }
                    "fade_in" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        fade_in = self.parse_fade_block()?;
                    }
                    "fade_out" => {
                        self.bump();
                        self.expect(&Token::Colon)?;
                        fade_out = self.parse_fade_block()?;
                    }
                    "envelope" => return Err(ParseError::Unsupported("clip envelopes")),
                    "time_stretch" | "pitch_shift_cents" | "locked" => {
                        return Err(ParseError::Unsupported("clip time/pitch/lock"));
                    }
                    other => {
                        return Err(self.err(format!("unexpected key `{other}` in clip")));
                    }
                },
                other => return Err(self.err(format!("unexpected token in clip: {other:?}"))),
            }
        }
        self.expect(&Token::RBrace)?;
        let track_position = SampleRange::new(at, at + (s_out.saturating_sub(s_in)))
            .map_err(|e| self.err(format!("bad clip position: {e}")))?;
        Ok(Clip {
            id,
            source_id,
            name,
            track_position,
            source_in: s_in,
            source_out: s_out,
            gain_db,
            pan,
            fade_in,
            fade_out,
            time_stretch: 1.0,
            pitch_shift_cents: 0.0,
            envelopes: Vec::new(),
            locked: false,
            group: None,
        })
    }

    fn parse_fade_block(&mut self) -> Result<Fade, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut duration_samples: u64 = 0;
        let mut shape = FadeShape::Linear;
        loop {
            match self.cur_tok() {
                Token::RBrace => {
                    self.bump();
                    break;
                }
                Token::Comma => {
                    self.bump();
                }
                Token::Ident(s) if s == "duration" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    duration_samples = self.parse_duration(self.sample_rate)?;
                }
                Token::Ident(s) if s == "shape" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    shape = self.parse_fade_shape()?;
                }
                other => return Err(self.err(format!("unexpected in fade: {other:?}"))),
            }
        }
        Ok(Fade {
            duration_samples,
            shape,
        })
    }

    fn parse_master_block(&mut self) -> Result<f32, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut gain_db = 0.0_f32;
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            match self.cur_tok() {
                Token::Ident(s) if s == "gain" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    gain_db = self.parse_db()?;
                }
                Token::Ident(s) if s == "inserts" => {
                    return Err(ParseError::Unsupported("master inserts"));
                }
                other => return Err(self.err(format!("unexpected in master: {other:?}"))),
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(gain_db)
    }

    fn parse_markers_block(&mut self) -> Result<Vec<Marker>, ParseError> {
        self.expect(&Token::LBrace)?;
        let mut markers = Vec::new();
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            self.expect_ident("marker")?;
            let name = self.parse_string()?;
            let time = self.parse_time(self.sample_rate)?;
            markers.push(Marker { name, time });
        }
        self.expect(&Token::RBrace)?;
        Ok(markers)
    }

    fn parse_transport_block(&mut self) -> Result<(u64, bool), ParseError> {
        self.expect(&Token::LBrace)?;
        let mut playhead = 0u64;
        let mut looping = false;
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            match self.cur_tok() {
                Token::Ident(s) if s == "playhead" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    playhead = self.parse_time(self.sample_rate)?;
                }
                Token::Ident(s) if s == "loop" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    looping = self.parse_bool()?;
                }
                other => return Err(self.err(format!("unexpected in transport: {other:?}"))),
            }
        }
        self.expect(&Token::RBrace)?;
        Ok((playhead, looping))
    }

    fn parse_view_block(&mut self) -> Result<(f32, u64, ActiveView), ParseError> {
        self.expect(&Token::LBrace)?;
        let mut zoom = 1.0_f32;
        let mut scroll = 0u64;
        let mut active = ActiveView::Waveform;
        while !matches!(self.cur_tok(), Token::RBrace | Token::Eof) {
            match self.cur_tok() {
                Token::Ident(s) if s == "zoom" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    zoom = self.parse_float()?;
                }
                Token::Ident(s) if s == "scroll" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    scroll = self.parse_time(self.sample_rate)?;
                }
                Token::Ident(s) if s == "active_view" => {
                    self.bump();
                    self.expect(&Token::Colon)?;
                    active = match self.parse_ident()?.as_str() {
                        "waveform" => ActiveView::Waveform,
                        "spectral" => ActiveView::Spectral,
                        other => {
                            return Err(self.err(format!("unknown active_view `{other}`")));
                        }
                    };
                }
                other => return Err(self.err(format!("unexpected in view: {other:?}"))),
            }
        }
        self.expect(&Token::RBrace)?;
        Ok((zoom, scroll, active))
    }

    fn parse_ident(&mut self) -> Result<String, ParseError> {
        match self.cur_tok().clone() {
            Token::Ident(s) => {
                self.bump();
                Ok(s)
            }
            other => Err(self.err(format!("expected identifier, got {other:?}"))),
        }
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        match self.cur_tok().clone() {
            Token::String(s) => {
                self.bump();
                Ok(s)
            }
            other => Err(self.err(format!("expected string, got {other:?}"))),
        }
    }

    fn parse_uint(&mut self) -> Result<u64, ParseError> {
        match self.cur_tok().clone() {
            Token::Integer(n) if n >= 0 => {
                self.bump();
                Ok(n as u64)
            }
            other => Err(self.err(format!("expected non-negative integer, got {other:?}"))),
        }
    }

    fn parse_float(&mut self) -> Result<f32, ParseError> {
        match self.cur_tok().clone() {
            Token::Integer(n) => {
                self.bump();
                Ok(n as f32)
            }
            Token::Float(f) => {
                self.bump();
                Ok(f as f32)
            }
            other => Err(self.err(format!("expected number, got {other:?}"))),
        }
    }

    fn parse_bool(&mut self) -> Result<bool, ParseError> {
        match self.cur_tok().clone() {
            Token::Ident(s) if s == "true" => {
                self.bump();
                Ok(true)
            }
            Token::Ident(s) if s == "false" => {
                self.bump();
                Ok(false)
            }
            other => Err(self.err(format!("expected true/false, got {other:?}"))),
        }
    }

    fn parse_db(&mut self) -> Result<f32, ParseError> {
        match self.cur_tok().clone() {
            Token::NegInf => {
                self.bump();
                Ok(f32::NEG_INFINITY)
            }
            Token::Suffixed { value, suffix } if suffix.eq_ignore_ascii_case("db") => {
                self.bump();
                Ok(value as f32)
            }
            other => Err(self.err(format!("expected dB literal, got {other:?}"))),
        }
    }

    fn parse_time(&mut self, rate: u32) -> Result<u64, ParseError> {
        match self.cur_tok().clone() {
            Token::TimeLit(form) => {
                self.bump();
                Ok(time_form_to_samples(form, rate))
            }
            other => Err(self.err(format!("expected time literal, got {other:?}"))),
        }
    }

    /// Durations look like `Nms`, `Nsec`, `Nsamples`, or HMS without `@`.
    /// Internally the lexer treats the latter as part of an `@` literal,
    /// so for now we only accept the suffixed numeric forms.
    fn parse_duration(&mut self, rate: u32) -> Result<u64, ParseError> {
        match self.cur_tok().clone() {
            Token::Suffixed { value, suffix } => {
                self.bump();
                let frames = match suffix.as_str() {
                    "ms" => (value / 1000.0 * rate as f64).round() as u64,
                    "sec" => (value * rate as f64).round() as u64,
                    "samples" | "s" => value as u64,
                    other => {
                        return Err(self.err(format!("unknown duration suffix `{other}`")));
                    }
                };
                Ok(frames)
            }
            other => Err(self.err(format!("expected duration, got {other:?}"))),
        }
    }

    fn parse_fade_shape(&mut self) -> Result<FadeShape, ParseError> {
        Ok(match self.parse_ident()?.as_str() {
            "linear" => FadeShape::Linear,
            "log" => FadeShape::Logarithmic,
            "exp" => FadeShape::Exponential,
            "scurve" => FadeShape::SCurve,
            other => return Err(self.err(format!("unknown fade shape `{other}`"))),
        })
    }

    fn parse_norm_target(&mut self) -> Result<NormTarget, ParseError> {
        Ok(match self.parse_ident()?.as_str() {
            "peak" => NormTarget::Peak,
            "rms" => NormTarget::Rms,
            "lufs" => NormTarget::LufsIntegrated,
            other => return Err(self.err(format!("unknown normalize target `{other}`"))),
        })
    }
}

fn time_form_to_samples(form: TimeForm, rate: u32) -> u64 {
    match form {
        TimeForm::Samples(n) => n,
        TimeForm::Seconds(s) => (s * rate as f64).round() as u64,
        TimeForm::Milliseconds(ms) => (ms / 1000.0 * rate as f64).round() as u64,
        TimeForm::Hms { h, m, s } => {
            let total = (h as f64) * 3600.0 + (m as f64) * 60.0 + s;
            (total * rate as f64).round() as u64
        }
        // Symbolic times don't have a sample frame in a static-parse context.
        // We map them to 0 for now; a future scripting layer can resolve them.
        TimeForm::Cursor | TimeForm::Start => 0,
        TimeForm::End | TimeForm::SelectionIn | TimeForm::SelectionOut => 0,
    }
}

fn static_op_name(name: &str) -> &'static str {
    // Names that can appear in DSL but the parser doesn't yet build into
    // typed Ops. We can't return the dynamic str, so map a few we know about
    // and fall back to a generic label.
    match name {
        "eq" => "Eq",
        "compress" => "Compress",
        "limit" => "Limit",
        "reverb" => "Reverb",
        "delay" => "Delay",
        "noise_reduce" => "NoiseReduce",
        "spectral" => "SpectralEdit",
        _ => "op",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_project_round_trips() {
        let p = Project::new(96_000);
        let dsl = super::super::emit::project_to_dsl(&p).unwrap();
        let parsed = parse_project(&dsl).unwrap();
        assert_eq!(parsed.format_version, p.format_version);
        assert_eq!(parsed.sample_rate(), p.sample_rate());
        assert!(parsed.sources.is_empty());
        assert!(parsed.tracks.is_empty());
    }

    #[test]
    fn project_with_metadata_round_trips() {
        let mut p = Project::new(48_000);
        p.metadata.name = "Session 1".into();
        p.metadata.created_at = Some("2026-05-03T12:00:00Z".into());
        p.metadata.modified_at = Some("2026-05-03T13:00:00Z".into());
        p.transport.playhead = 48_000;
        p.transport.looping = true;
        p.view.zoom = 2.0;
        p.master.gain_db = -3.0;
        p.markers.push(Marker {
            name: "A".into(),
            time: 0,
        });
        p.markers.push(Marker {
            name: "B".into(),
            time: 48_000,
        });

        let dsl = super::super::emit::project_to_dsl(&p).unwrap();
        let parsed = parse_project(&dsl).unwrap();
        assert_eq!(parsed.metadata.name, "Session 1");
        assert_eq!(parsed.transport.playhead, 48_000);
        assert!(parsed.transport.looping);
        assert!((parsed.view.zoom - 2.0).abs() < 1e-6);
        assert!((parsed.master.gain_db - -3.0).abs() < 1e-3);
        assert_eq!(parsed.markers.len(), 2);
        assert_eq!(parsed.markers[1].time, 48_000);
    }

    #[test]
    fn source_with_destructive_ops_round_trips() {
        use crate::edit_list::EditList;
        use crate::ids::SourceId;
        use crate::source::{Source, StoragePath, Timestamp};

        let mut p = Project::new(96_000);
        let mut s = Source::new(
            SourceId::new("src_a"),
            "vocals.wav",
            1,
            96_000,
            StoragePath::new("sources/src_a/base.f32"),
            96_000 * 5,
            Timestamp("2026-05-03T12:00:00Z".into()),
        );
        s.edits = EditList::new();
        s.edits.apply(Op::Silence {
            range: SampleRange::new(96_000, 96_000 * 2).unwrap(),
        });
        s.edits.apply(Op::Gain {
            range: SampleRange::new(96_000 * 3, 96_000 * 4).unwrap(),
            db: -3.0,
        });
        p.sources.insert(s.id.clone(), s);

        let dsl = super::super::emit::project_to_dsl(&p).unwrap();
        let parsed = parse_project(&dsl).unwrap();
        let parsed_source = parsed.sources.get(&SourceId::new("src_a")).unwrap();
        assert_eq!(parsed_source.edits.len(), 2);
        // Spot-check the second op.
        let ops: Vec<&Op> = parsed_source.edits.active().collect();
        assert!(matches!(ops[1], Op::Gain { db, .. } if (*db - -3.0).abs() < 1e-3));
    }

    #[test]
    fn track_with_clip_round_trips() {
        use crate::ids::SourceId;
        use crate::project::{Clip, Fade, Track};
        use crate::source::{Source, StoragePath, Timestamp};

        let mut p = Project::new(96_000);
        let s = Source::new(
            SourceId::new("src_a"),
            "vocals.wav",
            1,
            96_000,
            StoragePath::new("sources/src_a/base.f32"),
            96_000 * 10,
            Timestamp("2026-05-03T12:00:00Z".into()),
        );
        let src_id = s.id.clone();
        p.sources.insert(src_id.clone(), s);
        p.tracks.push(Track {
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
        });

        let dsl = super::super::emit::project_to_dsl(&p).unwrap();
        let parsed = parse_project(&dsl).unwrap();
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.tracks[0].name, "Vocal");
        let clip = &parsed.tracks[0].clips[0];
        assert_eq!(clip.name, "Take 1");
        assert_eq!(clip.source_out, 96_000 * 4);
        assert_eq!(clip.fade_in.duration_samples, 96 * 50);
    }

    #[test]
    fn rejects_unsupported_format_version() {
        let bad = "project \"\" { format_version: 999 sample_rate: 96000 }";
        let err = parse_project(bad).unwrap_err();
        assert!(matches!(err, ParseError::Unexpected { .. }));
    }

    #[test]
    fn unsupported_features_surface_with_a_clear_error() {
        // EQ insert on a track is something the emitter writes, but the
        // parser explicitly rejects it for now.
        let dsl = "project \"\" {
            format_version: 1
            sample_rate: 96000
            tracks {
                track \"T\" {
                    inserts {}
                }
            }
            master { gain: 0dB }
            transport { playhead: @00:00:00.000 loop: false }
            view { zoom: 1 scroll: @00:00:00.000 active_view: waveform }
        }";
        let err = parse_project(dsl).unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)));
    }
}
