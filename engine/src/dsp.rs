//! DSP primitives for sample-region operations.
//!
//! Implemented: Silence, Gain, Fade (4 shapes × 2 directions), DcRemove,
//! Reverse, Cut (length-changing), Generate (Silence/Tone/Noise — DTMF and
//! Sweep deferred), Normalize (Peak and RMS — LUFS deferred). The remaining
//! Op variants return [`DspError::Unsupported`] so callers get a clean
//! failure rather than silent no-ops.
//!
//! Sample buffers are interleaved `Vec<f32>`. The buffer is `&mut Vec<f32>`
//! rather than `&mut [f32]` because Cut and Generate change the frame count.
//! Range arithmetic is in frames; the channel count and sample rate are
//! supplied alongside (sample rate is needed by the tone generator).

use std::f32::consts::PI;

use crate::op::{
    FadeDirection, FadeShape, GeneratorParams, NoiseColor, NormTarget, Op, ToneShape,
};
use crate::range::SampleRange;

#[derive(Debug)]
pub enum DspError {
    Unsupported(&'static str),
    RangeOutsideBuffer { range: SampleRange, frames: u64 },
    InsertPositionPastEnd { at: u64, frames: u64 },
    EmptySignal,
}

impl std::fmt::Display for DspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(k) => write!(f, "op `{k}` is not implemented yet"),
            Self::RangeOutsideBuffer { range, frames } => write!(
                f,
                "range {}..{} exceeds buffer length ({frames} frames)",
                range.start(),
                range.end()
            ),
            Self::InsertPositionPastEnd { at, frames } => write!(
                f,
                "insertion at frame {at} is past the end of the buffer ({frames} frames)"
            ),
            Self::EmptySignal => {
                write!(f, "operation requires non-empty signal in the target range")
            }
        }
    }
}

impl std::error::Error for DspError {}

/// Apply a single op to `buffer` in place. May change the buffer's length
/// (Cut, Generate). `sample_rate` is the source's native rate, used by the
/// tone generator.
pub fn apply(
    op: &Op,
    buffer: &mut Vec<f32>,
    channels: u16,
    sample_rate: u32,
) -> Result<(), DspError> {
    match op {
        Op::Silence { range } => silence(buffer, channels, *range),
        Op::Gain { range, db } => gain(buffer, channels, *range, *db),
        Op::Fade {
            range,
            shape,
            direction,
        } => fade(buffer, channels, *range, *shape, *direction),
        Op::DcRemove { range } => dc_remove(buffer, channels, *range),
        Op::Reverse { range } => reverse(buffer, channels, *range),

        Op::Cut { range } => cut(buffer, channels, *range),
        Op::Normalize {
            range,
            target,
            value_db,
        } => normalize(buffer, channels, *range, *target, *value_db),
        Op::Generate { at, length, params } => {
            generate(buffer, channels, sample_rate, *at, *length, params)
        }

        Op::Insert { .. } => Err(DspError::Unsupported("Insert")),
        Op::PasteMix { .. } => Err(DspError::Unsupported("PasteMix")),
        Op::PasteOver { .. } => Err(DspError::Unsupported("PasteOver")),
        Op::Eq { .. } => Err(DspError::Unsupported("Eq")),
        Op::Compress { .. } => Err(DspError::Unsupported("Compress")),
        Op::Limit { .. } => Err(DspError::Unsupported("Limit")),
        Op::Reverb { .. } => Err(DspError::Unsupported("Reverb")),
        Op::Delay { .. } => Err(DspError::Unsupported("Delay")),
        Op::TimeStretch { .. } => Err(DspError::Unsupported("TimeStretch")),
        Op::PitchShift { .. } => Err(DspError::Unsupported("PitchShift")),
        Op::NoiseReduce { .. } => Err(DspError::Unsupported("NoiseReduce")),
        Op::SpectralEdit { .. } => Err(DspError::Unsupported("SpectralEdit")),
    }
}

fn slice_for(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
) -> Result<&mut [f32], DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    let start = (range.start() * ch) as usize;
    let end = (range.end() * ch) as usize;
    Ok(&mut buffer[start..end])
}

