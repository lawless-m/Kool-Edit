//! Peak data for waveform rendering.
//!
//! Doc 03 §"Peak cache" specifies pre-computed min/max pairs at multiple
//! resolutions (1:1, 1:64, 1:4096). This module currently builds the 1:64
//! level only and downsamples on demand for the renderer; the 1:4096 level
//! and incremental updates land when the storage / flatten work does.

use crate::wav::DecodedWav;

#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct MinMax {
    pub min: f32,
    pub max: f32,
}

impl MinMax {
    pub fn from_slice(samples: &[f32]) -> Self {
        let mut min = f32::INFINITY;
        let mut max = f32::NEG_INFINITY;
        for &s in samples {
            if s < min {
                min = s;
            }
            if s > max {
                max = s;
            }
        }
        if !min.is_finite() {
            min = 0.0;
        }
        if !max.is_finite() {
            max = 0.0;
        }
        Self { min, max }
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }
}

/// First-level peak cache: one min/max pair per `samples_per_pair` frames of
/// audio. `pairs` holds the mono-summed view used by the existing renderers;
/// `channel_pairs` holds an additional one-Vec-per-channel view used by the
/// stereo waveform renderer. Mono sources have `channel_pairs.len() == 1`
/// and that inner Vec is identical to `pairs`; stereo sources have two
/// inner Vecs (L, R). The duplication is cheap (a few MB even on long
/// stereo material) and saves the renderer from re-deriving per-channel
/// peaks on every viewport change.
#[derive(Clone, Debug)]
pub struct PeakCache {
    pub samples_per_pair: u32,
    pub pairs: Vec<MinMax>,
    pub channel_pairs: Vec<Vec<MinMax>>,
}

pub const DEFAULT_DECIMATION: u32 = 64;

impl PeakCache {
    pub fn from_decoded(decoded: &DecodedWav, samples_per_pair: u32) -> Self {
        Self::from_samples(&decoded.samples, decoded.channel_count, samples_per_pair)
    }

    /// Build a peak cache directly from interleaved samples. Used after
    /// flatten, when the engine has rendered a new base buffer and needs
    /// peaks regenerated.
    pub fn from_samples(interleaved: &[f32], channels: u16, samples_per_pair: u32) -> Self {
        assert!(samples_per_pair > 0);
        let mono = mono_sum(interleaved, channels);
        let pairs = build_pairs(&mono, samples_per_pair as usize);
        let channel_pairs = build_channel_pairs(interleaved, channels, samples_per_pair as usize);
        Self {
            samples_per_pair,
            pairs,
            channel_pairs,
        }
    }

    pub fn frame_count(&self) -> u64 {
        self.pairs.len() as u64 * self.samples_per_pair as u64
    }

    pub fn channel_count(&self) -> u16 {
        self.channel_pairs.len() as u16
    }

    /// Return `columns` summarised min/max pairs spanning the entire cache.
    /// Used by the renderer: ask for one pair per pixel column. If the cache
    /// has fewer pairs than requested, the result is padded with zeros so the
    /// caller always gets a fixed-size buffer.
    pub fn summarize(&self, columns: usize) -> Vec<MinMax> {
        let frames = self.frame_count();
        self.summarize_range(0, frames, columns)
    }

    /// Range-aware summary: bin the pairs covering `[start_frame, end_frame)`
    /// into `columns` columns. Used by the zoom renderer. The boundary frames
    /// are snapped outward to the nearest pair boundary, since the cache's
    /// resolution is `samples_per_pair`; sub-pair zoom won't reveal new detail.
    pub fn summarize_range(
        &self,
        start_frame: u64,
        end_frame: u64,
        columns: usize,
    ) -> Vec<MinMax> {
        Self::summarize_pairs_range(&self.pairs, self.samples_per_pair, start_frame, end_frame, columns)
    }

    /// Per-channel range summary. Returns one Vec<MinMax> of length
    /// `columns` per channel of the cached source. Used by the stereo
    /// waveform renderer; mono sources return a single inner Vec.
    pub fn summarize_range_channels(
        &self,
        start_frame: u64,
        end_frame: u64,
        columns: usize,
    ) -> Vec<Vec<MinMax>> {
        self.channel_pairs
            .iter()
            .map(|pairs| {
                Self::summarize_pairs_range(pairs, self.samples_per_pair, start_frame, end_frame, columns)
            })
            .collect()
    }

