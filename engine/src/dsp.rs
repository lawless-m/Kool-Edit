//! DSP primitives for sample-region operations.
//!
//! First slice: Silence, Gain, Fade (4 shapes × 2 directions), DcRemove,
//! Reverse. These are the simplest ops to implement and they cover most of
//! the destructive editor's "non-effect" toolbox.
//!
//! The rest of the Op variants (Cut/Insert/Paste, Eq/Compress/etc., Time/
//! Pitch, Noise reduction, Spectral edits, Generators) return
//! [`DspError::Unsupported`] so callers get a clean failure rather than
//! silent no-ops while DSP work is in flight.
//!
//! Sample buffers are interleaved float32 throughout. Range arithmetic is in
//! frames; the channel count is supplied alongside.

use crate::op::{FadeDirection, FadeShape, Op};
use crate::range::SampleRange;

#[derive(Debug)]
pub enum DspError {
    Unsupported(&'static str),
    RangeOutsideBuffer { range: SampleRange, frames: u64 },
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
        }
    }
}

impl std::error::Error for DspError {}

/// Apply a single op in place to `samples` (interleaved). Returns an error if
/// the op needs more than the buffer can give, or if it isn't yet
/// implemented.
pub fn apply(op: &Op, samples: &mut [f32], channels: u16) -> Result<(), DspError> {
    match op {
        Op::Silence { range } => silence(samples, channels, *range),
        Op::Gain { range, db } => gain(samples, channels, *range, *db),
        Op::Fade {
            range,
            shape,
            direction,
        } => fade(samples, channels, *range, *shape, *direction),
        Op::DcRemove { range } => dc_remove(samples, channels, *range),
        Op::Reverse { range } => reverse(samples, channels, *range),

        Op::Normalize { .. } => Err(DspError::Unsupported("Normalize")),
        Op::Cut { .. } => Err(DspError::Unsupported("Cut")),
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
        Op::Generate { .. } => Err(DspError::Unsupported("Generate")),
    }
}

fn slice_for(
    samples: &mut [f32],
    channels: u16,
    range: SampleRange,
) -> Result<&mut [f32], DspError> {
    let ch = channels as u64;
    let total_frames = samples.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    let start = (range.start() * ch) as usize;
    let end = (range.end() * ch) as usize;
    Ok(&mut samples[start..end])
}

fn silence(samples: &mut [f32], channels: u16, range: SampleRange) -> Result<(), DspError> {
    slice_for(samples, channels, range)?.fill(0.0);
    Ok(())
}

fn gain(
    samples: &mut [f32],
    channels: u16,
    range: SampleRange,
    db: f32,
) -> Result<(), DspError> {
    let g = db_to_linear(db);
    for s in slice_for(samples, channels, range)?.iter_mut() {
        *s *= g;
    }
    Ok(())
}

fn fade(
    samples: &mut [f32],
    channels: u16,
    range: SampleRange,
    shape: FadeShape,
    direction: FadeDirection,
) -> Result<(), DspError> {
    let ch = channels as usize;
    let s = slice_for(samples, channels, range)?;
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
    samples: &mut [f32],
    channels: u16,
    range: SampleRange,
) -> Result<(), DspError> {
    let ch = channels as usize;
    let s = slice_for(samples, channels, range)?;
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

fn reverse(samples: &mut [f32], channels: u16, range: SampleRange) -> Result<(), DspError> {
    let ch = channels as usize;
    let s = slice_for(samples, channels, range)?;
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

    #[test]
    fn silence_zeros_a_range() {
        let mut samples = vec![0.5, -0.5, 0.5, -0.5, 0.5];
        apply(&Op::Silence { range: r(1, 4) }, &mut samples, 1).unwrap();
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
        )
        .unwrap();
        assert!((samples[0] - 0.0).abs() < 1e-6);
        assert!((samples[4] - 1.0).abs() < 1e-6);
        // Strictly increasing
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
        // A non-linear shape still pins the endpoints to 0 and 1.
        let mut a = vec![1.0_f32; 11];
        apply(
            &Op::Fade {
                range: r(0, 11),
                shape: FadeShape::SCurve,
                direction: FadeDirection::In,
            },
            &mut a,
            1,
        )
        .unwrap();
        assert!((a[0] - 0.0).abs() < 1e-6);
        assert!((a[10] - 1.0).abs() < 1e-6);
        // Midpoint is exactly 0.5 by symmetry of the smoothstep formula.
        assert!((a[5] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn dc_remove_subtracts_mean() {
        // Mean is +0.5 → after removal should be zero-mean.
        let mut samples = vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0];
        apply(&Op::DcRemove { range: r(0, 6) }, &mut samples, 1).unwrap();
        let mean: f32 = samples.iter().sum::<f32>() / samples.len() as f32;
        assert!(mean.abs() < 1e-6);
    }

    #[test]
    fn dc_remove_handles_each_channel_independently() {
        // Stereo with offset on the left channel only.
        let mut samples = vec![1.0, 0.0, 1.0, 0.0, 1.0, 0.0]; // L=1 R=0 alternating frames
        apply(&Op::DcRemove { range: r(0, 3) }, &mut samples, 2).unwrap();
        // Left channel should be zero-mean; right untouched.
        let l_mean = (samples[0] + samples[2] + samples[4]) / 3.0;
        let r_mean = (samples[1] + samples[3] + samples[5]) / 3.0;
        assert!(l_mean.abs() < 1e-6);
        assert!(r_mean.abs() < 1e-6); // already zero-mean
    }

    #[test]
    fn reverse_reverses_frames() {
        let mut samples = vec![1.0, 2.0, 3.0, 4.0, 5.0];
        apply(&Op::Reverse { range: r(0, 5) }, &mut samples, 1).unwrap();
        assert_eq!(samples, vec![5.0, 4.0, 3.0, 2.0, 1.0]);
    }

    #[test]
    fn reverse_keeps_channel_pairing_in_stereo() {
        // (L,R) pairs: (1,2) (3,4) (5,6) → reverse → (5,6) (3,4) (1,2)
        let mut samples = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        apply(&Op::Reverse { range: r(0, 3) }, &mut samples, 2).unwrap();
        assert_eq!(samples, vec![5.0, 6.0, 3.0, 4.0, 1.0, 2.0]);
    }

    #[test]
    fn range_outside_buffer_returns_error() {
        let mut samples = vec![0.0; 4];
        let err = apply(&Op::Silence { range: r(0, 100) }, &mut samples, 1).unwrap_err();
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
        )
        .unwrap_err();
        assert!(matches!(err, DspError::Unsupported("Eq")));
    }
}