fn silence(buffer: &mut [f32], channels: u16, range: SampleRange) -> Result<(), DspError> {
    slice_for(buffer, channels, range)?.fill(0.0);
    Ok(())
}

fn gain(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    db: f32,
) -> Result<(), DspError> {
    let g = db_to_linear(db);
    for s in slice_for(buffer, channels, range)?.iter_mut() {
        *s *= g;
    }
    Ok(())
}

fn fade(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    shape: FadeShape,
    direction: FadeDirection,
) -> Result<(), DspError> {
    let ch = channels as usize;
    let s = slice_for(buffer, channels, range)?;
    let frames = s.len() / ch;
    if frames == 0 {
        return Ok(());
    }
    let denom = (frames - 1).max(1) as f32;
    for f in 0..frames {
        let t = f as f32 / denom;
        let g = fade_gain(t, shape, direction);
        for c in 0..ch {
            s[f * ch + c] *= g;
        }
    }
    Ok(())
}

fn dc_remove(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
) -> Result<(), DspError> {
    let ch = channels as usize;
    let s = slice_for(buffer, channels, range)?;
    if s.is_empty() {
        return Ok(());
    }
    for c in 0..ch {
        let mut sum = 0.0_f64;
        let mut count = 0_u64;
        let mut i = c;
        while i < s.len() {
            sum += s[i] as f64;
            count += 1;
            i += ch;
        }
        if count == 0 {
            continue;
        }
        let mean = (sum / count as f64) as f32;
        let mut i = c;
        while i < s.len() {
            s[i] -= mean;
            i += ch;
        }
    }
    Ok(())
}

fn reverse(buffer: &mut [f32], channels: u16, range: SampleRange) -> Result<(), DspError> {
    let ch = channels as usize;
    let s = slice_for(buffer, channels, range)?;
    let frames = s.len() / ch;
    for f in 0..frames / 2 {
        let lo = f * ch;
        let hi = (frames - 1 - f) * ch;
        for c in 0..ch {
            s.swap(lo + c, hi + c);
        }
    }
    Ok(())
}

fn cut(buffer: &mut Vec<f32>, channels: u16, range: SampleRange) -> Result<(), DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    let start = (range.start() * ch) as usize;
    let end = (range.end() * ch) as usize;
    buffer.drain(start..end);
    Ok(())
}

fn normalize(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    target: NormTarget,
    value_db: f32,
) -> Result<(), DspError> {
    let s = slice_for(buffer, channels, range)?;
    if s.is_empty() {
        return Err(DspError::EmptySignal);
    }
    let target_linear = db_to_linear(value_db);
    let scale = match target {
        NormTarget::Peak => {
            let peak = s.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
            if peak == 0.0 {
                return Err(DspError::EmptySignal);
            }
            target_linear / peak
        }
        NormTarget::Rms => {
            let mut sum_sq = 0.0_f64;
            for &x in s.iter() {
                sum_sq += (x as f64) * (x as f64);
            }
            let rms = (sum_sq / s.len() as f64).sqrt() as f32;
            if rms == 0.0 {
                return Err(DspError::EmptySignal);
            }
            target_linear / rms
        }
        NormTarget::LufsIntegrated => {
            return Err(DspError::Unsupported("Normalize/LUFS"));
        }
    };
    for x in s.iter_mut() {
        *x *= scale;
    }
    Ok(())
}

fn generate(
    buffer: &mut Vec<f32>,
    channels: u16,
    sample_rate: u32,
    at: u64,
    length: u64,
    params: &GeneratorParams,
) -> Result<(), DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if at > total_frames {
        return Err(DspError::InsertPositionPastEnd {
            at,
            frames: total_frames,
        });
    }
    let mono = synthesize(params, length as usize, sample_rate, at)?;
    let pos = (at * ch) as usize;
    let to_insert: Vec<f32> = mono
        .into_iter()
        .flat_map(|s| std::iter::repeat_n(s, channels as usize))
        .collect();
    buffer.splice(pos..pos, to_insert);
    Ok(())
}

