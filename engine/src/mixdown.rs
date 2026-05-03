//! Offline multitrack mixdown.
//!
//! Per `03-data-model.md` §"Clip semantics at playback", the renderer walks
//! every clip on every track, applies clip parameters and fades, sums into
//! a per-track stereo bus, applies track gain and pan, and sums into the
//! master. Master gain produces the final stereo output.
//!
//! This first slice deliberately leaves out time-stretch, pitch-shift,
//! per-clip envelopes, track inserts, track automation, and master inserts.
//! Each of those returns [`MixdownError::Unsupported`] so the caller knows
//! exactly which feature blocked the render.

use crate::engine::{Engine, QueryError};
use crate::ids::{ClipId, SourceId};
use crate::op::{FadeDirection, FadeShape};
use crate::project::{Clip, Fade, Project, Track};

#[derive(Debug)]
pub enum MixdownError {
    Query(QueryError),
    Unsupported(&'static str),
    SampleRateMismatch {
        clip: ClipId,
        source: SourceId,
        source_rate: u32,
        project_rate: u32,
    },
    ClipBeyondSource {
        clip: ClipId,
        source: SourceId,
        source_out: u64,
        effective: u64,
    },
}

impl std::fmt::Display for MixdownError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Query(e) => write!(f, "{e}"),
            Self::Unsupported(k) => write!(f, "mixdown: feature `{k}` not yet supported"),
            Self::SampleRateMismatch {
                clip,
                source,
                source_rate,
                project_rate,
            } => write!(
                f,
                "{clip} references {source} at {source_rate} Hz but project is {project_rate} Hz \
                 (resampling not yet implemented)"
            ),
            Self::ClipBeyondSource {
                clip,
                source,
                source_out,
                effective,
            } => write!(
                f,
                "{clip}: source_out {source_out} exceeds {source}'s effective length {effective}"
            ),
        }
    }
}

impl std::error::Error for MixdownError {}

impl From<QueryError> for MixdownError {
    fn from(e: QueryError) -> Self {
        Self::Query(e)
    }
}

/// Render the project to interleaved stereo at the project's sample rate.
/// Output length is the latest clip end position across every track. A
/// project with no clips renders to an empty buffer.
pub fn mixdown_stereo(engine: &Engine) -> Result<Vec<f32>, MixdownError> {
    let project = engine.project();
    check_unsupported(project)?;

    let total_frames = project_length_frames(project);
    let mut master = vec![0.0_f32; (total_frames * 2) as usize];

    for track in &project.tracks {
        if track.mute {
            continue;
        }
        let track_buf = render_track(engine, track, total_frames)?;
        // Track pan on an already-stereo bus uses balance (pan=0 unchanged,
        // pan=-1 zeros R, pan=+1 zeros L). The equal-power law is reserved
        // for placing a mono source into the stereo field at the clip stage.
        let track_gain = db_to_linear(track.gain_db);
        let (l_w, r_w) = stereo_balance(track.pan);
        for f in 0..total_frames as usize {
            let l = track_buf[f * 2] * track_gain * l_w;
            let r = track_buf[f * 2 + 1] * track_gain * r_w;
            master[f * 2] += l;
            master[f * 2 + 1] += r;
        }
    }

    let mg = db_to_linear(project.master.gain_db);
    if mg != 1.0 {
        for s in &mut master {
            *s *= mg;
        }
    }

    Ok(master)
}

fn check_unsupported(project: &Project) -> Result<(), MixdownError> {
    if !project.master.inserts.is_empty() {
        return Err(MixdownError::Unsupported("master inserts"));
    }
    for track in &project.tracks {
        if !track.inserts.is_empty() {
            return Err(MixdownError::Unsupported("track inserts"));
        }
        if !track.automation.is_empty() {
            return Err(MixdownError::Unsupported("track automation"));
        }
        for clip in &track.clips {
            if clip.time_stretch != 1.0 {
                return Err(MixdownError::Unsupported("clip time_stretch"));
            }
            if clip.pitch_shift_cents != 0.0 {
                return Err(MixdownError::Unsupported("clip pitch_shift"));
            }
            if !clip.envelopes.is_empty() {
                return Err(MixdownError::Unsupported("clip envelopes"));
            }
        }
    }
    Ok(())
}

