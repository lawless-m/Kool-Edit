//! Noise reduction by spectral subtraction.
//!
//! Doc 01 §"Noise reduction" describes the two-step workflow: first capture
//! a noise profile from a quiet region, then apply per-bin gain reduction
//! based on the target's magnitude relative to that profile.
//!
//! This first slice covers the static (no per-frame attack/release smoothing)
//! version. Each STFT bin gets a gain
//!
//!   gain = max(floor, 1 - oversubtraction * (profile / magnitude))
//!
//! which is then multiplied into the complex bin in place. Frequency
//! smoothing and time smoothing are deferred.

use rustfft::num_complex::Complex;

use crate::stft::Stft;

/// Estimate a noise profile (averaged magnitude spectrum) from `samples`,
/// which should contain only noise. Returned magnitudes have length
/// `fft_size`; bins above Nyquist are the conjugate mirror of the lower
/// bins, but kept in the array so callers don't need to know.
pub fn estimate_profile(samples: &[f32], fft_size: usize, hop_size: usize) -> Vec<f32> {
    let stft = Stft::new_hann(fft_size, hop_size);
    let mut acc = vec![0.0_f64; fft_size];
    let mut frame_count = 0_u64;
    stft.process(samples, |bins| {
        for (i, b) in bins.iter().enumerate() {
            acc[i] += b.norm() as f64;
        }
        frame_count += 1;
    });
    let denom = (frame_count as f64).max(1.0);
    acc.into_iter().map(|m| (m / denom) as f32).collect()
}

#[derive(Copy, Clone, Debug)]
pub struct NrSettings {
    /// Reduction depth in dB. The whole subtraction is scaled so 0 dB is a
    /// pass-through; larger values move the noise floor further down.
    pub amount_db: f32,
    /// Minimum gain (in dB). Output never falls below this, which avoids
    /// musical noise from over-aggressive subtraction.
    pub floor_db: f32,
    /// Multiplier applied to the profile before subtraction (typically
    /// 1.0..2.5). Higher = more reduction at the cost of more artefacts.
    pub oversubtraction: f32,
}

/// Apply spectral-subtraction noise reduction to a mono buffer.
pub fn apply(
    input: &[f32],
    profile: &[f32],
    fft_size: usize,
    hop_size: usize,
    settings: NrSettings,
) -> Vec<f32> {
    let stft = Stft::new_hann(fft_size, hop_size);
    let floor_lin = 10.0_f32.powf(settings.floor_db / 20.0);
    let amount_lin = 10.0_f32.powf(-settings.amount_db.abs() / 20.0);
    let oversub = settings.oversubtraction.max(0.0);

    stft.process(input, |bins| {
        for (i, bin) in bins.iter_mut().enumerate() {
            let mag = bin.norm();
            if mag <= 1e-12 {
                continue;
            }
            let p = profile.get(i).copied().unwrap_or(0.0);
            // Gain reduction in [0, 1]. amount_db scales the *depth* of the
            // reduction: at amount=0 dB the gain is unity (no NR); as amount
            // grows, the wet curve approaches the spectral-subtraction
            // formula. Lerp between identity and the subtraction.
            let raw = 1.0 - oversub * (p / mag);
            let target_gain = raw.max(floor_lin).min(1.0);
            // amount_lin = 1.0 means no reduction; 0.0 is full reduction.
            let gain = amount_lin + (1.0 - amount_lin) * target_gain;
            *bin = Complex::new(bin.re * gain, bin.im * gain);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let s: f64 = samples.iter().map(|&x| (x as f64).powi(2)).sum();
        (s / samples.len() as f64).sqrt() as f32
    }

    fn xorshift_noise(len: usize, amp: f32, seed: u32) -> Vec<f32> {
        let mut state = seed | 1;
        let mut step = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        (0..len).map(|_| step() * amp).collect()
    }

    fn sine(len: usize, freq_hz: f32, fs: f32, amp: f32) -> Vec<f32> {
        (0..len)
            .map(|n| amp * (n as f32 / fs * freq_hz * std::f32::consts::TAU).sin())
            .collect()
    }

    #[test]
    fn profile_estimate_grows_with_input_amplitude() {
        let a = xorshift_noise(2048, 0.01, 0xdead_beef);
        let b = xorshift_noise(2048, 0.1, 0xcafe_f00d);
        let pa = estimate_profile(&a, 256, 64);
        let pb = estimate_profile(&b, 256, 64);
        let avg_a: f32 = pa.iter().sum::<f32>() / pa.len() as f32;
        let avg_b: f32 = pb.iter().sum::<f32>() / pb.len() as f32;
        assert!(avg_b > avg_a * 5.0, "louder input should yield larger profile");
    }

    #[test]
    fn nr_with_zero_amount_is_a_passthrough() {
        let signal = sine(4096, 1000.0, 48_000.0, 0.5);
        let profile = vec![0.1_f32; 256];
        let out = apply(
            &signal,
            &profile,
            256,
            64,
            NrSettings {
                amount_db: 0.0,
                floor_db: -120.0,
                oversubtraction: 1.0,
            },
        );
        // Far from the edges, signal should be (approximately) unchanged.
        let inner_in = &signal[256..signal.len() - 256];
        let inner_out = &out[256..out.len() - 256];
        let err = rms(
            &inner_in
                .iter()
                .zip(inner_out.iter())
                .map(|(a, b)| a - b)
                .collect::<Vec<f32>>(),
        );
        assert!(err < 5e-3, "zero-amount NR should pass through, err {err}");
    }

    #[test]
    fn nr_reduces_noise_floor_when_signal_only_has_noise() {
        // Profile = average noise spectrum. Apply NR to fresh noise of the
        // same statistics: the wet output should be quieter than the dry.
        let profile_input = xorshift_noise(8192, 0.05, 0x1234_5678);
        let profile = estimate_profile(&profile_input, 512, 128);

        let target = xorshift_noise(8192, 0.05, 0x9876_5432);
        let dry_rms = rms(&target[512..target.len() - 512]);
        let wet = apply(
            &target,
            &profile,
            512,
            128,
            NrSettings {
                amount_db: 24.0,
                floor_db: -30.0,
                oversubtraction: 1.5,
            },
        );
        let wet_rms = rms(&wet[512..wet.len() - 512]);
        let reduction_db = 20.0 * (wet_rms / dry_rms).log10();
        assert!(
            reduction_db < -3.0,
            "expected significant reduction, got {reduction_db} dB"
        );
    }

    #[test]
    fn nr_preserves_strong_tonal_signal() {
        // 1 kHz sine well above the noise profile level; NR should leave it
        // mostly intact.
        let signal = sine(8192, 1000.0, 48_000.0, 0.5);
        // Pretend the noise profile estimates a low broadband level.
        let profile = vec![0.001_f32; 512];
        let dry_rms = rms(&signal[512..signal.len() - 512]);
        let wet = apply(
            &signal,
            &profile,
            512,
            128,
            NrSettings {
                amount_db: 18.0,
                floor_db: -30.0,
                oversubtraction: 1.5,
            },
        );
        let wet_rms = rms(&wet[512..wet.len() - 512]);
        let preserved_db = 20.0 * (wet_rms / dry_rms).log10();
        assert!(
            preserved_db > -1.0,
            "tonal signal should survive NR, got {preserved_db} dB"
        );
    }
}
