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

use crate::dsp::{self, DspError};
use crate::effect::EffectParams;
use crate::engine::{Engine, QueryError};
use crate::envelope::{AutomationLane, ClipEnvelope, EnvelopeParam};
use crate::ids::{ClipId, SourceId};
use crate::op::{FadeDirection, FadeShape, Op};
use crate::project::{Clip, EffectInstance, Fade, Project, Track};
use crate::range::SampleRange;

#[derive(Debug)]
pub enum MixdownError {
    Query(QueryError),
    Dsp(DspError),
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
            Self::Dsp(e) => write!(f, "mixdown DSP: {e}"),
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

impl From<DspError> for MixdownError {
    fn from(e: DspError) -> Self {
        Self::Dsp(e)
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
        let static_gain = db_to_linear(track.gain_db);
        let static_pan = stereo_balance(track.pan);
        let gain_lane = track_automation_for(track, "track.gain");
        let pan_lane = track_automation_for(track, "track.pan");
        for f in 0..total_frames as usize {
            let gain = match gain_lane {
                Some(l) => l
                    .breakpoints
                    .evaluate_mapped(f as u64, db_or_zero_lin)
                    .unwrap_or(static_gain),
                None => static_gain,
            };
            let (l_w, r_w) = match pan_lane {
                Some(l) => {
                    let pan = l
                        .breakpoints
                        .evaluate(f as u64)
                        .unwrap_or(track.pan);
                    stereo_balance(pan)
                }
                None => static_pan,
            };
            let l = track_buf[f * 2] * gain * l_w;
            let r = track_buf[f * 2 + 1] * gain * r_w;
            master[f * 2] += l;
            master[f * 2 + 1] += r;
        }
    }

    apply_inserts(&mut master, 2, project.sample_rate(), &project.master.inserts)?;

    let mg = db_to_linear(project.master.gain_db);
    if mg != 1.0 {
        for s in &mut master {
            *s *= mg;
        }
    }

    Ok(master)
}

fn check_unsupported(project: &Project) -> Result<(), MixdownError> {
    for track in &project.tracks {
        for lane in &track.automation {
            // Only track-level gain and pan are supported. Insert-parameter
            // automation would need to recompute biquad coefficients per
            // frame and is left for later.
            match lane.parameter.as_str() {
                "track.gain" | "track.pan" => {}
                _ => return Err(MixdownError::Unsupported("automation parameter path")),
            }
        }
        for clip in &track.clips {
            if clip.time_stretch != 1.0 {
                return Err(MixdownError::Unsupported("clip time_stretch"));
            }
            if clip.pitch_shift_cents != 0.0 {
                return Err(MixdownError::Unsupported("clip pitch_shift"));
            }
            for env in &clip.envelopes {
                match env.parameter {
                    EnvelopeParam::Volume | EnvelopeParam::Pan => {}
                }
            }
        }
    }
    Ok(())
}

fn db_or_zero_lin(db: f32) -> f32 {
    if db == f32::NEG_INFINITY {
        0.0
    } else {
        10.0_f32.powf(db / 20.0)
    }
}

fn clip_volume_envelope(envelopes: &[ClipEnvelope]) -> Option<&ClipEnvelope> {
    envelopes
        .iter()
        .find(|e| matches!(e.parameter, EnvelopeParam::Volume))
}

fn clip_pan_envelope(envelopes: &[ClipEnvelope]) -> Option<&ClipEnvelope> {
    envelopes
        .iter()
        .find(|e| matches!(e.parameter, EnvelopeParam::Pan))
}

fn track_automation_for<'a>(track: &'a Track, path: &str) -> Option<&'a AutomationLane> {
    track.automation.iter().find(|l| l.parameter.as_str() == path)
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
    apply_inserts(&mut buf, 2, engine.project().sample_rate(), &track.inserts)?;
    Ok(buf)
}