fn project_length_frames(project: &Project) -> u64 {
    let mut end = 0_u64;
    for track in &project.tracks {
        for clip in &track.clips {
            end = end.max(clip.track_position.end());
        }
    }
    end
}

fn render_track(
    engine: &Engine,
    track: &Track,
    total_frames: u64,
) -> Result<Vec<f32>, MixdownError> {
    let mut buf = vec![0.0_f32; (total_frames * 2) as usize];
    for clip in &track.clips {
        render_clip_into(engine, clip, &mut buf, total_frames)?;
    }
    Ok(buf)
}

fn render_clip_into(
    engine: &Engine,
    clip: &Clip,
    track_buf: &mut [f32],
    total_frames: u64,
) -> Result<(), MixdownError> {
    let project = engine.project();
    let source = project
        .sources
        .get(&clip.source_id)
        .ok_or_else(|| MixdownError::Query(QueryError::UnknownSource(clip.source_id.clone())))?;

    if source.sample_rate != project.sample_rate() {
        return Err(MixdownError::SampleRateMismatch {
            clip: clip.id,
            source: clip.source_id.clone(),
            source_rate: source.sample_rate,
            project_rate: project.sample_rate(),
        });
    }

    let effective = engine.effective_frame_count(&clip.source_id)?;
    if clip.source_out > effective {
        return Err(MixdownError::ClipBeyondSource {
            clip: clip.id,
            source: clip.source_id.clone(),
            source_out: clip.source_out,
            effective,
        });
    }

    let clip_range =
        crate::range::SampleRange::new(clip.source_in, clip.source_out).expect("validated");
    let samples = engine.query_samples(&clip.source_id, clip_range)?;
    let frames = (clip.source_out - clip.source_in) as usize;
    let ch = source.channel_count as usize;

    let clip_gain = db_to_linear(clip.gain_db);
    // Mono sources get equal-power-panned into the stereo field (pan=0
    // splits the sample at -3 dB into L and R). Stereo sources are
    // already-stereo: we apply pan as a balance so the centre is unity.
    let (l_w, r_w) = match ch {
        1 => mono_pan(clip.pan),
        2 => stereo_balance(clip.pan),
        other => return Err(MixdownError::Unsupported(static_chan_name(other))),
    };

    let track_pos_start = clip.track_position.start();
    let max_frame = total_frames.min(track_pos_start + frames as u64);

    for (i, frame_idx) in (track_pos_start..max_frame).enumerate() {
        let fade_g = fade_envelope(i, frames, clip.fade_in, clip.fade_out);
        let g = clip_gain * fade_g;
        let (left, right) = match ch {
            1 => {
                let s = samples[i] * g;
                (s * l_w, s * r_w)
            }
            2 => {
                let l = samples[i * 2] * g * l_w;
                let r = samples[i * 2 + 1] * g * r_w;
                (l, r)
            }
            _ => unreachable!("checked above"),
        };
        let dest = frame_idx as usize * 2;
        track_buf[dest] += left;
        track_buf[dest + 1] += right;
    }
    Ok(())
}

fn fade_envelope(frame: usize, total: usize, fade_in: Fade, fade_out: Fade) -> f32 {
    let mut g = 1.0_f32;
    let fi = fade_in.duration_samples as usize;
    if fi > 0 && frame < fi {
        let t = frame as f32 / (fi.max(1) as f32);
        g *= shape_curve(t.clamp(0.0, 1.0), fade_in.shape, FadeDirection::In);
    }
    let fo = fade_out.duration_samples as usize;
    if fo > 0 && frame >= total.saturating_sub(fo) {
        let into = (frame - total.saturating_sub(fo)) as f32 / (fo.max(1) as f32);
        g *= shape_curve(into.clamp(0.0, 1.0), fade_out.shape, FadeDirection::Out);
    }
    g
}