    fn summarize_pairs_range(
        pairs: &[MinMax],
        samples_per_pair: u32,
        start_frame: u64,
        end_frame: u64,
        columns: usize,
    ) -> Vec<MinMax> {
        if columns == 0 || pairs.is_empty() || end_frame <= start_frame {
            return vec![MinMax::default(); columns];
        }
        let spp = samples_per_pair as u64;
        let total_pairs = pairs.len();
        let pair_start = (start_frame / spp) as usize;
        let pair_end_excl = end_frame.div_ceil(spp) as usize;
        let pair_start = pair_start.min(total_pairs);
        let pair_end_excl = pair_end_excl.min(total_pairs).max(pair_start);
        if pair_start == pair_end_excl {
            return vec![MinMax::default(); columns];
        }
        let span = pair_end_excl - pair_start;
        let mut out = Vec::with_capacity(columns);
        for col in 0..columns {
            let s = pair_start + col * span / columns;
            let e = (pair_start + (col + 1) * span / columns)
                .max(s + 1)
                .min(pair_end_excl);
            let bucket = &pairs[s..e];
            let merged = bucket
                .iter()
                .copied()
                .fold(MinMax::default(), MinMax::merge);
            out.push(merged);
        }
        out
    }
}

/// Build one Vec<MinMax> per channel from an interleaved sample buffer.
/// Mirrors `build_pairs` but de-interleaves first so each channel's peaks
/// are independent. Used by the stereo waveform renderer.
fn build_channel_pairs(
    interleaved: &[f32],
    channels: u16,
    samples_per_pair: usize,
) -> Vec<Vec<MinMax>> {
    if channels == 0 {
        return Vec::new();
    }
    let ch = channels as usize;
    let frames = interleaved.len() / ch.max(1);
    let mut per_channel: Vec<Vec<f32>> = (0..ch).map(|_| Vec::with_capacity(frames)).collect();
    for f in 0..frames {
        let base = f * ch;
        for c in 0..ch {
            per_channel[c].push(interleaved[base + c]);
        }
    }
    per_channel
        .iter()
        .map(|samples| build_pairs(samples, samples_per_pair))
        .collect()
}

fn mono_sum(interleaved: &[f32], channel_count: u16) -> Vec<f32> {
    if channel_count == 1 {
        return interleaved.to_vec();
    }
    let ch = channel_count as usize;
    let frames = interleaved.len() / ch;
    let mut mono = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * ch;
        let mut sum = 0.0;
        for c in 0..ch {
            sum += interleaved[base + c];
        }
        mono.push(sum / ch as f32);
    }
    mono
}

/// Bin raw interleaved samples into `columns` min/max pairs after mono-summing.
/// Used for the zoom path when the requested resolution is finer than the
/// peak cache's decimation. `samples` is `frames * channels` long.
pub fn bin_raw_samples(samples: &[f32], channels: u16, columns: usize) -> Vec<MinMax> {
    if columns == 0 {
        return Vec::new();
    }
    let mono = mono_sum(samples, channels);
    if mono.is_empty() {
        return vec![MinMax::default(); columns];
    }
    let total = mono.len();
    let mut out = Vec::with_capacity(columns);
    for col in 0..columns {
        let s = col * total / columns;
        let e = ((col + 1) * total / columns).max(s + 1).min(total);
        out.push(MinMax::from_slice(&mono[s..e]));
    }
    out
}