/// Run a stereo buffer through every (non-bypassed) insert in order. Each
/// insert is materialised as the matching destructive `Op` and processed
/// over the full buffer length, so the same DSP code path serves both
/// destructive editing and live track effects.
fn apply_inserts(
    buffer: &mut Vec<f32>,
    channels: u16,
    sample_rate: u32,
    inserts: &[EffectInstance],
) -> Result<(), MixdownError> {
    if buffer.is_empty() || inserts.is_empty() {
        return Ok(());
    }
    let frames = buffer.len() as u64 / channels as u64;
    let range = SampleRange::new(0, frames).expect("non-empty buffer");
    for insert in inserts {
        if insert.bypass {
            continue;
        }
        let op = match &insert.params {
            EffectParams::Eq(p) => Op::Eq {
                range,
                params: p.clone(),
            },
            EffectParams::Compressor(p) => Op::Compress {
                range,
                params: *p,
            },
            EffectParams::Limiter(p) => Op::Limit {
                range,
                params: *p,
            },
            EffectParams::Reverb(p) => Op::Reverb {
                range,
                params: *p,
            },
            EffectParams::Delay(p) => Op::Delay {
                range,
                params: *p,
            },
        };
        dsp::apply(&op, buffer, channels, sample_rate)?;
    }
    Ok(())
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
    let volume_env = clip_volume_envelope(&clip.envelopes);
    let pan_env = clip_pan_envelope(&clip.envelopes);
    // Mono sources get equal-power-panned into the stereo field (pan=0
    // splits the sample at -3 dB into L and R). Stereo sources are
    // already-stereo: we apply pan as a balance so the centre is unity.
    let pan_law = |pan: f32| match ch {
        1 => mono_pan(pan),
        2 => stereo_balance(pan),
        _ => (1.0, 1.0),
    };
    let static_pan = pan_law(clip.pan);

    let track_pos_start = clip.track_position.start();
    let max_frame = total_frames.min(track_pos_start + frames as u64);

    for (i, frame_idx) in (track_pos_start..max_frame).enumerate() {
        let fade_g = fade_envelope(i, frames, clip.fade_in, clip.fade_out);
        // Volume envelope multiplies on top of the static gain. Per doc 03
        // §"Per-clip envelopes", "Envelope rides on top of clip gain".
        let env_gain = volume_env
            .and_then(|e| e.breakpoints.evaluate_mapped(i as u64, db_or_zero_lin))
            .unwrap_or(1.0);
        let g = clip_gain * fade_g * env_gain;
        // Pan envelope, if any, replaces the static pan for this frame.
        let (l_w, r_w) = match pan_env {
            Some(e) => {
                let p = e
                    .breakpoints
                    .evaluate(i as u64)
                    .unwrap_or(clip.pan);
                pan_law(p)
            }
            None => static_pan,
        };
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
            other => return Err(MixdownError::Unsupported(static_chan_name(other))),
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
    fn track_compressor_insert_reduces_peak() {
        use crate::effect::{CompParams, EffectParams};
        use crate::ids::EffectInstanceId;

        let sample_rate = 48_000;
        // Build a -10 dB sine source long enough for the envelope to settle.
        let frames = sample_rate as usize;
        let amp = (-10.0_f32 / 20.0 * std::f32::consts::LN_10).exp();
        let sine: Vec<f32> = (0..frames)
            .map(|n| amp * (n as f32 / 48.0 * std::f32::consts::TAU).sin())
            .collect();
        let dry_peak = sine.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));

        let mut engine = Engine::new(sample_rate);
        let bytes = wav::encode_f32(&sine, 1, sample_rate);
        let id = engine.import_wav("a.wav", &bytes, now()).unwrap();

        let mut track = empty_track(1, "T");
        track.clips.push(clip_for(1, id, 0, frames as u64));
        track.inserts.push(crate::project::EffectInstance {
            id: EffectInstanceId(1),
            bypass: false,
            params: EffectParams::Compressor(CompParams {
                threshold_db: -20.0,
                ratio: 4.0,
                attack_ms: 1.0,
                release_ms: 100.0,
                makeup_db: 0.0,
                knee_db: 0.0,
            }),
        });
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        // Look at the second half of the L channel where the compressor has
        // settled. The output peak should be ~7.5 dB lower than the dry peak.
        let tail_peak = (frames..frames * 2)
            .step_by(2)
            .map(|i| out[i].abs())
            .fold(0.0_f32, f32::max);
        let reduction_db = 20.0 * (dry_peak / tail_peak).log10();
        // pan-law adds ~3 dB attenuation, so the dry/wet ratio is the static
        // 7.5 dB minus the pan attenuation that affected both sides equally.
        // Easier: assert tail_peak is below dry_peak after factoring the
        // ~0.707 pan weight.
        let _ = reduction_db;
        assert!(
            tail_peak < dry_peak * 0.707 * 0.6,
            "tail_peak {tail_peak} should be well below dry_peak after compression ({dry_peak})"
        );
        // Sanity: signal still present.
        assert!(tail_peak > 0.05);
    }

    #[test]
    fn track_reverb_insert_adds_tail_after_clip_ends() {
        use crate::effect::{EffectParams, ReverbModel, ReverbParams};
        use crate::ids::EffectInstanceId;

        // Source is a short impulse-like blip; track is twice as long so the
        // tail extends into the silent half.
        let sample_rate = 48_000;
        let blip_frames = sample_rate as usize / 100; // 10 ms
        let mut blip = vec![0.0_f32; blip_frames];
        blip[0] = 1.0;
        let bytes = wav::encode_f32(&blip, 1, sample_rate);

        let mut engine = Engine::new(sample_rate);
        let id = engine.import_wav("b.wav", &bytes, now()).unwrap();

        // Make the track buffer long enough that the reverb tail has room
        // to develop. The clip itself is the 10 ms blip; track_position
        // extends to half a second so the reverb fills the silence after
        // the clip ends.
        let mut track = empty_track(1, "T");
        let mut clip = clip_for(1, id, 0, blip_frames as u64);
        clip.track_position = SampleRange::new(0, sample_rate as u64 / 2).unwrap();
        track.clips.push(clip);
        track.inserts.push(crate::project::EffectInstance {
            id: EffectInstanceId(1),
            bypass: false,
            params: EffectParams::Reverb(ReverbParams {
                model: ReverbModel::Hall,
                size: 0.7,
                damping: 0.3,
                mix: 1.0,
            }),
        });
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        // The mixdown buffer is exactly the clip length (no clip extends
        // beyond it), so we read the tail energy in the last 25% of the
        // buffer — that energy is entirely insert output.
        let frames = out.len() / 2;
        let tail_start = frames * 3 / 4;
        let tail_rms = ((tail_start..frames)
            .map(|f| (out[f * 2] as f64).powi(2) + (out[f * 2 + 1] as f64).powi(2))
            .sum::<f64>()
            / (2 * (frames - tail_start)) as f64)
            .sqrt() as f32;
        assert!(tail_rms > 1e-4, "expected reverb tail in last quarter, got rms {tail_rms}");
    }

    #[test]
    fn bypassed_insert_does_not_change_signal() {
        use crate::effect::{CompParams, EffectParams};
        use crate::ids::EffectInstanceId;

        let mut engine = Engine::new(48_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[0.5_f32; 32], 48_000), now())
            .unwrap();

        let mut track = empty_track(1, "T");
        track.clips.push(clip_for(1, id, 0, 32));
        track.inserts.push(crate::project::EffectInstance {
            id: EffectInstanceId(1),
            bypass: true,
            params: EffectParams::Compressor(CompParams {
                threshold_db: -120.0, // would crush everything if active
                ratio: 100.0,
                attack_ms: 0.1,
                release_ms: 50.0,
                makeup_db: 0.0,
                knee_db: 0.0,
            }),
        });
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        // Centre-panned mono at 0.5 → 0.5 × cos(π/4) ≈ 0.354 per channel.
        let expected = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        for f in 0..32 {
            assert!((out[f * 2] - expected).abs() < 5e-3, "L mismatch at frame {f}");
            assert!((out[f * 2 + 1] - expected).abs() < 5e-3, "R mismatch at frame {f}");
        }
    }

    #[test]
    fn master_inserts_run_after_track_summation() {
        use crate::effect::{EffectParams, LimitParams};
        use crate::ids::EffectInstanceId;

        let mut engine = Engine::new(48_000);
        // Loud dry source; clip gain at 0 dB; pan-law leaves ~0.707.
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[1.0_f32; 64], 48_000), now())
            .unwrap();

        let mut track = empty_track(1, "T");
        track.clips.push(clip_for(1, id, 0, 64));
        engine.project_mut().tracks.push(track);

        // Master limiter at -6 dB ceiling (linear ~0.5).
        engine
            .project_mut()
            .master
            .inserts
            .push(crate::project::EffectInstance {
                id: EffectInstanceId(1),
                bypass: false,
                params: EffectParams::Limiter(LimitParams {
                    ceiling_db: -6.0,
                    lookahead_ms: 5.0,
                    release_ms: 50.0,
                }),
            });

        let out = mixdown_stereo(&engine).unwrap();
        let ceiling = 10.0_f32.powf(-6.0 / 20.0);
        let peak = out.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!(peak <= ceiling + 1e-6, "master peak {peak} above ceiling {ceiling}");
    }

    #[test]
    fn clip_volume_envelope_fades_to_silence() {
        use crate::envelope::{Breakpoint, BreakpointSeq, ClipEnvelope, CurveKind, EnvelopeParam};

        let mut engine = Engine::new(48_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[1.0_f32; 100], 48_000), now())
            .unwrap();

        let mut track = empty_track(1, "T");
        let mut clip = clip_for(1, id, 0, 100);
        clip.envelopes.push(ClipEnvelope {
            parameter: EnvelopeParam::Volume,
            breakpoints: BreakpointSeq::new(vec![
                Breakpoint {
                    time: 0,
                    value: 0.0,
                    curve: CurveKind::Linear,
                },
                Breakpoint {
                    time: 99,
                    value: f32::NEG_INFINITY,
                    curve: CurveKind::Linear,
                },
            ])
            .unwrap(),
        });
        track.clips.push(clip);
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        // First frame: full gain × pan-law (~0.707).
        let expected_first = std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - expected_first).abs() < 1e-3);
        // Last frame: -inf dB → silence.
        assert!(out[(99) * 2].abs() < 1e-3);
        // Middle: somewhere between (the envelope is linear in linear gain
        // because the mapping converts dB to linear before interpolation).
        let mid = out[50 * 2];
        assert!(mid > 0.0 && mid < expected_first, "mid {mid}");
    }

    #[test]
    fn clip_pan_envelope_sweeps_left_to_right() {
        use crate::envelope::{Breakpoint, BreakpointSeq, ClipEnvelope, CurveKind, EnvelopeParam};

        let mut engine = Engine::new(48_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[0.5_f32; 100], 48_000), now())
            .unwrap();

        let mut track = empty_track(1, "T");
        let mut clip = clip_for(1, id, 0, 100);
        clip.envelopes.push(ClipEnvelope {
            parameter: EnvelopeParam::Pan,
            breakpoints: BreakpointSeq::new(vec![
                Breakpoint {
                    time: 0,
                    value: -1.0,
                    curve: CurveKind::Linear,
                },
                Breakpoint {
                    time: 99,
                    value: 1.0,
                    curve: CurveKind::Linear,
                },
            ])
            .unwrap(),
        });
        track.clips.push(clip);
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        // At pan=-1 (frame 0): all energy on L, R is silent.
        assert!(out[0] > 0.49 && out[1].abs() < 1e-3);
        // At pan=+1 (frame 99): mirror.
        assert!(out[99 * 2].abs() < 1e-3 && out[99 * 2 + 1] > 0.49);
    }

    #[test]
    fn track_gain_automation_attenuates_over_time() {
        use crate::envelope::{
            AutomationLane, Breakpoint, BreakpointSeq, CurveKind, ParamPath,
        };

        let mut engine = Engine::new(48_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[1.0_f32; 100], 48_000), now())
            .unwrap();

        let mut track = empty_track(1, "T");
        track.clips.push(clip_for(1, id, 0, 100));
        track.automation.push(AutomationLane {
            parameter: ParamPath::new("track.gain"),
            breakpoints: BreakpointSeq::new(vec![
                Breakpoint {
                    time: 0,
                    value: 0.0,
                    curve: CurveKind::Linear,
                },
                Breakpoint {
                    time: 99,
                    value: f32::NEG_INFINITY,
                    curve: CurveKind::Linear,
                },
            ])
            .unwrap(),
        });
        engine.project_mut().tracks.push(track);

        let out = mixdown_stereo(&engine).unwrap();
        let first = out[0].abs();
        let mid = out[50 * 2].abs();
        let last = out[99 * 2].abs();
        assert!(first > 0.49, "first {first}");
        assert!(mid < first, "mid {mid} should be < first {first}");
        assert!(last < 1e-3, "last {last} should be ~zero");
    }

    #[test]
    fn unsupported_automation_path_errors_cleanly() {
        use crate::envelope::{
            AutomationLane, Breakpoint, BreakpointSeq, CurveKind, ParamPath,
        };

        let mut engine = Engine::new(48_000);
        let id = engine
            .import_wav("a.wav", &synth_mono_wav(&[0.5_f32; 8], 48_000), now())
            .unwrap();

        let mut track = empty_track(1, "T");
        track.clips.push(clip_for(1, id, 0, 8));
        track.automation.push(AutomationLane {
            parameter: ParamPath::new("insert.1.threshold"),
            breakpoints: BreakpointSeq::new(vec![Breakpoint {
                time: 0,
                value: -18.0,
                curve: CurveKind::Linear,
            }])
            .unwrap(),
        });
        engine.project_mut().tracks.push(track);
        let err = mixdown_stereo(&engine).unwrap_err();
        assert!(matches!(
            err,
            MixdownError::Unsupported("automation parameter path")
        ));
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
