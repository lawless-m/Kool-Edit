//! Short-Time Fourier Transform analysis / synthesis.
//!
//! Doc 02 §"STFT machinery" describes a single processor used by spectral
//! view, noise reduction, time stretch, and pitch shift. This first slice
//! covers the analysis-modify-synthesis loop with a Hann window and
//! overlap-add reconstruction; the more specialised time/pitch processors
//! land later.
//!
//! Sample buffers are mono. Multichannel processing is the caller's job
//! (interleave or process channels separately).

use std::sync::Arc;

use rustfft::{Fft, FftPlanner, num_complex::Complex};

pub struct Stft {
    pub fft_size: usize,
    pub hop_size: usize,
    window: Vec<f32>,
    forward: Arc<dyn Fft<f32>>,
    inverse: Arc<dyn Fft<f32>>,
    /// Steady-state Constant-Overlap-Add normalisation: the sum of overlapping
    /// squared windows at any output sample. Pre-computed once so the per-
    /// frame loop can use a single multiply-by-scalar.
    cola_norm: f32,
}

impl Stft {
    pub fn new_hann(fft_size: usize, hop_size: usize) -> Self {
        assert!(fft_size > 0, "fft_size must be > 0");
        assert!(hop_size > 0 && hop_size <= fft_size, "hop_size out of range");
        let mut planner = FftPlanner::<f32>::new();
        let forward = planner.plan_fft_forward(fft_size);
        let inverse = planner.plan_fft_inverse(fft_size);
        let window = hann_window(fft_size);
        let cola_norm = steady_state_cola(&window, hop_size);
        Self {
            fft_size,
            hop_size,
            window,
            forward,
            inverse,
            cola_norm,
        }
    }

    /// Number of FFT frames produced by an input of `input_len` samples,
    /// padded so the last input sample sits within at least one full
    /// window. Matches the loop bound used by [`Stft::process`] and
    /// [`Stft::analyze`] (positions `0, hop, 2*hop, ..., k*hop` with
    /// `k*hop <= input_len`).
    pub fn frame_count(&self, input_len: usize) -> usize {
        input_len / self.hop_size + 1
    }