/// Per-channel variant of `bin_raw_samples`. Returns one Vec<MinMax> of
/// length `columns` per channel; mono input yields a single inner Vec.
pub fn bin_raw_samples_channels(
    samples: &[f32],
    channels: u16,
    columns: usize,
) -> Vec<Vec<MinMax>> {
    if columns == 0 || channels == 0 {
        return Vec::new();
    }
    let ch = channels as usize;
    let frames = samples.len() / ch.max(1);
    if frames == 0 {
        return (0..ch).map(|_| vec![MinMax::default(); columns]).collect();
    }
    let mut out: Vec<Vec<MinMax>> = (0..ch).map(|_| Vec::with_capacity(columns)).collect();
    let mut scratch: Vec<f32> = Vec::with_capacity(frames / columns + 1);
    for c in 0..ch {
        for col in 0..columns {
            let s = col * frames / columns;
            let e = ((col + 1) * frames / columns).max(s + 1).min(frames);
            scratch.clear();
            for f in s..e {
                scratch.push(samples[f * ch + c]);
            }
            out[c].push(MinMax::from_slice(&scratch));
        }
    }
    out
}

fn build_pairs(mono: &[f32], samples_per_pair: usize) -> Vec<MinMax> {
    if mono.is_empty() {
        return Vec::new();
    }
    let n = mono.len().div_ceil(samples_per_pair);
    let mut pairs = Vec::with_capacity(n);
    for i in 0..n {
        let start = i * samples_per_pair;
        let end = (start + samples_per_pair).min(mono.len());
        pairs.push(MinMax::from_slice(&mono[start..end]));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded_mono(samples: Vec<f32>, sample_rate: u32) -> DecodedWav {
        let frames = samples.len() as u64;
        DecodedWav {
            channel_count: 1,
            sample_rate,
            samples,
            frames,
        }
    }

    #[test]
    fn min_max_of_a_known_signal() {
        let mm = MinMax::from_slice(&[0.1, -0.3, 0.5, -0.2]);
        assert!((mm.min + 0.3).abs() < 1e-6);
        assert!((mm.max - 0.5).abs() < 1e-6);
    }

    #[test]
    fn empty_slice_yields_zero_pair() {
        let mm = MinMax::from_slice(&[]);
        assert_eq!(mm.min, 0.0);
        assert_eq!(mm.max, 0.0);
    }

    #[test]
    fn cache_pair_count_rounds_up() {
        let decoded = decoded_mono(vec![0.0; 130], 44_100);
        let cache = PeakCache::from_decoded(&decoded, 64);
        assert_eq!(cache.pairs.len(), 3);
        assert_eq!(cache.samples_per_pair, 64);
    }

    #[test]
    fn cache_captures_extremes() {
        let mut samples = vec![0.0; 128];
        samples[10] = 0.9;
        samples[80] = -0.7;
        let decoded = decoded_mono(samples, 44_100);
        let cache = PeakCache::from_decoded(&decoded, 64);
        assert!((cache.pairs[0].max - 0.9).abs() < 1e-6);
        assert!((cache.pairs[1].min + 0.7).abs() < 1e-6);
    }

    #[test]
    fn summarize_returns_exactly_n_columns() {
        let decoded = decoded_mono((0..640).map(|i| i as f32 / 640.0).collect(), 44_100);
        let cache = PeakCache::from_decoded(&decoded, 64);
        assert_eq!(cache.summarize(20).len(), 20);
        assert_eq!(cache.summarize(1).len(), 1);
    }

    #[test]
    fn summarize_preserves_global_extremes() {
        let mut samples = vec![0.0; 1024];
        samples[100] = 1.0;
        samples[900] = -1.0;
        let decoded = decoded_mono(samples, 44_100);
        let cache = PeakCache::from_decoded(&decoded, 64);
        let summary = cache.summarize(8);
        let global_max = summary.iter().map(|p| p.max).fold(f32::NEG_INFINITY, f32::max);
        let global_min = summary.iter().map(|p| p.min).fold(f32::INFINITY, f32::min);
        assert!((global_max - 1.0).abs() < 1e-6);
        assert!((global_min + 1.0).abs() < 1e-6);
    }

    #[test]
    fn stereo_input_is_summed_to_mono() {
        // L = +1, R = -1 → mono = 0
        let mut interleaved = Vec::with_capacity(128);
        for _ in 0..64 {
            interleaved.push(1.0);
            interleaved.push(-1.0);
        }
        let mono = mono_sum(&interleaved, 2);
        assert_eq!(mono.len(), 64);
        assert!(mono.iter().all(|&s| s.abs() < 1e-6));
    }
}
