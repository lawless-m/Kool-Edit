//! BPM (tempo) detection via spectral-flux onsets + autocorrelation.
//!
//! Pipeline:
//!   1. Spectral-flux onset envelope: STFT, then per-frame sum of the
//!      half-wave-rectified positive bin-to-bin magnitude differences.
//!      Captures percussive and tonal onsets; cheap on top of the existing
//!      `Stft` machinery.
//!   2. Local-mean subtraction on the flux signal so steady tones / drift
//!      don't bias autocorrelation toward zero lag.
//!   3. Autocorrelation across lags spanning 50–220 BPM at the chosen
//!      hop's frame rate (~100 fps).
//!   4. Log-Gaussian perceptual prior centred at 120 BPM nudges the octave
//!      choice toward perceptually salient tempi (Parncutt, 1994).
//!   5. Peak-pick the weighted autocorrelation, deduplicate within ~3% BPM,
//!      return the top three candidates so the UI can offer half/double
//!      overrides.
//!
//! Sample buffers are mono. Multichannel callers should sum/average first.

use crate::stft::Stft;

#[derive(Debug, Clone)]
pub struct TempoEstimate {
    /// Best-guess tempo in BPM after perceptual weighting.
    pub bpm: f32,
    /// 0..1 — top candidate's score as a fraction of the sum of all
    /// returned candidates' scores. ~1.0 means a clear winner; ~0.33 means
    /// the top three candidates are roughly equally plausible.
    pub confidence: f32,
    /// Up to three `(bpm, normalised_score)` pairs, ordered best-first.
    /// Scores are scaled so the top candidate is always 1.0.
    pub candidates: Vec<(f32, f32)>,
}

#[derive(Debug)]
pub enum TempoError {
    /// The source was too short to span the lag range. We need at least
    /// `2 * max_lag` flux frames for a stable autocorrelation; below that
    /// the result is dominated by edge effects.
    TooShort {
        needed_frames: usize,
        got_frames: usize,
    },
    Empty,
}

impl std::fmt::Display for TempoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort {
                needed_frames,
                got_frames,
            } => write!(
                f,
                "source too short for tempo estimation (need >= {needed_frames} frames, got {got_frames})"
            ),
            Self::Empty => write!(f, "empty signal"),
        }
    }
}

impl std::error::Error for TempoError {}

const MIN_BPM: f32 = 50.0;
const MAX_BPM: f32 = 220.0;
/// Centre of the log-Gaussian perceptual prior. Parncutt's listening
/// data peaks near 110 BPM with a wide spread; 120 (round number, common
/// in detector papers) is close enough and easier to reason about.
const PERCEPTUAL_CENTRE_BPM: f32 = 120.0;
/// Width of the perceptual prior in octaves. 0.5 octaves → the prior is
/// down ~13 dB at 60 BPM and 240 BPM, biasing away from half/double-tempo
/// errors without flatly forbidding them.
const PERCEPTUAL_SIGMA_OCT: f32 = 0.5;