    /// Analyse a mono buffer into a sequence of STFT frames. Number of
    /// frames matches [`Stft::frame_count`]; each frame is `fft_size`
    /// complex bins. Used by spectral edits, which need random access to
    /// frames adjacent to a selection (e.g. Repair).
    pub fn analyze(&self, input: &[f32]) -> Vec<Vec<Complex<f32>>> {
        let n = self.fft_size;
        let hop = self.hop_size;
        let padded_len = input.len() + n;
        let mut frames = Vec::with_capacity(self.frame_count(input.len()));
        let mut frame: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); n];
        let mut pos = 0;
        while pos + n <= padded_len {
            for i in 0..n {
                let s = if pos + i < input.len() {
                    input[pos + i]
                } else {
                    0.0
                };
                frame[i] = Complex::new(s * self.window[i], 0.0);
            }
            self.forward.process(&mut frame);
            frames.push(frame.clone());
            pos += hop;
        }
        frames
    }

    /// Synthesise a mono buffer from STFT frames via overlap-add.
    /// `output_len` is truncated to the same convention as [`Stft::process`].
    pub fn synthesize(&self, frames: &[Vec<Complex<f32>>], output_len: usize) -> Vec<f32> {
        let n = self.fft_size;
        let hop = self.hop_size;
        let padded_len = output_len + n;
        let mut output = vec![0.0_f32; padded_len];
        let mut frame_buf: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); n];
        for (idx, frame) in frames.iter().enumerate() {
            let pos = idx * hop;
            if pos + n > padded_len {
                break;
            }
            // The caller's frame may have been mutated in place; copy into
            // a scratch buffer so the inverse FFT doesn't trample shared data.
            frame_buf.copy_from_slice(frame);
            self.inverse.process(&mut frame_buf);
            for i in 0..n {
                output[pos + i] += frame_buf[i].re * self.window[i];
            }
        }
        let scale = 1.0 / (n as f32 * self.cola_norm);
        for s in output.iter_mut() {
            *s *= scale;
        }
        output.truncate(output_len);
        output
    }

    /// Centre frequency in hertz of bin `k` at `sample_rate`. Bins above
    /// `fft_size/2` are the conjugate mirror of the lower half.
    pub fn bin_hz(&self, k: usize, sample_rate: u32) -> f32 {
        k as f32 * sample_rate as f32 / self.fft_size as f32
    }

    /// Run analysis-modify-synthesis on a mono buffer. `modify` is called
    /// once per frame with the FFT bins (length `fft_size`) and may rewrite
    /// them in place. Returns a buffer the same length as `input`.
    pub fn process<F>(&self, input: &[f32], mut modify: F) -> Vec<f32>
    where
        F: FnMut(&mut [Complex<f32>]),
    {
        let n = self.fft_size;
        let hop = self.hop_size;
        // Pad past the end so the last input sample sits in at least one
        // full window. The output is truncated back to input length.
        let padded_len = input.len() + n;
        let mut output = vec![0.0_f32; padded_len];
        let mut frame: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); n];

        let mut pos = 0;
        while pos + n <= padded_len {
            // Analysis: window the (possibly padded) input into the frame.
            for i in 0..n {
                let s = if pos + i < input.len() {
                    input[pos + i]
                } else {
                    0.0
                };
                frame[i] = Complex::new(s * self.window[i], 0.0);
            }
            self.forward.process(&mut frame);
            modify(&mut frame);
            self.inverse.process(&mut frame);
            // Synthesis: window again for COLA, accumulate into output.
            for i in 0..n {
                output[pos + i] += frame[i].re * self.window[i];
            }
            pos += hop;
        }

        // rustfft is unnormalised in both directions, so the round-trip
        // multiplied magnitudes by N. Dividing by N and the COLA factor
        // (sum of overlapping squared windows) restores unity gain.
        let scale = 1.0 / (n as f32 * self.cola_norm);
        for s in output.iter_mut() {
            *s *= scale;
        }

        output.truncate(input.len());
        output
    }
}

fn hann_window(n: usize) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![1.0];
    }
    let denom = (n - 1) as f32;
    (0..n)
        .map(|k| {
            let arg = 2.0 * std::f32::consts::PI * k as f32 / denom;
            0.5 * (1.0 - arg.cos())
        })
        .collect()
}

