//! Project: top-level multitrack composition.
//!
//! Tracks the structure described in `03-data-model.md` §"Multitrack project".
//! Effects, clips, and envelopes hang off tracks. Sources live in their own
//! registry so multiple clips can reference the same source, and so "Make
//! Unique" can clone one cleanly.

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::effect::EffectKind;
use crate::envelope::{AutomationLane, ClipEnvelope};
use crate::ids::{ClipId, EffectInstanceId, GroupId, ProfileId, SourceId, TrackId};
use crate::op::FadeShape;
use crate::range::SampleRange;
use crate::source::Source;

/// Default project sample rate per `01-feature-spec.md`.
pub const DEFAULT_SAMPLE_RATE: u32 = 96_000;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectMetadata {
    pub name: String,
    pub created_at: Option<String>,
    pub modified_at: Option<String>,
}

#[derive(Copy, Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct TransportState {
    pub playhead: u64,
    pub looping: bool,
    pub loop_range: Option<SampleRange>,
}

#[derive(Copy, Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub enum ActiveView {
    #[default]
    Waveform,
    Spectral,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewState {
    pub zoom: f32,
    pub scroll_samples: u64,
    pub active_view: ActiveView,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            scroll_samples: 0,
            active_view: ActiveView::Waveform,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub name: String,
    pub time: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoiseProfile {
    pub id: ProfileId,
    pub name: String,
    pub captured_from: SourceId,
    pub range: SampleRange,
    pub magnitudes: Vec<f32>,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Fade {
    pub duration_samples: u64,
    pub shape: FadeShape,
}

impl Fade {
    pub fn none() -> Self {
        Self {
            duration_samples: 0,
            shape: FadeShape::Linear,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectInstance {
    pub id: EffectInstanceId,
    pub kind: EffectKind,
    pub bypass: bool,
    pub params: BTreeMap<String, f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: ClipId,
    pub source_id: SourceId,
    pub name: String,
    pub track_position: SampleRange,
    pub source_in: u64,
    pub source_out: u64,
    pub gain_db: f32,
    pub pan: f32,
    pub fade_in: Fade,
    pub fade_out: Fade,
    pub time_stretch: f32,
    pub pitch_shift_cents: f32,
    pub envelopes: Vec<ClipEnvelope>,
    pub locked: bool,
    pub group: Option<GroupId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub name: String,
    pub height: f32,
    pub mute: bool,
    pub solo: bool,
    pub arm: bool,
    pub gain_db: f32,
    pub pan: f32,
    pub inserts: Vec<EffectInstance>,
    pub automation: Vec<AutomationLane>,
    pub clips: Vec<Clip>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MasterBus {
    pub gain_db: f32,
    pub inserts: Vec<EffectInstance>,
}

impl Default for MasterBus {
    fn default() -> Self {
        Self {
            gain_db: 0.0,
            inserts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    /// JSON `format_version`. Pinned at construction; migration runs at load.
    pub format_version: u32,
    pub metadata: ProjectMetadata,
    /// Project-internal sample rate. Doc 03 invariant #7: never changes after
    /// creation. Field is private and only set by `Project::new`.
    sample_rate: u32,
    pub sources: BTreeMap<SourceId, Source>,
    pub tracks: Vec<Track>,
    pub master: MasterBus,
    pub markers: Vec<Marker>,
    pub transport: TransportState,
    pub view: ViewState,
    pub noise_profiles: BTreeMap<ProfileId, NoiseProfile>,
}

impl Project {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            format_version: crate::FORMAT_VERSION,
            metadata: ProjectMetadata::default(),
            sample_rate,
            sources: BTreeMap::new(),
            tracks: Vec::new(),
            master: MasterBus::default(),
            markers: Vec::new(),
            transport: TransportState::default(),
            view: ViewState::default(),
            noise_profiles: BTreeMap::new(),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Pretty-print the project as JSON. Doc 03 §Persistence: the JSON is the
    /// canonical form; sample data lives in the storage layer and is
    /// referenced by path.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse a JSON project. Rejects unknown `format_version`s before
    /// attempting full deserialisation, so the user gets a clean message
    /// rather than an arbitrary serde error.
    pub fn from_json(s: &str) -> Result<Self, ProjectLoadError> {
        // Peek at format_version first; deserialising into Value is cheap and
        // gives us a clean failure mode for old/new files.
        let head: serde_json::Value =
            serde_json::from_str(s).map_err(ProjectLoadError::Parse)?;
        let v = head
            .get("format_version")
            .and_then(|v| v.as_u64())
            .ok_or(ProjectLoadError::MissingFormatVersion)?;
        if v != crate::FORMAT_VERSION as u64 {
            return Err(ProjectLoadError::UnsupportedFormatVersion {
                found: v,
                supported: crate::FORMAT_VERSION,
            });
        }
        serde_json::from_value(head).map_err(ProjectLoadError::Parse)
    }

    /// Find a track by id without exposing internal storage.
    pub fn track(&self, id: TrackId) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn track_mut(&mut self, id: TrackId) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Walk every clip referenced by every track. Used for invariant
    /// validation and for the future delete-source check.
    pub fn clips(&self) -> impl Iterator<Item = (&Track, &Clip)> {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter().map(move |c| (t, c)))
    }

    /// Doc 03 §"Make Unique": clone the source for one clip's exclusive use.
    /// Subsequent destructive edits to either source no longer affect the other.
    /// Returns the new source ID, or `None` if the clip or source can't be found.
    pub fn make_unique(
        &mut self,
        clip_id: ClipId,
        new_source_id: SourceId,
    ) -> Option<SourceId> {
        let track = self
            .tracks
            .iter_mut()
            .find(|t| t.clips.iter().any(|c| c.id == clip_id))?;
        let clip = track.clips.iter_mut().find(|c| c.id == clip_id)?;
        let original = self.sources.get(&clip.source_id)?.clone();
        let mut cloned = original;
        cloned.id = new_source_id.clone();
        self.sources.insert(new_source_id.clone(), cloned);
        clip.source_id = new_source_id.clone();
        Some(new_source_id)
    }

    /// Cross-cutting invariant check. Construction-time invariants are
    /// enforced in their own constructors (e.g. breakpoint ordering); this
    /// method picks up the things that need the whole project to verify.
    pub fn validate(&self) -> Result<(), ProjectInvariantError> {
        for (track, clip) in self.clips() {
            // Doc 03 invariant #2: clip.source_in <= clip.source_out, both within source.
            if clip.source_in > clip.source_out {
                return Err(ProjectInvariantError::ClipSourceRangeInverted {
                    track: track.id,
                    clip: clip.id,
                });
            }
            let Some(source) = self.sources.get(&clip.source_id) else {
                return Err(ProjectInvariantError::ClipReferencesMissingSource {
                    track: track.id,
                    clip: clip.id,
                    source: clip.source_id.clone(),
                });
            };
            // base_length is the *minimum* possible length; the edit list can
            // grow it, but we never shrink to less than the smaller of the two.
            // For now we treat base_length as the upper bound; once query_samples
            // can return a real length, validate against that.
            if clip.source_out > source.base_length {
                return Err(ProjectInvariantError::ClipExceedsSource {
                    track: track.id,
                    clip: clip.id,
                    source: clip.source_id.clone(),
                    source_out: clip.source_out,
                    base_length: source.base_length,
                });
            }
            // Doc 03 invariant #3: track_position length matches source span * stretch.
            let source_span = clip.source_out - clip.source_in;
            let expected = (source_span as f64 * clip.time_stretch as f64).round() as u64;
            if clip.track_position.len() != expected {
                return Err(ProjectInvariantError::ClipDurationMismatch {
                    track: track.id,
                    clip: clip.id,
                    track_len: clip.track_position.len(),
                    expected,
                });
            }
            // Doc 03 invariant #5: fades non-negative and within clip duration.
            let clip_len = clip.track_position.len();
            if clip.fade_in.duration_samples + clip.fade_out.duration_samples > clip_len {
                return Err(ProjectInvariantError::FadesExceedClip {
                    track: track.id,
                    clip: clip.id,
                });
            }
        }

        // Doc 03 invariant #6: effect instance IDs unique within a track.
        for track in &self.tracks {
            let mut seen = HashSet::new();
            for insert in &track.inserts {
                if !seen.insert(insert.id) {
                    return Err(ProjectInvariantError::DuplicateEffectInstance {
                        track: track.id,
                        effect: insert.id,
                    });
                }
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub enum ProjectLoadError {
    Parse(serde_json::Error),
    MissingFormatVersion,
    UnsupportedFormatVersion { found: u64, supported: u32 },
}

impl fmt::Display for ProjectLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "json parse error: {e}"),
            Self::MissingFormatVersion => {
                write!(f, "missing `format_version` at the top level")
            }
            Self::UnsupportedFormatVersion { found, supported } => write!(
                f,
                "unsupported format_version {found} (this build supports {supported})"
            ),
        }
    }
}

impl std::error::Error for ProjectLoadError {}

#[derive(Debug, PartialEq)]
pub enum ProjectInvariantError {
    ClipSourceRangeInverted {
        track: TrackId,
        clip: ClipId,
    },
    ClipReferencesMissingSource {
        track: TrackId,
        clip: ClipId,
        source: SourceId,
    },
    ClipExceedsSource {
        track: TrackId,
        clip: ClipId,
        source: SourceId,
        source_out: u64,
        base_length: u64,
    },
    ClipDurationMismatch {
        track: TrackId,
        clip: ClipId,
        track_len: u64,
        expected: u64,
    },
    FadesExceedClip {
        track: TrackId,
        clip: ClipId,
    },
    DuplicateEffectInstance {
        track: TrackId,
        effect: EffectInstanceId,
    },
}

impl fmt::Display for ProjectInvariantError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClipSourceRangeInverted { track, clip } => write!(
                f,
                "{track}/{clip}: source_in > source_out"
            ),
            Self::ClipReferencesMissingSource { track, clip, source } => write!(
                f,
                "{track}/{clip} references missing source {source}"
            ),
            Self::ClipExceedsSource {
                track,
                clip,
                source,
                source_out,
                base_length,
            } => write!(
                f,
                "{track}/{clip}: source_out {source_out} > {source}.base_length {base_length}"
            ),
            Self::ClipDurationMismatch {
                track,
                clip,
                track_len,
                expected,
            } => write!(
                f,
                "{track}/{clip}: track_position.len {track_len} != expected {expected}"
            ),
            Self::FadesExceedClip { track, clip } => {
                write!(f, "{track}/{clip}: fade_in + fade_out exceeds clip length")
            }
            Self::DuplicateEffectInstance { track, effect } => {
                write!(f, "{track}: duplicate effect instance {effect}")
            }
        }
    }
}

impl std::error::Error for ProjectInvariantError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{StoragePath, Timestamp};

    fn fixture_source(id: &str, base_length: u64) -> Source {
        Source::new(
            SourceId::new(id),
            "test.wav",
            1,
            96_000,
            StoragePath::new(format!("sources/{id}/base.f32")),
            base_length,
            Timestamp("2026-05-03T12:00:00Z".into()),
        )
    }

    fn fixture_track(id: TrackId, name: &str) -> Track {
        Track {
            id,
            name: name.into(),
            height: 80.0,
            mute: false,
            solo: false,
            arm: false,
            gain_db: 0.0,
            pan: 0.0,
            inserts: Vec::new(),
            automation: Vec::new(),
            clips: Vec::new(),
        }
    }

    fn fixture_clip(id: ClipId, source_id: SourceId, len: u64) -> Clip {
        Clip {
            id,
            source_id,
            name: "clip".into(),
            track_position: SampleRange::new(0, len).unwrap(),
            source_in: 0,
            source_out: len,
            gain_db: 0.0,
            pan: 0.0,
            fade_in: Fade::none(),
            fade_out: Fade::none(),
            time_stretch: 1.0,
            pitch_shift_cents: 0.0,
            envelopes: Vec::new(),
            locked: false,
            group: None,
        }
    }

    #[test]
    fn empty_project_validates() {
        let p = Project::new(96_000);
        assert!(p.validate().is_ok());
        assert_eq!(p.sample_rate(), 96_000);
    }

    #[test]
    fn well_formed_project_validates() {
        let mut p = Project::new(96_000);
        let src = fixture_source("src_a", 1000);
        let src_id = src.id.clone();
        p.sources.insert(src_id.clone(), src);

        let mut track = fixture_track(TrackId(1), "Vocal");
        track.clips.push(fixture_clip(ClipId(1), src_id, 1000));
        p.tracks.push(track);

        p.validate().unwrap();
    }

    #[test]
    fn detects_clip_exceeding_source() {
        let mut p = Project::new(96_000);
        let src = fixture_source("src_a", 100);
        let src_id = src.id.clone();
        p.sources.insert(src_id.clone(), src);

        let mut track = fixture_track(TrackId(1), "T");
        track.clips.push(fixture_clip(ClipId(1), src_id, 200));
        p.tracks.push(track);

        let err = p.validate().unwrap_err();
        assert!(matches!(err, ProjectInvariantError::ClipExceedsSource { .. }));
    }

    #[test]
    fn detects_track_position_not_matching_stretch() {
        let mut p = Project::new(96_000);
        let src = fixture_source("src_a", 1000);
        let src_id = src.id.clone();
        p.sources.insert(src_id.clone(), src);

        let mut track = fixture_track(TrackId(1), "T");
        let mut clip = fixture_clip(ClipId(1), src_id, 1000);
        clip.time_stretch = 2.0; // expect track_position.len == 2000
        track.clips.push(clip);
        p.tracks.push(track);

        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            ProjectInvariantError::ClipDurationMismatch { .. }
        ));
    }

    #[test]
    fn detects_fades_exceeding_clip() {
        let mut p = Project::new(96_000);
        let src = fixture_source("src_a", 1000);
        let src_id = src.id.clone();
        p.sources.insert(src_id.clone(), src);

        let mut track = fixture_track(TrackId(1), "T");
        let mut clip = fixture_clip(ClipId(1), src_id, 100);
        clip.fade_in = Fade {
            duration_samples: 80,
            shape: FadeShape::Linear,
        };
        clip.fade_out = Fade {
            duration_samples: 80,
            shape: FadeShape::Linear,
        };
        track.clips.push(clip);
        p.tracks.push(track);

        let err = p.validate().unwrap_err();
        assert!(matches!(err, ProjectInvariantError::FadesExceedClip { .. }));
    }

    #[test]
    fn detects_duplicate_effect_instance_id() {
        let mut p = Project::new(96_000);
        let mut track = fixture_track(TrackId(1), "T");
        track.inserts.push(EffectInstance {
            id: EffectInstanceId(1),
            kind: EffectKind::Eq,
            bypass: false,
            params: BTreeMap::new(),
        });
        track.inserts.push(EffectInstance {
            id: EffectInstanceId(1),
            kind: EffectKind::Compressor,
            bypass: false,
            params: BTreeMap::new(),
        });
        p.tracks.push(track);

        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            ProjectInvariantError::DuplicateEffectInstance { .. }
        ));
    }

    #[test]
    fn detects_missing_source_reference() {
        let mut p = Project::new(96_000);
        let mut track = fixture_track(TrackId(1), "T");
        track
            .clips
            .push(fixture_clip(ClipId(1), SourceId::new("src_missing"), 100));
        p.tracks.push(track);

        let err = p.validate().unwrap_err();
        assert!(matches!(
            err,
            ProjectInvariantError::ClipReferencesMissingSource { .. }
        ));
    }

    #[test]
    fn empty_project_round_trips_through_json() {
        let p = Project::new(96_000);
        let s = p.to_json().unwrap();
        let p2 = Project::from_json(&s).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn populated_project_round_trips_through_json() {
        use crate::edit_list::EditList;
        use crate::ids::EffectInstanceId;
        use crate::op::Op;

        let mut p = Project::new(96_000);
        p.metadata.name = "Session 1".into();
        let mut src = fixture_source("src_a", 10_000);
        src.edits = EditList::new();
        src.edits.apply(Op::Silence {
            range: SampleRange::new(100, 200).unwrap(),
        });
        src.edits.apply(Op::Gain {
            range: SampleRange::new(300, 500).unwrap(),
            db: -3.0,
        });
        let src_id = src.id.clone();
        p.sources.insert(src_id.clone(), src);

        let mut track = fixture_track(TrackId(1), "Vocal");
        track.inserts.push(EffectInstance {
            id: EffectInstanceId(1),
            kind: EffectKind::Eq,
            bypass: false,
            params: BTreeMap::new(),
        });
        track.clips.push(fixture_clip(ClipId(1), src_id, 1000));
        p.tracks.push(track);
        p.markers.push(Marker {
            name: "Verse 1".into(),
            time: 0,
        });

        let s = p.to_json().unwrap();
        let p2 = Project::from_json(&s).unwrap();
        assert_eq!(p, p2);
    }

    #[test]
    fn rejects_missing_format_version() {
        let err = Project::from_json("{}").unwrap_err();
        assert!(matches!(err, ProjectLoadError::MissingFormatVersion));
    }

    #[test]
    fn rejects_unsupported_format_version() {
        let err = Project::from_json(r#"{"format_version": 999}"#).unwrap_err();
        assert!(matches!(
            err,
            ProjectLoadError::UnsupportedFormatVersion { found: 999, supported: 1 }
        ));
    }

    #[test]
    fn rejects_invalid_breakpoint_ordering_in_json() {
        // Embed a clip envelope with out-of-order breakpoints; the BreakpointSeq
        // try_from validator should reject it during deserialisation.
        let bad = r#"{
            "format_version": 1,
            "metadata": {"name": "", "created_at": null, "modified_at": null},
            "sample_rate": 96000,
            "sources": {},
            "tracks": [{
                "id": 1, "name": "T", "height": 80.0, "mute": false, "solo": false,
                "arm": false, "gain_db": 0.0, "pan": 0.0, "inserts": [],
                "automation": [{
                    "parameter": "track.gain",
                    "breakpoints": [
                        {"time": 100, "value": 0.0, "curve": "Linear"},
                        {"time": 50,  "value": 0.0, "curve": "Linear"}
                    ]
                }],
                "clips": []
            }],
            "master": {"gain_db": 0.0, "inserts": []},
            "markers": [],
            "transport": {"playhead": 0, "looping": false, "loop_range": null},
            "view": {"zoom": 1.0, "scroll_samples": 0, "active_view": "Waveform"},
            "noise_profiles": {}
        }"#;
        let err = Project::from_json(bad).unwrap_err();
        assert!(matches!(err, ProjectLoadError::Parse(_)));
    }

    #[test]
    fn make_unique_assigns_new_source_id_to_clip() {
        let mut p = Project::new(96_000);
        let src = fixture_source("src_a", 1000);
        let src_a = src.id.clone();
        p.sources.insert(src_a.clone(), src);

        let mut track = fixture_track(TrackId(1), "T");
        track.clips.push(fixture_clip(ClipId(1), src_a.clone(), 1000));
        track.clips.push(fixture_clip(ClipId(2), src_a.clone(), 1000));
        p.tracks.push(track);

        let new_id = SourceId::new("src_b");
        let returned = p.make_unique(ClipId(1), new_id.clone()).unwrap();
        assert_eq!(returned, new_id);
        assert_eq!(p.tracks[0].clips[0].source_id, new_id);
        assert_eq!(p.tracks[0].clips[1].source_id, src_a);
        assert!(p.sources.contains_key(&new_id));
        assert!(p.sources.contains_key(&src_a));
    }
}