fn synthesize(
    params: &GeneratorParams,
    frames: usize,
    sample_rate: u32,
    seed_offset: u64,
) -> Result<Vec<f32>, DspError> {
    match params {
        GeneratorParams::Silence => Ok(vec![0.0; frames]),
        GeneratorParams::Tone {
            shape,
            frequency_hz,
            amplitude_db,
        } => {
            let amp = db_to_linear(*amplitude_db);
            let inv_sr = 1.0 / sample_rate as f32;
            let mut out = Vec::with_capacity(frames);
            for n in 0..frames {
                let phase = (n as f32 * *frequency_hz * inv_sr).fract();
                let raw = match shape {
                    ToneShape::Sine => (phase * 2.0 * PI).sin(),
                    ToneShape::Square => {
                        if phase < 0.5 {
                            1.0
                        } else {
                            -1.0
                        }
                    }
                    ToneShape::Saw => 2.0 * phase - 1.0,
                    ToneShape::Triangle => {
                        // Range -1..1; piecewise linear, peak at 0.25 phase.
                        let p = (phase + 0.25).fract();
                        4.0 * (p - 0.5).abs() - 1.0
                    }
                };
                out.push(raw * amp);
            }
            Ok(out)
        }
        GeneratorParams::Noise {
            color,
            amplitude_db,
        } => {
            let amp = db_to_linear(*amplitude_db);
            // Deterministic per-op PRNG so applying the same op twice produces
            // the same samples. Seed mixes the insertion frame so two
            // adjacent generates don't share a sequence.
            let mut state = (seed_offset.wrapping_mul(0x9e3779b97f4a7c15) ^ 0xdeadbeef)
                as u32
                | 1; // ensure non-zero
            let mut next_uniform = || -> f32 {
                let x = xorshift32(&mut state);
                (x as f32 / u32::MAX as f32) * 2.0 - 1.0
            };
            match color {
                NoiseColor::White => {
                    Ok((0..frames).map(|_| next_uniform() * amp).collect())
                }
                NoiseColor::Pink | NoiseColor::Brown => {
                    Err(DspError::Unsupported("Generate/Noise non-white"))
                }
            }
        }
        GeneratorParams::Dtmf { .. } => Err(DspError::Unsupported("Generate/Dtmf")),
        GeneratorParams::Sweep { .. } => Err(DspError::Unsupported("Generate/Sweep")),
    }
}