/// Detect the dominant tempo of a mono signal.
pub fn estimate_bpm(samples: &[f32], sample_rate: u32) -> Result<TempoEstimate, TempoError> {
    if samples.is_empty() || sample_rate == 0 {
        return Err(TempoError::Empty);
    }

    // ~10 ms hop → ~100 fps. fft_size = 4 × hop keeps frequency
    // resolution adequate for percussive transients without being
    // wasteful at high sample rates.
    let hop_size = ((sample_rate as f32 * 0.010).round() as usize).max(64);
    let fft_size = (hop_size * 4).next_power_of_two();
    let fps = sample_rate as f32 / hop_size as f32;

    let max_lag = (fps * 60.0 / MIN_BPM).round() as usize;
    let min_lag = ((fps * 60.0 / MAX_BPM).round() as usize).max(1);
    // 2× the longest lag is the bare minimum for a stable autocorrelation
    // peak; below that, the lag-window length shrinks too much.
    let needed_flux_frames = max_lag * 2;
    let needed_input_frames = needed_flux_frames * hop_size;
    if samples.len() < needed_input_frames {
        return Err(TempoError::TooShort {
            needed_frames: needed_input_frames,
            got_frames: samples.len(),
        });
    }

    // 1) Spectral-flux onset envelope.
    let stft = Stft::new_hann(fft_size, hop_size);
    let frames = stft.analyze(samples);
    let bin_count = fft_size / 2 + 1;
    let mut flux = vec![0.0_f32; frames.len()];
    let mut prev_mag = vec![0.0_f32; bin_count];
    for (n, frame) in frames.iter().enumerate() {
        let mut sum = 0.0_f32;
        for k in 0..bin_count {
            let mag = frame[k].norm();
            let diff = mag - prev_mag[k];
            if diff > 0.0 {
                sum += diff;
            }
            prev_mag[k] = mag;
        }
        flux[n] = sum;
    }

    // 2) Subtract a ~200 ms moving mean. O(N*W) on a small W is cheap and
    // avoids edge-handling nonsense from a running-sum implementation.
    let smooth_w = (fps * 0.2).round() as usize;
    if smooth_w > 1 && flux.len() > smooth_w {
        let mut local_mean = vec![0.0_f32; flux.len()];
        for i in 0..flux.len() {
            let lo = i.saturating_sub(smooth_w / 2);
            let hi = (i + smooth_w / 2 + 1).min(flux.len());
            let mut s = 0.0_f32;
            for j in lo..hi {
                s += flux[j];
            }
            local_mean[i] = s / (hi - lo) as f32;
        }
        for i in 0..flux.len() {
            flux[i] = (flux[i] - local_mean[i]).max(0.0);
        }
    }

    // 3) Autocorrelation across the tempo lag range.
    let n = flux.len();
    if n < min_lag + 2 {
        return Err(TempoError::TooShort {
            needed_frames: (min_lag + 2) * hop_size,
            got_frames: samples.len(),
        });
    }
    let max_lag = max_lag.min(n.saturating_sub(1));
    let mut acf = vec![0.0_f32; max_lag + 1];
    for lag in min_lag..=max_lag {
        let mut s = 0.0_f32;
        for i in lag..n {
            s += flux[i] * flux[i - lag];
        }
        acf[lag] = s;
    }

    // 4) Perceptual weighting (log-Gaussian on BPM).
    let mut weighted = vec![0.0_f32; max_lag + 1];
    for lag in min_lag..=max_lag {
        let bpm = 60.0 * fps / lag as f32;
        let z = (bpm.max(1.0) / PERCEPTUAL_CENTRE_BPM).log2() / PERCEPTUAL_SIGMA_OCT;
        let w = (-0.5 * z * z).exp();
        weighted[lag] = acf[lag] * w;
    }

    // 5) Peak pick. Strict local maxima only.
    let mut peaks: Vec<(usize, f32)> = Vec::new();
    for lag in (min_lag + 1)..max_lag {
        let v = weighted[lag];
        if v > weighted[lag - 1] && v > weighted[lag + 1] && v > 0.0 {
            peaks.push((lag, v));
        }
    }
    if peaks.is_empty() {
        // Degenerate (e.g. monotonic weighted curve): use the global max
        // in the range so the caller still gets a single candidate back.
        let mut best = (min_lag, weighted[min_lag]);
        for lag in min_lag..=max_lag {
            if weighted[lag] > best.1 {
                best = (lag, weighted[lag]);
            }
        }
        peaks.push(best);
    }
    peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    // Deduplicate candidates within ~3% BPM (one is usually a sub-bin
    // shoulder of the other) and keep the top three.
    let mut chosen: Vec<(f32, f32)> = Vec::new();
    for (lag, score) in peaks {
        let bpm = 60.0 * fps / lag as f32;
        let dup = chosen.iter().any(|(b, _)| {
            let ratio = if *b > bpm { *b / bpm } else { bpm / *b };
            ratio < 1.03
        });
        if dup {
            continue;
        }
        chosen.push((bpm, score));
        if chosen.len() == 3 {
            break;
        }
    }

    let top_score = chosen[0].1.max(1e-12);
    let sum_score: f32 = chosen.iter().map(|(_, s)| *s).sum::<f32>().max(1e-12);
    let confidence = chosen[0].1 / sum_score;
    for (_, s) in chosen.iter_mut() {
        *s /= top_score;
    }

    Ok(TempoEstimate {
        bpm: chosen[0].0,
        confidence,
        candidates: chosen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    /// Render a click-track-like signal with an impulse every `bpm` BPM.
    /// 5 ms exponential decay so each click has a real spectral footprint
    /// (not a pure dirac, which the FFT would smear unrealistically).
    fn click_track(bpm: f32, sr: u32, seconds: f32) -> Vec<f32> {
        let total = (seconds * sr as f32) as usize;
        let click_period_samples = (60.0 / bpm * sr as f32) as usize;
        let decay_samples = (0.005 * sr as f32) as usize;
        let mut out = vec![0.0_f32; total];
        let mut next = 0;
        while next < total {
            for i in 0..decay_samples.min(total - next) {
                let env = (-(i as f32) / (decay_samples as f32 / 4.0)).exp();
                out[next + i] += env * (TAU * 2_000.0 * i as f32 / sr as f32).sin();
            }
            next += click_period_samples;
        }
        out
    }

    #[test]
    fn detects_120_bpm_click_track() {
        let est = estimate_bpm(&click_track(120.0, 22_050, 8.0), 22_050).unwrap();
        // Within ~2 BPM is plenty for a click track at this resolution.
        assert!(
            (est.bpm - 120.0).abs() < 2.0,
            "expected ~120, got {} (candidates: {:?})",
            est.bpm,
            est.candidates
        );
    }

    #[test]
    fn detects_90_bpm_click_track() {
        let est = estimate_bpm(&click_track(90.0, 22_050, 10.0), 22_050).unwrap();
        assert!(
            (est.bpm - 90.0).abs() < 2.0,
            "expected ~90, got {} (candidates: {:?})",
            est.bpm,
            est.candidates
        );
    }

    #[test]
    fn detects_160_bpm_click_track() {
        let est = estimate_bpm(&click_track(160.0, 22_050, 8.0), 22_050).unwrap();
        assert!(
            (est.bpm - 160.0).abs() < 3.0,
            "expected ~160, got {} (candidates: {:?})",
            est.bpm,
            est.candidates
        );
    }

    #[test]
    fn too_short_signal_errors() {
        // 0.5 s — well under the 2× max-lag minimum for the 50 BPM bound.
        let samples = click_track(120.0, 22_050, 0.5);
        let err = estimate_bpm(&samples, 22_050).unwrap_err();
        assert!(
            matches!(err, TempoError::TooShort { .. }),
            "expected TooShort, got {err:?}"
        );
    }

    #[test]
    fn empty_signal_errors() {
        assert!(matches!(
            estimate_bpm(&[], 22_050),
            Err(TempoError::Empty)
        ));
    }

    #[test]
    fn confidence_is_higher_with_a_clear_winner() {
        // A click track should have a near-1.0 top, much higher than a
        // random-noise signal where no lag dominates.
        let click = click_track(120.0, 22_050, 8.0);
        let noise: Vec<f32> = (0..22_050 * 8)
            .map(|i| {
                // Cheap deterministic pseudo-noise so the test is repeatable.
                let x = (i as u32).wrapping_mul(2_654_435_761);
                (x as i32 as f32) / (i32::MAX as f32) * 0.3
            })
            .collect();
        let click_conf = estimate_bpm(&click, 22_050).unwrap().confidence;
        let noise_conf = estimate_bpm(&noise, 22_050).unwrap().confidence;
        assert!(
            click_conf > noise_conf,
            "click conf {click_conf} should beat noise conf {noise_conf}"
        );
    }
}