fn shape_curve(t: f32, shape: FadeShape, direction: FadeDirection) -> f32 {
    let p = match direction {
        FadeDirection::In => t,
        FadeDirection::Out => 1.0 - t,
    };
    match shape {
        FadeShape::Linear => p,
        FadeShape::Logarithmic => p.sqrt(),
        FadeShape::Exponential => p * p,
        FadeShape::SCurve => p * p * (3.0 - 2.0 * p),
    }
}

/// Equal-power pan law for placing a mono source into the stereo field.
/// pan = -1 routes to L only, +1 to R only, 0 splits at cos(π/4).
fn mono_pan(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    let theta = (p + 1.0) * 0.25 * std::f32::consts::PI;
    (theta.cos(), theta.sin())
}

/// Balance law for an already-stereo signal: pan = 0 leaves both channels at
/// unity. Negative pan attenuates R linearly to zero at -1; positive pan
/// attenuates L to zero at +1.
fn stereo_balance(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    let l = 1.0 - p.max(0.0);
    let r = 1.0 + p.min(0.0);
    (l, r)
}

fn db_to_linear(db: f32) -> f32 {
    if db == 0.0 {
        return 1.0;
    }
    10.0_f32.powf(db / 20.0)
}

// Borrow-checker workaround: MixdownError variants take a `'static str`,
// so we materialise the channel-count bucket here.
fn static_chan_name(n: usize) -> &'static str {
    match n {
        0 => "0-channel source",
        3 => "3-channel source",
        4 => "4-channel source",
        _ => "multichannel source",
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Engine;
    use crate::ids::{ClipId, SourceId, TrackId};
    use crate::project::{Clip, Fade, Track};
    use crate::range::SampleRange;
    use crate::source::Timestamp;
    use crate::wav;

    fn synth_mono_wav(samples: &[f32], rate: u32) -> Vec<u8> {
        wav::encode_f32(samples, 1, rate)
    }

    fn now() -> Timestamp {
        Timestamp("2026-05-03T12:00:00Z".into())
    }

    fn empty_track(id: u64, name: &str) -> Track {
        Track {
            id: TrackId(id),
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

    fn clip_for(id: u64, source: SourceId, at: u64, len: u64) -> Clip {
        Clip {
            id: ClipId(id),
            source_id: source,
            name: "clip".into(),
            track_position: SampleRange::new(at, at + len).unwrap(),
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
    fn empty_project_renders_to_empty_buffer() {
        let engine = Engine::new(48_000);
        let out = mixdown_stereo(&engine).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn single_mono_clip_centred_lands_equally_on_l_and_r() {
        let mut engine = Engine::new(48_000);
        let bytes = synth_mono_wav(&[0.5_f32; 100], 48_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();

        let mut track = empty_track(1, "T");
        track.clips.push(clip_for(1, id.clone(), 0, 100));
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        assert_eq!(out.len(), 200);
        // For an equal-power centre pan, L and R both get sample × cos(π/4)
        // ≈ 0.5 × 0.707 ≈ 0.354.
        assert!((out[0] - 0.5 * (std::f32::consts::FRAC_1_SQRT_2)).abs() < 1e-3);
        assert!((out[1] - 0.5 * (std::f32::consts::FRAC_1_SQRT_2)).abs() < 1e-3);
    }

    #[test]
    fn hard_left_pan_zeros_the_right_channel() {
        let mut engine = Engine::new(48_000);
        let bytes = synth_mono_wav(&[0.5_f32; 32], 48_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();

        let mut track = empty_track(1, "T");
        let mut clip = clip_for(1, id, 0, 32);
        clip.pan = -1.0;
        track.clips.push(clip);
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        for f in 0..32 {
            assert!(out[f * 2] > 0.49);
            assert!(out[f * 2 + 1].abs() < 1e-6);
        }
    }

    #[test]
    fn two_clips_on_separate_tracks_sum_into_master() {
        let mut engine = Engine::new(48_000);
        let a = engine
            .import_wav("a.wav", &synth_mono_wav(&[0.25_f32; 8], 48_000), now())
            .unwrap();
        let b = engine
            .import_wav("b.wav", &synth_mono_wav(&[0.25_f32; 8], 48_000), now())
            .unwrap();

        let mut t1 = empty_track(1, "A");
        t1.clips.push(clip_for(1, a, 0, 8));
        let mut t2 = empty_track(2, "B");
        t2.clips.push(clip_for(2, b, 0, 8));
        engine.project_mut().tracks.push(t1);
        engine.project_mut().tracks.push(t2);

        let out = mixdown_stereo(&engine).unwrap();
        // Each centre-panned clip contributes 0.25 × cos(π/4) ≈ 0.177 to L
        // and the same to R. Two clips sum to ~0.354 per channel.
        let expected = 0.25 * std::f32::consts::FRAC_1_SQRT_2 * 2.0;
        assert!((out[0] - expected).abs() < 1e-3, "L got {}", out[0]);
        assert!((out[1] - expected).abs() < 1e-3, "R got {}", out[1]);
    }

    #[test]
    fn fade_in_silences_the_first_frame() {
        let mut engine = Engine::new(48_000);
        let bytes = synth_mono_wav(&[1.0_f32; 16], 48_000);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();

        let mut t = empty_track(1, "T");
        let mut clip = clip_for(1, id, 0, 16);
        clip.fade_in = Fade {
            duration_samples: 8,
            shape: FadeShape::Linear,
        };
        t.clips.push(clip);
        engine.project_mut().tracks.push(t);

        let out = mixdown_stereo(&engine).unwrap();
        assert!(out[0].abs() < 1e-6);
        // After the fade ends, full unity-times-pan-law amplitude.
        let expected_unity = std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[8 * 2] - expected_unity).abs() < 1e-3);
    }

    #[test]
    fn muted_track_contributes_nothing() {
        let mut engine = Engine::new(48_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[1.0_f32; 8], 48_000), now())
            .unwrap();

        let mut t = empty_track(1, "T");
        t.mute = true;
        t.clips.push(clip_for(1, id, 0, 8));
        engine.project_mut().tracks.push(t);

        let out = mixdown_stereo(&engine).unwrap();
        for s in &out {
            assert!(s.abs() < 1e-6);
        }
    }

    #[test]
    fn rejects_clip_at_mismatched_sample_rate() {
        // Project at 96k, source at 48k.
        let mut engine = Engine::new(96_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[0.5_f32; 8], 48_000), now())
            .unwrap();

        let mut t = empty_track(1, "T");
        t.clips.push(clip_for(1, id, 0, 8));
        engine.project_mut().tracks.push(t);

        let err = mixdown_stereo(&engine).unwrap_err();
        assert!(matches!(err, MixdownError::SampleRateMismatch { .. }));
    }

    #[test]
    fn rejects_unsupported_track_inserts() {
        use crate::effect::EffectKind;
        use crate::project::EffectInstance;
        use std::collections::BTreeMap;

        let mut engine = Engine::new(48_000);
        let mut t = empty_track(1, "T");
        t.inserts.push(EffectInstance {
            id: crate::ids::EffectInstanceId(1),
            kind: EffectKind::Eq,
            bypass: false,
            params: BTreeMap::new(),
        });
        engine.project_mut().tracks.push(t);
        let err = mixdown_stereo(&engine).unwrap_err();
        assert!(matches!(err, MixdownError::Unsupported("track inserts")));
    }

    #[test]
    fn master_gain_attenuates_output() {
        let mut engine = Engine::new(48_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[1.0_f32; 8], 48_000), now())
            .unwrap();

        let mut t = empty_track(1, "T");
        t.clips.push(clip_for(1, id, 0, 8));
        engine.project_mut().tracks.push(t);
        engine.project_mut().master.gain_db = -6.0206;

        let out = mixdown_stereo(&engine).unwrap();
        // Centre-panned mono at +1 → ~0.707 per channel after the equal-power
        // pan law, then halved by -6dB master.
        let expected = std::f32::consts::FRAC_1_SQRT_2 * 0.5;
        assert!((out[0] - expected).abs() < 5e-3);
    }
}