fn xorshift32(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

/// Fade gain at normalised position `t ∈ [0, 1]`. Direction `In` produces a
/// 0 → 1 curve, `Out` mirrors it. Shape selects between linear, log
/// (fast-then-slow), exp (slow-then-fast), and the smooth s-curve. The exact
/// shape coefficients are tuneable later — the test asserts shape, not the
/// specific values.
fn fade_gain(t: f32, shape: FadeShape, direction: FadeDirection) -> f32 {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u64, end: u64) -> SampleRange {
        SampleRange::new(start, end).unwrap()
    }

    fn buf(v: &[f32]) -> Vec<f32> {
        v.to_vec()
    }

    #[test]
    fn silence_zeros_a_range() {
        let mut samples = buf(&[0.5, -0.5, 0.5, -0.5, 0.5]);
        apply(&Op::Silence { range: r(1, 4) }, &mut samples, 1, 48_000).unwrap();
        assert_eq!(samples, vec![0.5, 0.0, 0.0, 0.0, 0.5]);
    }

    #[test]
    fn gain_minus_six_db_halves_amplitude() {
        let mut samples = vec![1.0_f32; 4];
        apply(
            &Op::Gain {
                range: r(0, 4),
                db: -6.0206,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        for s in &samples {
            assert!((s - 0.5).abs() < 1e-3, "got {s}");
        }
    }

    #[test]
    fn fade_in_linear_goes_zero_to_one() {
        let mut samples = vec![1.0_f32; 5];
        apply(
            &Op::Fade {
                range: r(0, 5),
                shape: FadeShape::Linear,
                direction: FadeDirection::In,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert!((samples[0] - 0.0).abs() < 1e-6);
        assert!((samples[4] - 1.0).abs() < 1e-6);
        for w in samples.windows(2) {
            assert!(w[1] >= w[0]);
        }
    }

    #[test]
    fn fade_out_linear_goes_one_to_zero() {
        let mut samples = vec![1.0_f32; 5];
        apply(
            &Op::Fade {
                range: r(0, 5),
                shape: FadeShape::Linear,
                direction: FadeDirection::Out,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert!((samples[0] - 1.0).abs() < 1e-6);
        assert!((samples[4] - 0.0).abs() < 1e-6);
        for w in samples.windows(2) {
            assert!(w[1] <= w[0]);
        }
    }

    #[test]
    fn fade_scurve_endpoints_match_linear() {
        let mut a = vec![1.0_f32; 11];
        apply(
            &Op::Fade {
                range: r(0, 11),
                shape: FadeShape::SCurve,
                direction: FadeDirection::In,
            },
            &mut a,
            1,
            48_000,
        )
        .unwrap();
        assert!((a[0] - 0.0).abs() < 1e-6);
        assert!((a[10] - 1.0).abs() < 1e-6);
        assert!((a[5] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn dc_remove_subtracts_mean() {
        let mut samples = vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        apply(&Op::DcRemove { range: r(0, 6) }, &mut samples, 1, 48_000).unwrap();
        let mean: f32 = samples.iter().sum::<f32>() / samples.len() as f32;
        assert!(mean.abs() < 1e-6);
    }

    #[test]
    fn dc_remove_handles_each_channel_independently() {
        let mut samples = vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        apply(&Op::DcRemove { range: r(0, 3) }, &mut samples, 2, 48_000).unwrap();
        let l_mean = (samples[0] + samples[2] + samples[4]) / 3.0;
        let r_mean = (samples[1] + samples[3] + samples[5]) / 3.0;
        assert!(l_mean.abs() < 1e-6);
        assert!(r_mean.abs() < 1e-6);
    }

    #[test]
    fn reverse_reverses_frames() {
        let mut samples = buf(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        apply(&Op::Reverse { range: r(0, 5) }, &mut samples, 1, 48_000).unwrap();
        assert_eq!(samples, vec![5.0, 4.0, 3.0, 2.0, 1.0]);
    }

    #[test]
    fn reverse_keeps_channel_pairing_in_stereo() {
        let mut samples = buf(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        apply(&Op::Reverse { range: r(0, 3) }, &mut samples, 2, 48_000).unwrap();
        assert_eq!(samples, vec![5.0, 6.0, 3.0, 4.0, 1.0, 2.0]);
    }

    #[test]
    fn range_outside_buffer_returns_error() {
        let mut samples = vec![0.0; 4];
        let err =
            apply(&Op::Silence { range: r(0, 100) }, &mut samples, 1, 48_000).unwrap_err();
        assert!(matches!(err, DspError::RangeOutsideBuffer { .. }));
    }

    #[test]
    fn unsupported_ops_are_reported() {
        use crate::effect::EqParams;
        let mut samples = vec![0.0; 4];
        let err = apply(
            &Op::Eq {
                range: r(0, 4),
                params: EqParams::default(),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap_err();
        assert!(matches!(err, DspError::Unsupported("Eq")));
    }

    #[test]
    fn cut_removes_frames_and_shrinks_buffer() {
        let mut samples = buf(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        apply(&Op::Cut { range: r(1, 4) }, &mut samples, 1, 48_000).unwrap();
        assert_eq!(samples, vec![1.0, 5.0]);
    }

    #[test]
    fn cut_in_stereo_removes_frame_pairs() {
        let mut samples = buf(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        apply(&Op::Cut { range: r(1, 2) }, &mut samples, 2, 48_000).unwrap();
        assert_eq!(samples, vec![1.0, -1.0, 3.0, -3.0]);
    }

    #[test]
    fn normalize_peak_scales_to_target_db() {
        let mut samples = buf(&[0.25, -0.5, 0.5, -0.25]);
        apply(
            &Op::Normalize {
                range: r(0, 4),
                target: NormTarget::Peak,
                value_db: 0.0,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        let peak = samples.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!((peak - 1.0).abs() < 1e-5);
    }

    #[test]
    fn normalize_peak_to_minus_six_db_caps_at_half() {
        let mut samples = buf(&[0.1, -0.2, 0.3]);
        apply(
            &Op::Normalize {
                range: r(0, 3),
                target: NormTarget::Peak,
                value_db: -6.0206,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        let peak = samples.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 1e-3);
    }

    #[test]
    fn normalize_rms_scales_to_target_rms() {
        let mut samples = buf(&[0.1, -0.1, 0.1, -0.1, 0.1]);
        apply(
            &Op::Normalize {
                range: r(0, 5),
                target: NormTarget::Rms,
                value_db: 0.0,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        let sum_sq: f64 = samples.iter().map(|&x| (x as f64).powi(2)).sum();
        let rms = (sum_sq / samples.len() as f64).sqrt();
        assert!((rms - 1.0).abs() < 1e-5);
    }

    #[test]
    fn normalize_silent_signal_errors() {
        let mut samples = vec![0.0; 4];
        let err = apply(
            &Op::Normalize {
                range: r(0, 4),
                target: NormTarget::Peak,
                value_db: 0.0,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap_err();
        assert!(matches!(err, DspError::EmptySignal));
    }

    #[test]
    fn generate_silence_inserts_zeros() {
        let mut samples = buf(&[1.0, 1.0, 1.0]);
        apply(
            &Op::Generate {
                at: 1,
                length: 3,
                params: GeneratorParams::Silence,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_eq!(samples, vec![1.0, 0.0, 0.0, 0.0, 1.0, 1.0]);
    }

    #[test]
    fn generate_tone_sine_has_expected_amplitude() {
        let mut samples = vec![];
        apply(
            &Op::Generate {
                at: 0,
                length: 480,
                params: GeneratorParams::Tone {
                    shape: ToneShape::Sine,
                    frequency_hz: 100.0,
                    amplitude_db: 0.0,
                },
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_eq!(samples.len(), 480);
        let peak = samples.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!((peak - 1.0).abs() < 1e-2, "expected near-unity peak, got {peak}");
    }

    #[test]
    fn generate_tone_square_alternates_sign() {
        let mut samples = vec![];
        apply(
            &Op::Generate {
                at: 0,
                length: 480,
                params: GeneratorParams::Tone {
                    shape: ToneShape::Square,
                    frequency_hz: 100.0,
                    amplitude_db: 0.0,
                },
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        // Square wave at any normalised amplitude has ±1 only.
        for s in &samples {
            assert!(s.abs() > 0.99, "got {s}");
        }
    }

    #[test]
    fn generate_white_noise_is_deterministic() {
        let mut a = vec![];
        let mut b = vec![];
        let op = Op::Generate {
            at: 0,
            length: 100,
            params: GeneratorParams::Noise {
                color: NoiseColor::White,
                amplitude_db: -6.0,
            },
        };
        apply(&op, &mut a, 1, 48_000).unwrap();
        apply(&op, &mut b, 1, 48_000).unwrap();
        assert_eq!(a, b, "same op must produce same samples");
        let peak = a.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!(peak <= db_to_linear(-6.0) + 1e-5);
    }

    #[test]
    fn generate_in_stereo_replicates_to_all_channels() {
        let mut samples = vec![];
        apply(
            &Op::Generate {
                at: 0,
                length: 4,
                params: GeneratorParams::Tone {
                    shape: ToneShape::Sine,
                    frequency_hz: 12_000.0,
                    amplitude_db: 0.0,
                },
            },
            &mut samples,
            2,
            48_000,
        )
        .unwrap();
        assert_eq!(samples.len(), 8);
        for f in 0..4 {
            assert_eq!(samples[f * 2], samples[f * 2 + 1]);
        }
    }

    #[test]
    fn generate_position_past_end_errors() {
        let mut samples = vec![1.0, 2.0];
        let err = apply(
            &Op::Generate {
                at: 99,
                length: 1,
                params: GeneratorParams::Silence,
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap_err();
        assert!(matches!(err, DspError::InsertPositionPastEnd { .. }));
    }
}