fn steady_state_cola(window: &[f32], hop: usize) -> f32 {
    // Lay several windows down at hop intervals and read a centre sample;
    // by then the overlapping contributions have stabilised.
    let n = window.len();
    let total = 4 * n;
    let mut sum = vec![0.0_f32; total];
    let mut pos = 0;
    while pos + n <= total {
        for i in 0..n {
            sum[pos + i] += window[i] * window[i];
        }
        pos += hop;
    }
    sum[total / 2].max(1e-12)
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

    #[test]
    fn identity_round_trip_recovers_input() {
        let stft = Stft::new_hann(512, 128);
        // A 200 Hz sine over a few hundred ms is enough to cover the head
        // and tail edges of the analysis grid.
        let fs = 48_000.0_f32;
        let input: Vec<f32> = (0..6_000)
            .map(|n| (n as f32 / fs * 200.0 * std::f32::consts::TAU).sin())
            .collect();
        let output = stft.process(&input, |_bins| {});
        assert_eq!(output.len(), input.len());
        // Discard the first / last fft_size samples where the COLA hasn't
        // fully built up; the steady-state interior should match closely.
        let n = stft.fft_size;
        let interior_in = &input[n..input.len() - n];
        let interior_out = &output[n..output.len() - n];
        let err_rms = rms(
            &interior_in
                .iter()
                .zip(interior_out.iter())
                .map(|(a, b)| a - b)
                .collect::<Vec<f32>>(),
        );
        assert!(err_rms < 5e-3, "round-trip rms error {err_rms}");
    }

    #[test]
    fn zeroing_all_bins_silences_output() {
        let stft = Stft::new_hann(512, 128);
        let input: Vec<f32> = (0..2_048)
            .map(|n| (n as f32 / 48_000.0 * 1_000.0 * std::f32::consts::TAU).sin())
            .collect();
        let output = stft.process(&input, |bins| {
            for b in bins.iter_mut() {
                *b = Complex::new(0.0, 0.0);
            }
        });
        let peak = output.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!(peak < 1e-5, "expected near-silent output, got peak {peak}");
    }

    #[test]
    fn analyze_synthesize_round_trip_recovers_input() {
        let stft = Stft::new_hann(512, 128);
        let fs = 48_000.0_f32;
        let input: Vec<f32> = (0..6_000)
            .map(|n| (n as f32 / fs * 200.0 * std::f32::consts::TAU).sin())
            .collect();
        let frames = stft.analyze(&input);
        assert_eq!(frames.len(), stft.frame_count(input.len()));
        let output = stft.synthesize(&frames, input.len());
        assert_eq!(output.len(), input.len());
        let n = stft.fft_size;
        let interior_in = &input[n..input.len() - n];
        let interior_out = &output[n..output.len() - n];
        let err_rms = rms(
            &interior_in
                .iter()
                .zip(interior_out.iter())
                .map(|(a, b)| a - b)
                .collect::<Vec<f32>>(),
        );
        assert!(err_rms < 5e-3, "round-trip rms error {err_rms}");
    }

    #[test]
    fn bin_hz_matches_dft_definition() {
        let stft = Stft::new_hann(2048, 512);
        assert!((stft.bin_hz(0, 48_000) - 0.0).abs() < 1e-6);
        // Bin (fft_size / 2) is Nyquist = sample_rate / 2.
        assert!((stft.bin_hz(1024, 48_000) - 24_000.0).abs() < 1e-3);
    }

    #[test]
    fn frame_count_grows_with_input_length() {
        let stft = Stft::new_hann(1024, 256);
        // An empty input still yields the trailing-pad frames.
        assert!(stft.frame_count(0) > 0);
        assert!(stft.frame_count(48_000) > stft.frame_count(0));
    }

    #[test]
    fn high_pass_via_bin_zeroing_attenuates_low_frequency() {
        let stft = Stft::new_hann(1024, 256);
        let fs = 48_000.0_f32;
        // 200 Hz sine — well below the cut frequency.
        let input: Vec<f32> = (0..4_096)
            .map(|n| (n as f32 / fs * 200.0 * std::f32::consts::TAU).sin())
            .collect();
        let nyquist_bins = stft.fft_size / 2 + 1;
        let cut_bin =
            (1_000.0 / fs * stft.fft_size as f32) as usize; // ~21 at 1 kHz
        let output = stft.process(&input, |bins| {
            // Zero the low-frequency bins (and their conjugate mirror).
            for i in 0..cut_bin {
                bins[i] = Complex::new(0.0, 0.0);
                if i > 0 && bins.len() - i < bins.len() {
                    bins[bins.len() - i] = Complex::new(0.0, 0.0);
                }
            }
            // Sanity: nyquist_bins is bounded by fft_size.
            assert!(nyquist_bins <= bins.len());
        });
        let n = stft.fft_size;
        let in_rms = rms(&input[n..input.len() - n]);
        let out_rms = rms(&output[n..output.len() - n]);
        let attenuation_db = 20.0 * (out_rms / in_rms).log10();
        assert!(
            attenuation_db < -20.0,
            "expected strong cut, got {attenuation_db} dB (in {in_rms}, out {out_rms})"
        );
    }
}
