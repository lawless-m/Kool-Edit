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

use rustfft::{FftPlanner, num_complex::Complex};

use crate::effect::{
    AutotuneParams, AutotuneScale, AutotuneTarget, ChorusParams, CompParams, DelayParams,
    DistortionParams, EqBand, EqBandKind, EqParams, LimitParams, ReverbModel, ReverbParams,
};
use crate::op::{
    FadeDirection, FadeShape, GeneratorParams, NoiseColor, NormTarget, Op, ToneShape,
};
use crate::range::SampleRange;
use crate::stft::Stft;

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
        Op::Trim { range } => trim(buffer, channels, *range),
        Op::Normalize {
            range,
            target,
            value_db,
        } => normalize(buffer, channels, *range, *target, *value_db),
        Op::Generate { at, length, params } => {
            generate(buffer, channels, sample_rate, *at, *length, params)
        }
        Op::Delay { range, params } => {
            delay(buffer, channels, *range, params, sample_rate)
        }
        Op::Distortion { range, params } => {
            distortion(buffer, channels, *range, params, sample_rate)
        }
        Op::Chorus { range, params } => {
            chorus(buffer, channels, *range, params, sample_rate)
        }
        Op::Compress { range, params } => {
            compress(buffer, channels, *range, params, sample_rate)
        }
        Op::Limit { range, params } => limit(buffer, channels, *range, params, sample_rate),
        Op::Eq { range, params } => eq(buffer, channels, *range, params, sample_rate),
        Op::Reverb { range, params } => reverb(buffer, channels, *range, params, sample_rate),

        Op::TimeStretch { range, ratio } => time_stretch(buffer, channels, *range, *ratio),
        Op::PitchShift { range, cents } => pitch_shift(buffer, channels, *range, *cents),
        Op::Autotune { range, params } => {
            autotune(buffer, channels, *range, params, sample_rate)
        }

        Op::Insert { .. } => Err(DspError::Unsupported("Insert")),
        Op::PasteMix { .. } => Err(DspError::Unsupported("PasteMix")),
        Op::PasteOver { .. } => Err(DspError::Unsupported("PasteOver")),
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

/// Keep only `range`, discarding everything outside. Drains the tail
/// first so the head's indices stay valid for the second drain.
fn trim(buffer: &mut Vec<f32>, channels: u16, range: SampleRange) -> Result<(), DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    let end = (range.end() * ch) as usize;
    buffer.truncate(end);
    let start = (range.start() * ch) as usize;
    buffer.drain(0..start);
    Ok(())
}

/// WSOLA (Waveform Similarity-based Overlap-Add) time-stretch over `range`.
/// `ratio > 1` makes the segment longer (slower), `< 1` shorter (faster).
/// Pitch is preserved (within the limits of the algorithm). The buffer is
/// resized: the stretched segment replaces the original in place.
///
/// Algorithm: for each output frame, search a small window of the input
/// around its "ideal" analysis position for the best match against the
/// natural-progression reference from the previous frame, then Hann-window
/// it and overlap-add at 50 % into the output. With Hann + 50 % overlap the
/// COLA condition holds, so no normalisation is needed.
fn time_stretch(
    buffer: &mut Vec<f32>,
    channels: u16,
    range: SampleRange,
    ratio: f32,
) -> Result<(), DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    if !(ratio > 0.05 && ratio < 20.0) {
        return Err(DspError::Unsupported("TimeStretch ratio out of range"));
    }
    let in_frames = (range.end() - range.start()) as usize;
    if in_frames == 0 {
        return Err(DspError::EmptySignal);
    }
    let stretched = wsola_stretch(
        &buffer[(range.start() * ch) as usize..(range.end() * ch) as usize],
        channels,
        ratio,
    );
    let start = (range.start() * ch) as usize;
    let end = (range.end() * ch) as usize;
    buffer.splice(start..end, stretched);
    Ok(())
}

/// Pitch-shift over `range` by `cents`. Length is preserved. Built from
/// time-stretch (changes length, preserves pitch) followed by linear
/// resampling (changes length, changes pitch). The two combine to leave the
/// length unchanged while pitch moves by `2^(cents/1200)`.
fn pitch_shift(
    buffer: &mut Vec<f32>,
    channels: u16,
    range: SampleRange,
    cents: f32,
) -> Result<(), DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    if !(-2400.0..=2400.0).contains(&cents) {
        return Err(DspError::Unsupported("PitchShift cents out of range"));
    }
    let in_frames = (range.end() - range.start()) as usize;
    if in_frames == 0 {
        return Err(DspError::EmptySignal);
    }
    let ratio = 2.0_f32.powf(cents / 1200.0);
    let stretched = wsola_stretch(
        &buffer[(range.start() * ch) as usize..(range.end() * ch) as usize],
        channels,
        ratio,
    );
    // Resample stretched (length ≈ in_frames * ratio) back to in_frames so
    // the segment length is unchanged. resample_by_factor expects an
    // interleaved buffer and a "src/dst" frame ratio.
    let resampled = resample_to_target_frames(&stretched, channels, in_frames);
    let start = (range.start() * ch) as usize;
    let end = (range.end() * ch) as usize;
    buffer.splice(start..end, resampled);
    Ok(())
}

/// Resample `interleaved` (length `src_frames * channels`) so the output has
/// exactly `dst_frames` frames. Linear interpolation, same as the import-time
/// resampler in `engine.rs` but parameterised by frame count rather than
/// sample rate.
fn resample_to_target_frames(interleaved: &[f32], channels: u16, dst_frames: usize) -> Vec<f32> {
    let ch = channels as usize;
    let src_frames = interleaved.len() / ch;
    if src_frames == 0 || dst_frames == 0 {
        return Vec::new();
    }
    if src_frames == dst_frames {
        return interleaved.to_vec();
    }
    let mut out = vec![0.0_f32; dst_frames * ch];
    let last = src_frames - 1;
    let step = src_frames as f64 / dst_frames as f64;
    for i in 0..dst_frames {
        let src_pos = i as f64 * step;
        let i0 = src_pos.floor() as usize;
        let frac = (src_pos - i0 as f64) as f32;
        let i1 = (i0 + 1).min(last);
        for c in 0..ch {
            let a = interleaved[i0 * ch + c];
            let b = interleaved[i1 * ch + c];
            out[i * ch + c] = a + (b - a) * frac;
        }
    }
    out
}

/// Core WSOLA worker. Operates on interleaved input, returns interleaved
/// output. Window = 1024 frames, synthesis hop = 512 (50 % Hann COLA).
/// Search tolerance bounded so the cost stays linear in the input length
/// for typical ratios.
fn wsola_stretch(input: &[f32], channels: u16, ratio: f32) -> Vec<f32> {
    const W: usize = 1024;
    const HS: usize = W / 2;
    const T: usize = 256;

    let ch = channels as usize;
    let in_frames = input.len() / ch;
    let out_frames = ((in_frames as f32) * ratio).round().max(0.0) as usize;
    if out_frames == 0 || in_frames == 0 {
        return Vec::new();
    }

    // Short signals fall back to plain resampling — there's no room for the
    // sliding-window machinery, and the result is the same anyway because
    // there are no transients to preserve.
    if in_frames < W * 2 {
        return resample_to_target_frames(input, channels, out_frames);
    }

    // Pre-compute a Hann window once.
    let hann: Vec<f32> = (0..W)
        .map(|n| 0.5 - 0.5 * ((2.0 * PI * n as f32) / (W as f32 - 1.0)).cos())
        .collect();

    // Mono reference for the cross-correlation search. For multi-channel
    // input we sum the channels; using a single offset for every channel
    // keeps the stereo image intact.
    let mono: Vec<f32> = if ch == 1 {
        input.to_vec()
    } else {
        let mut m = Vec::with_capacity(in_frames);
        for f in 0..in_frames {
            let mut s = 0.0;
            for c in 0..ch {
                s += input[f * ch + c];
            }
            m.push(s / ch as f32);
        }
        m
    };

    let analysis_hop = (HS as f32) / ratio.max(1e-3);
    let max_search = in_frames.saturating_sub(W);

    let mut output = vec![0.0_f32; out_frames * ch];
    let mut prev_match: usize = 0;
    let mut k: usize = 0;
    loop {
        let synth_pos = k * HS;
        if synth_pos + W > out_frames {
            break;
        }
        let ideal_in = (k as f32 * analysis_hop) as usize;
        let best = if k == 0 {
            ideal_in.min(max_search)
        } else {
            // Reference is where the previous match would land if we
            // followed the natural progression of one synthesis hop.
            let ref_pos = (prev_match + HS).min(max_search);
            let lo = ideal_in.saturating_sub(T);
            let hi = (ideal_in + T).min(max_search);
            if hi <= lo {
                lo
            } else {
                let mut best_corr = f32::NEG_INFINITY;
                let mut best_off = lo;
                for off in lo..=hi {
                    let mut corr = 0.0_f32;
                    // Stride through the window in steps of 4 — this is just
                    // a similarity heuristic, full resolution isn't needed
                    // and the speedup is significant.
                    let mut i = 0;
                    while i < W {
                        corr += mono[off + i] * mono[ref_pos + i];
                        i += 4;
                    }
                    if corr > best_corr {
                        best_corr = corr;
                        best_off = off;
                    }
                }
                best_off
            }
        };

        if best + W > in_frames {
            break;
        }

        for i in 0..W {
            let dst = synth_pos + i;
            if dst >= out_frames {
                break;
            }
            let w = hann[i];
            for c in 0..ch {
                output[dst * ch + c] += input[(best + i) * ch + c] * w;
            }
        }
        prev_match = best;
        k += 1;
    }
    let _ = k;

    // OLA leaves a Hann ramp-up on the first HS frames and a ramp-down on
    // the last HS frames. That's a natural fade and doesn't click. Anything
    // past the last write stays zero.
    output
}

/// YIN pitch detector. Returns the dominant fundamental frequency in Hz, or
/// `None` for windows that don't have a clear pitched signal (silence,
/// noise, polyphonic material). Search range is bounded to 80–1000 Hz which
/// covers the human voice; calling code can decide what to do with `None`.
pub(crate) fn yin_pitch(samples: &[f32], sample_rate: u32) -> Option<f32> {
    const MIN_HZ: f32 = 80.0;
    const MAX_HZ: f32 = 1000.0;
    const THRESHOLD: f32 = 0.15;

    let n = samples.len();
    if n < 64 {
        return None;
    }
    let min_tau = ((sample_rate as f32 / MAX_HZ).floor() as usize).max(2);
    let max_tau = ((sample_rate as f32 / MIN_HZ).ceil() as usize).min(n / 2);
    if max_tau <= min_tau + 1 {
        return None;
    }

    // Difference function d(tau) = Σ (x[j] - x[j+tau])^2 over all valid j.
    let mut diff = vec![0.0_f32; max_tau + 1];
    for tau in 1..=max_tau {
        let mut sum = 0.0_f32;
        let count = n - tau;
        for j in 0..count {
            let delta = samples[j] - samples[j + tau];
            sum += delta * delta;
        }
        diff[tau] = sum;
    }

    // Cumulative-mean-normalised difference. cmnd[0] is unused.
    let mut cmnd = vec![1.0_f32; max_tau + 1];
    let mut running = 0.0_f32;
    for tau in 1..=max_tau {
        running += diff[tau];
        cmnd[tau] = if running > 0.0 {
            diff[tau] * (tau as f32) / running
        } else {
            1.0
        };
    }

    // Absolute-threshold pass: walk forward, find the first tau whose CMND
    // dips below the threshold, then descend into that dip's local minimum.
    let mut found_tau: Option<usize> = None;
    let mut tau = min_tau;
    while tau < max_tau {
        if cmnd[tau] < THRESHOLD {
            while tau + 1 < max_tau && cmnd[tau + 1] < cmnd[tau] {
                tau += 1;
            }
            found_tau = Some(tau);
            break;
        }
        tau += 1;
    }
    let tau = found_tau?;

    // Parabolic interpolation around the discrete minimum.
    let better_tau = if tau > min_tau && tau + 1 < max_tau {
        let s0 = cmnd[tau - 1];
        let s1 = cmnd[tau];
        let s2 = cmnd[tau + 1];
        let denom = 2.0 * (s0 - 2.0 * s1 + s2);
        if denom.abs() > 1e-9 {
            tau as f32 + (s0 - s2) / denom
        } else {
            tau as f32
        }
    } else {
        tau as f32
    };

    if better_tau > 1.0 {
        Some(sample_rate as f32 / better_tau)
    } else {
        None
    }
}

/// Build a pitch contour over `samples` (mono). Returns one f32 per
/// `hop_samples` step, where each value is the YIN-detected fundamental
/// (Hz) of a `window_samples`-long block centred on that step (or 0.0 if
/// no pitch was detected). Used by the arranger when wiring up Autotune in
/// Reference mode.
#[cfg_attr(not(feature = "wasm"), allow(dead_code))]
pub(crate) fn pitch_contour(
    samples: &[f32],
    sample_rate: u32,
    hop_samples: usize,
    window_samples: usize,
) -> Vec<f32> {
    if samples.is_empty() || hop_samples == 0 || window_samples == 0 {
        return Vec::new();
    }
    let n = samples.len();
    let num_hops = n.div_ceil(hop_samples);
    let mut out = Vec::with_capacity(num_hops);
    let half = window_samples / 2;
    for h in 0..num_hops {
        let centre = h * hop_samples;
        let lo = centre.saturating_sub(half);
        let hi = (centre + half).min(n);
        if hi <= lo + 64 {
            out.push(0.0);
            continue;
        }
        let f = yin_pitch(&samples[lo..hi], sample_rate).unwrap_or(0.0);
        out.push(f);
    }
    out
}

fn hz_to_midi(hz: f32) -> f32 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

fn midi_to_hz(midi: f32) -> f32 {
    440.0 * 2.0_f32.powf((midi - 69.0) / 12.0)
}

const MAJOR_DEGREES: [u8; 7] = [0, 2, 4, 5, 7, 9, 11];
const NATURAL_MINOR_DEGREES: [u8; 7] = [0, 2, 3, 5, 7, 8, 10];

/// Snap `hz` to the nearest note allowed by `scale` rooted at `key_pc`
/// (0=C..11=B). Chromatic snaps to the nearest semitone; Major/Minor only
/// allow the seven scale degrees relative to the key.
fn snap_to_scale(hz: f32, scale: AutotuneScale, key_pc: u8) -> f32 {
    if hz <= 0.0 || !hz.is_finite() {
        return hz;
    }
    let midi = hz_to_midi(hz);
    let target_midi = match scale {
        AutotuneScale::Chromatic => midi.round(),
        AutotuneScale::Major => snap_to_degrees(midi, key_pc, &MAJOR_DEGREES),
        AutotuneScale::Minor => snap_to_degrees(midi, key_pc, &NATURAL_MINOR_DEGREES),
    };
    midi_to_hz(target_midi)
}

fn snap_to_degrees(midi: f32, key_pc: u8, degrees: &[u8]) -> f32 {
    // For each scale degree, find the nearest absolute MIDI note that
    // matches that degree relative to the key. Then pick whichever of those
    // candidates is closest to the input.
    let mut best = midi.round();
    let mut best_dist = f32::INFINITY;
    let key = key_pc as f32;
    for &d in degrees {
        let degree_pc = (key + d as f32) % 12.0;
        // Find the octave whose MIDI value with this pitch class is nearest.
        let raw = midi - degree_pc;
        let nearest_octave = (raw / 12.0).round();
        let candidate = nearest_octave * 12.0 + degree_pc;
        let dist = (candidate - midi).abs();
        if dist < best_dist {
            best_dist = dist;
            best = candidate;
        }
    }
    best
}

/// Autotune entry point used by `apply`. Dispatches on
/// `params.preserve_formants`: the WSOLA path is the chipmunky-but-simple
/// "T-Pain" sound; the spectral path runs a phase vocoder with cepstral
/// envelope preservation so formants stay in place.
fn autotune(
    buffer: &mut Vec<f32>,
    channels: u16,
    range: SampleRange,
    params: &AutotuneParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    if params.preserve_formants {
        autotune_spectral(buffer, channels, range, params, sample_rate)
    } else {
        autotune_wsola(buffer, channels, range, params, sample_rate)
    }
}

/// WSOLA-based autotune. Pitch-shifts each analysis window in time-domain
/// using the existing WSOLA + linear-resample pitch_shift helper, then
/// overlap-adds the corrected windows. Cheap and works for moderate
/// shifts; the shift itself doesn't preserve formants so this is the
/// chipmunk / T-Pain sound at large ratios.
///
/// Algorithm sketch:
/// 1. Window the input range with 50 ms Hann windows at 50 % overlap.
/// 2. Per window, run YIN to detect f₀.
/// 3. Pick a target f₀ (scale snap, or look up the reference contour).
/// 4. Smooth the per-window pitch ratio toward target with a one-pole IIR
///    whose time constant is `retune_ms`. Zero ms → instant snap (T-Pain
///    style); larger values → glide.
/// 5. Pitch-shift the window by the smoothed ratio (reusing WSOLA + linear
///    resampling), then OLA back into the output, dividing by the summed
///    window weights so unity output is preserved.
/// 6. Splice the autotuned region into `buffer`.
fn autotune_wsola(
    buffer: &mut Vec<f32>,
    channels: u16,
    range: SampleRange,
    params: &AutotuneParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    let in_frames = (range.end() - range.start()) as usize;
    if in_frames == 0 {
        return Err(DspError::EmptySignal);
    }
    let ch_us = channels as usize;

    // 50 ms windows at 50 % overlap — short enough to track pitch changes,
    // long enough for YIN to lock on at low fundamentals.
    let window_size = (((sample_rate as f32) * 0.05).round() as usize).max(256);
    let hop = window_size / 2;
    if in_frames < window_size {
        return Err(DspError::EmptySignal);
    }

    let start = (range.start() * ch) as usize;
    let end = (range.end() * ch) as usize;
    let input: Vec<f32> = buffer[start..end].to_vec();

    // Mono mix is the YIN reference. We apply one ratio per window to every
    // channel — same as our other multichannel DSP — to keep the stereo
    // image intact.
    let mono: Vec<f32> = if ch_us == 1 {
        input.clone()
    } else {
        let mut m = Vec::with_capacity(in_frames);
        for f in 0..in_frames {
            let mut s = 0.0_f32;
            for c in 0..ch_us {
                s += input[f * ch_us + c];
            }
            m.push(s / ch_us as f32);
        }
        m
    };

    // One-pole smoothing toward the target ratio. retune_ms = 0 → α = 0
    // (instant). Otherwise α = exp(-hop_ms / retune_ms) so 63 % is reached
    // after one retune-time-constant of audio.
    let hop_ms = (hop as f32) / (sample_rate as f32) * 1000.0;
    let alpha = if params.retune_ms <= 0.0 {
        0.0
    } else {
        (-hop_ms / params.retune_ms).exp()
    };
    let mut smoothed_ratio = 1.0_f32;

    let mut output = vec![0.0_f32; input.len()];
    let mut weight = vec![0.0_f32; in_frames];
    let hann_window: Vec<f32> = (0..window_size)
        .map(|n| 0.5 - 0.5 * ((2.0 * PI * n as f32) / (window_size as f32 - 1.0)).cos())
        .collect();

    let mut seg_start = 0_usize;
    while seg_start + window_size <= in_frames {
        let mono_seg = &mono[seg_start..seg_start + window_size];

        let detected = yin_pitch(mono_seg, sample_rate);
        let target_hz = match (&params.target, detected) {
            (AutotuneTarget::Scale { scale, key_pc }, Some(d)) => {
                Some(snap_to_scale(d, *scale, *key_pc))
            }
            (
                AutotuneTarget::Reference {
                    contour_hz,
                    hop_samples,
                },
                _,
            ) => {
                if *hop_samples == 0 {
                    None
                } else {
                    let centre = (seg_start + window_size / 2) as u64;
                    let idx = (centre / *hop_samples) as usize;
                    contour_hz.get(idx).copied().filter(|h| *h > 0.0)
                }
            }
            _ => None,
        };

        let target_ratio = match (detected, target_hz) {
            (Some(d), Some(t)) if d > 0.0 => (t / d).clamp(0.25, 4.0),
            _ => 1.0,
        };
        // IIR smoother. With α=0 this collapses to "follow the target
        // exactly" — the T-Pain instant-snap behaviour.
        smoothed_ratio = smoothed_ratio * alpha + target_ratio * (1.0 - alpha);

        // Build a working buffer for this window and pitch-shift in place.
        let mut window_buf: Vec<f32> =
            input[seg_start * ch_us..(seg_start + window_size) * ch_us].to_vec();
        if (smoothed_ratio - 1.0).abs() > 1e-3 {
            let cents = 1200.0 * smoothed_ratio.log2();
            let _ = pitch_shift(
                &mut window_buf,
                channels,
                SampleRange::new(0, window_size as u64).expect("non-empty window"),
                cents,
            );
            // pitch_shift preserves length, so window_buf still has the same
            // frame count we expect. Fall through and OLA.
        }

        for i in 0..window_size {
            let w = hann_window[i];
            weight[seg_start + i] += w;
            for c in 0..ch_us {
                output[(seg_start + i) * ch_us + c] += window_buf[i * ch_us + c] * w;
            }
        }

        seg_start += hop;
    }

    // Normalise OLA: with Hann + 50 % overlap the body sums to ~1.0 but the
    // first/last hop have only one window, so dividing by the per-frame
    // weight gives unity gain everywhere a window contributed. Frames that
    // never got a window (the trailing partial hop) stay at zero.
    for f in 0..in_frames {
        if weight[f] > 1e-3 {
            for c in 0..ch_us {
                output[f * ch_us + c] /= weight[f];
            }
        }
    }

    buffer.splice(start..end, output);
    Ok(())
}

/// Phase-vocoder autotune with cepstral envelope preservation.
///
/// For each STFT frame:
/// 1. Get magnitude/phase from the FFT bins.
/// 2. Compute the cepstrum, lifter the low-quefrency coefficients, and
///    re-FFT to get the spectral *envelope* (the slow shape, formants).
/// 3. Gather-style bin remap: for each output bin `k_out`, find source
///    bin `k_in = round(k_out / ratio)`, copy the magnitude and advance
///    the output's running phase by `ratio * true_freq * hop`. This is
///    the standard phase-vocoder pitch shift.
/// 4. While remapping, swap envelopes — multiply the source bin's
///    magnitude by `envelope[k_out] / envelope[k_in]` so the formant
///    structure stays at its original Hz rather than moving with the
///    pitch.
/// 5. Conjugate-mirror to fill the second half of the spectrum, then
///    let the STFT module overlap-add the synthesised frames.
fn autotune_spectral(
    buffer: &mut Vec<f32>,
    channels: u16,
    range: SampleRange,
    params: &AutotuneParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let ch = channels as u64;
    let total_frames = buffer.len() as u64 / ch;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    let in_frames = (range.end() - range.start()) as usize;
    if in_frames == 0 {
        return Err(DspError::EmptySignal);
    }
    let ch_us = channels as usize;

    let start = (range.start() * ch) as usize;
    let end = (range.end() * ch) as usize;
    let input: Vec<f32> = buffer[start..end].to_vec();

    let mono: Vec<f32> = if ch_us == 1 {
        input.clone()
    } else {
        let mut m = Vec::with_capacity(in_frames);
        for f in 0..in_frames {
            let mut s = 0.0_f32;
            for c in 0..ch_us {
                s += input[f * ch_us + c];
            }
            m.push(s / ch_us as f32);
        }
        m
    };

    // 2048-point FFT with 75 % overlap — the standard phase-vocoder grid.
    // 43 ms windows at 48 kHz, hop 10.7 ms.
    const FFT_SIZE: usize = 2048;
    const HOP: usize = 512;
    let half = FFT_SIZE / 2;
    let stft = Stft::new_hann(FFT_SIZE, HOP);
    let frame_count = stft.frame_count(in_frames);

    // Pre-compute per-frame target ratios via mono YIN. Smoothed by a
    // one-pole IIR keyed off retune_ms.
    let yin_window = (((sample_rate as f32) * 0.05).round() as usize).max(256);
    let hop_ms = (HOP as f32) / sample_rate as f32 * 1000.0;
    let alpha = if params.retune_ms <= 0.0 {
        0.0
    } else {
        (-hop_ms / params.retune_ms).exp()
    };
    let mut ratios = vec![1.0_f32; frame_count];
    let mut smoothed = 1.0_f32;
    for f in 0..frame_count {
        let centre = f * HOP;
        let half_yin = yin_window / 2;
        let lo = centre.saturating_sub(half_yin);
        let hi = (centre + half_yin).min(mono.len());
        let detected = if hi > lo + 64 {
            yin_pitch(&mono[lo..hi], sample_rate)
        } else {
            None
        };
        let target_hz = match (&params.target, detected) {
            (AutotuneTarget::Scale { scale, key_pc }, Some(d)) => {
                Some(snap_to_scale(d, *scale, *key_pc))
            }
            (
                AutotuneTarget::Reference {
                    contour_hz,
                    hop_samples,
                },
                _,
            ) => {
                if *hop_samples == 0 {
                    None
                } else {
                    let idx = (centre as u64 / *hop_samples) as usize;
                    contour_hz.get(idx).copied().filter(|h| *h > 0.0)
                }
            }
            _ => None,
        };
        let raw = match (detected, target_hz) {
            (Some(d), Some(t)) if d > 0.0 => (t / d).clamp(0.5, 2.0),
            _ => 1.0,
        };
        smoothed = smoothed * alpha + raw * (1.0 - alpha);
        ratios[f] = smoothed;
    }

    // Cepstrum FFT planner — the STFT module's planners aren't exposed,
    // so we keep our own. Cheap to construct; rustfft caches twiddles.
    let mut planner = FftPlanner::<f32>::new();
    let cep_fwd = planner.plan_fft_forward(FFT_SIZE);
    let cep_inv = planner.plan_fft_inverse(FFT_SIZE);

    let expected_advance = 2.0 * PI * HOP as f32 / FFT_SIZE as f32;
    let mut output = vec![0.0_f32; input.len()];

    for c in 0..ch_us {
        let chan_input: Vec<f32> = (0..in_frames).map(|f| input[f * ch_us + c]).collect();

        let mut prev_phase = vec![0.0_f32; FFT_SIZE];
        let mut out_phase = vec![0.0_f32; FFT_SIZE];
        let mut cep = vec![Complex::<f32>::new(0.0, 0.0); FFT_SIZE];
        let mut new_bins = vec![Complex::<f32>::new(0.0, 0.0); FFT_SIZE];
        let mut frame_idx = 0_usize;

        let chan_out = stft.process(&chan_input, |bins| {
            let ratio = ratios.get(frame_idx).copied().unwrap_or(1.0);

            let mut mag = vec![0.0_f32; FFT_SIZE];
            let mut phase = vec![0.0_f32; FFT_SIZE];
            for k in 0..FFT_SIZE {
                mag[k] = bins[k].norm();
                phase[k] = bins[k].arg();
            }

            // Cepstral spectral envelope: log|X|, IFFT, lifter (zero high
            // quefrency), FFT back, exp. The cutoff of 30 keeps just the
            // formant-scale shape and discards the harmonic detail. Mirror
            // the lifter on both ends because the cepstrum is symmetric.
            for k in 0..FFT_SIZE {
                cep[k] = Complex::new((mag[k] + 1e-9).ln(), 0.0);
            }
            cep_inv.process(&mut cep);
            let cutoff = 30usize;
            if FFT_SIZE > 2 * cutoff {
                for k in cutoff..(FFT_SIZE - cutoff) {
                    cep[k] = Complex::new(0.0, 0.0);
                }
            }
            cep_fwd.process(&mut cep);
            let envelope: Vec<f32> = cep
                .iter()
                .map(|c| (c.re / FFT_SIZE as f32).exp().max(1e-9))
                .collect();

            // Build the new spectrum by *scattering* input bins to their
            // round-mapped output bins. Scatter avoids the gather-mode
            // aliasing that happens when an output bin's phase advance
            // differs from its nominal frequency by more than ±π/H — that
            // distance is fs/(2H), and any output bin further from the
            // best source gets phase-wrapped, leaking energy at fs/H from
            // the target. With scatter, each input bin writes to exactly
            // the output bin closest to ratio·input frequency.
            for nb in new_bins.iter_mut() {
                *nb = Complex::new(0.0, 0.0);
            }
            let mut phase_set_this_frame = vec![false; FFT_SIZE];
            for k_in in 1..=half {
                let k_out = ((k_in as f32) * ratio).round() as isize;
                if k_out < 1 || (k_out as usize) > half {
                    continue;
                }
                let k_out = k_out as usize;

                // Phase-vocoder true frequency at the source bin.
                let pd = phase[k_in] - prev_phase[k_in] - (k_in as f32) * expected_advance;
                let wrapped = pd - (pd / (2.0 * PI)).round() * 2.0 * PI;
                let advance_per_hop = (k_in as f32) * expected_advance + wrapped;

                // Each output bin's phase advances exactly once per frame
                // (otherwise colliding input bins would over-advance).
                if !phase_set_this_frame[k_out] {
                    out_phase[k_out] += ratio * advance_per_hop;
                    phase_set_this_frame[k_out] = true;
                }

                // Envelope swap: input bin's mag normalised by the source
                // envelope, re-multiplied by the *output* bin's envelope.
                // Formants stay put at their original Hz.
                let scaled = mag[k_in] * envelope[k_out] / envelope[k_in];
                new_bins[k_out] += Complex::from_polar(scaled, out_phase[k_out]);
            }
            new_bins[0] = Complex::new(0.0, 0.0);
            // Conjugate symmetry for a real-valued IFFT.
            for k in 1..half {
                new_bins[FFT_SIZE - k] = new_bins[k].conj();
            }
            // Nyquist bin must be real.
            new_bins[half] = Complex::new(new_bins[half].re, 0.0);

            bins.copy_from_slice(&new_bins);
            prev_phase.copy_from_slice(&phase);
            frame_idx += 1;
        });

        for f in 0..in_frames.min(chan_out.len()) {
            output[f * ch_us + c] = chan_out[f];
        }
    }

    buffer.splice(start..end, output);
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

fn eq(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    params: &EqParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let s = slice_for(buffer, channels, range)?;
    let ch = channels as usize;
    let frames = s.len() / ch;
    if frames == 0 {
        return Ok(());
    }
    // Build coefficients once for every enabled band; disabled bands are
    // simply skipped in the chain so they have no effect on the signal.
    let coefs: Vec<BiquadCoefs> = params
        .bands
        .iter()
        .filter(|b| b.enabled)
        .map(|b| BiquadCoefs::for_band(b, sample_rate as f32))
        .collect();
    if coefs.is_empty() {
        return Ok(());
    }
    // One filter chain per channel so the biquad state doesn't bleed between
    // channels of a stereo source.
    let mut chains: Vec<Vec<Biquad>> = (0..ch)
        .map(|_| {
            coefs
                .iter()
                .copied()
                .map(|c| Biquad {
                    coefs: c,
                    z1: 0.0,
                    z2: 0.0,
                })
                .collect()
        })
        .collect();
    for f in 0..frames {
        for c in 0..ch {
            let mut x = s[f * ch + c];
            for biquad in &mut chains[c] {
                x = biquad.process(x);
            }
            s[f * ch + c] = x;
        }
    }
    Ok(())
}

/// Direct-form-II-transposed biquad. State (`z1`, `z2`) is per-instance so a
/// stereo chain holds two of these per band.
#[derive(Copy, Clone, Debug)]
struct Biquad {
    coefs: BiquadCoefs,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn process(&mut self, x: f32) -> f32 {
        let y = self.coefs.b0 * x + self.z1;
        self.z1 = self.coefs.b1 * x - self.coefs.a1 * y + self.z2;
        self.z2 = self.coefs.b2 * x - self.coefs.a2 * y;
        y
    }
}

/// Normalised biquad coefficients (a0 already divided out). Computed via the
/// RBJ Audio EQ Cookbook formulas, then divided through by `a0` so the
/// runtime path is just five multiplies and adds per sample.
#[derive(Copy, Clone, Debug)]
struct BiquadCoefs {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl BiquadCoefs {
    fn for_band(band: &EqBand, sample_rate: f32) -> Self {
        // Clamp frequency below Nyquist; a band tuned at fs/2 produces NaNs
        // because alpha collapses to zero, so we leave a small headroom.
        let nyq = sample_rate * 0.499;
        let freq = band.frequency_hz.clamp(1.0, nyq);
        let q = band.q.max(0.0001);
        let gain_db = band.gain_db;
        match band.kind {
            EqBandKind::Peak => Self::peak(freq, q, gain_db, sample_rate),
            EqBandKind::Lowshelf => Self::low_shelf(freq, q, gain_db, sample_rate),
            EqBandKind::Highshelf => Self::high_shelf(freq, q, gain_db, sample_rate),
            EqBandKind::Highpass => Self::highpass(freq, q, sample_rate),
            EqBandKind::Lowpass => Self::lowpass(freq, q, sample_rate),
            EqBandKind::Notch => Self::notch(freq, q, sample_rate),
        }
    }

    fn peak(freq: f32, q: f32, gain_db: f32, fs: f32) -> Self {
        let a = 10.0_f32.powf(gain_db / 40.0);
        let (cos_w, sin_w) = (2.0 * PI * freq / fs).sin_cos_swapped();
        let alpha = sin_w / (2.0 * q);
        let a0 = 1.0 + alpha / a;
        Self::norm(
            a0,
            1.0 + alpha * a,
            -2.0 * cos_w,
            1.0 - alpha * a,
            -2.0 * cos_w,
            1.0 - alpha / a,
        )
    }

    fn low_shelf(freq: f32, q: f32, gain_db: f32, fs: f32) -> Self {
        let a = 10.0_f32.powf(gain_db / 40.0);
        let (cos_w, sin_w) = (2.0 * PI * freq / fs).sin_cos_swapped();
        let alpha = sin_w / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let ap = a + 1.0;
        let am = a - 1.0;
        let a0 = ap + am * cos_w + two_sqrt_a_alpha;
        let b0 = a * (ap - am * cos_w + two_sqrt_a_alpha);
        let b1 = 2.0 * a * (am - ap * cos_w);
        let b2 = a * (ap - am * cos_w - two_sqrt_a_alpha);
        let a1 = -2.0 * (am + ap * cos_w);
        let a2 = ap + am * cos_w - two_sqrt_a_alpha;
        Self::norm(a0, b0, b1, b2, a1, a2)
    }

    fn high_shelf(freq: f32, q: f32, gain_db: f32, fs: f32) -> Self {
        let a = 10.0_f32.powf(gain_db / 40.0);
        let (cos_w, sin_w) = (2.0 * PI * freq / fs).sin_cos_swapped();
        let alpha = sin_w / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;
        let ap = a + 1.0;
        let am = a - 1.0;
        let a0 = ap - am * cos_w + two_sqrt_a_alpha;
        let b0 = a * (ap + am * cos_w + two_sqrt_a_alpha);
        let b1 = -2.0 * a * (am + ap * cos_w);
        let b2 = a * (ap + am * cos_w - two_sqrt_a_alpha);
        let a1 = 2.0 * (am - ap * cos_w);
        let a2 = ap - am * cos_w - two_sqrt_a_alpha;
        Self::norm(a0, b0, b1, b2, a1, a2)
    }

    fn highpass(freq: f32, q: f32, fs: f32) -> Self {
        let (cos_w, sin_w) = (2.0 * PI * freq / fs).sin_cos_swapped();
        let alpha = sin_w / (2.0 * q);
        let a0 = 1.0 + alpha;
        let one_plus_cos = 1.0 + cos_w;
        Self::norm(
            a0,
            one_plus_cos * 0.5,
            -one_plus_cos,
            one_plus_cos * 0.5,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    fn lowpass(freq: f32, q: f32, fs: f32) -> Self {
        let (cos_w, sin_w) = (2.0 * PI * freq / fs).sin_cos_swapped();
        let alpha = sin_w / (2.0 * q);
        let a0 = 1.0 + alpha;
        let one_minus_cos = 1.0 - cos_w;
        Self::norm(
            a0,
            one_minus_cos * 0.5,
            one_minus_cos,
            one_minus_cos * 0.5,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    fn notch(freq: f32, q: f32, fs: f32) -> Self {
        let (cos_w, sin_w) = (2.0 * PI * freq / fs).sin_cos_swapped();
        let alpha = sin_w / (2.0 * q);
        let a0 = 1.0 + alpha;
        Self::norm(
            a0,
            1.0,
            -2.0 * cos_w,
            1.0,
            -2.0 * cos_w,
            1.0 - alpha,
        )
    }

    fn norm(a0: f32, b0: f32, b1: f32, b2: f32, a1: f32, a2: f32) -> Self {
        let inv = 1.0 / a0;
        Self {
            b0: b0 * inv,
            b1: b1 * inv,
            b2: b2 * inv,
            a1: a1 * inv,
            a2: a2 * inv,
        }
    }
}

/// Helper that returns `(cos, sin)` to keep the biquad call sites readable.
trait SinCosSwap {
    fn sin_cos_swapped(self) -> (f32, f32);
}

impl SinCosSwap for f32 {
    fn sin_cos_swapped(self) -> (f32, f32) {
        let (s, c) = self.sin_cos();
        (c, s)
    }
}

/// Algorithmic reverb in the spirit of Jezar Wakefield's Freeverb: a bank of
/// parallel low-pass-damped comb filters feeds a series of allpass diffusers,
/// with a slight per-channel delay spread for stereo. Tuned by `model`,
/// `size`, `damping`, and a wet/dry `mix`.
///
/// For destructive ops the reverb processor is created fresh, runs over the
/// whole range in one pass, and is dropped — the state lives only for the
/// duration of `apply`.
fn reverb(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    params: &ReverbParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let s = slice_for(buffer, channels, range)?;
    let ch = channels as usize;
    let frames = s.len() / ch;
    if frames == 0 {
        return Ok(());
    }
    let mix = params.mix.clamp(0.0, 1.0);
    if mix == 0.0 {
        return Ok(());
    }
    let dry = 1.0 - mix;
    let wet = mix;

    let (room_offset, room_scale, damp_scale) = match params.model {
        ReverbModel::Room => (0.65, 0.28, 0.40),
        ReverbModel::Hall => (0.78, 0.20, 0.30),
        ReverbModel::Plate => (0.85, 0.13, 0.25),
    };
    let size = params.size.clamp(0.0, 1.0);
    let damping = params.damping.clamp(0.0, 1.0);
    let feedback = room_offset + size * room_scale;
    let damp = damping * damp_scale;

    // Freeverb tunings, given at 44.1 kHz; rescale to the actual rate.
    let scale = sample_rate as f32 / 44_100.0;
    const COMB_TUNINGS: [usize; 8] =
        [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
    const ALLPASS_TUNINGS: [usize; 4] = [556, 441, 341, 225];
    const STEREO_SPREAD: usize = 23;
    // Empirically calibrated input gain so the wet level stays in a sensible
    // range across sample rates and sizes (Freeverb's published constant).
    const FIXED_GAIN: f32 = 0.015;

    let make_combs = |spread: usize| {
        COMB_TUNINGS
            .iter()
            .map(|&n| {
                let len = ((n + spread) as f32 * scale) as usize;
                LowpassComb::new(len.max(1), feedback, damp)
            })
            .collect::<Vec<_>>()
    };
    let make_allpasses = |spread: usize| {
        ALLPASS_TUNINGS
            .iter()
            .map(|&n| {
                let len = ((n + spread) as f32 * scale) as usize;
                Allpass::new(len.max(1), 0.5)
            })
            .collect::<Vec<_>>()
    };

    if ch == 1 {
        let mut combs = make_combs(0);
        let mut allpasses = make_allpasses(0);
        for sample in s.iter_mut() {
            let input = *sample * FIXED_GAIN;
            let mut acc = 0.0_f32;
            for c in combs.iter_mut() {
                acc += c.process(input);
            }
            let mut out = acc;
            for a in allpasses.iter_mut() {
                out = a.process(out);
            }
            *sample = *sample * dry + out * wet;
        }
    } else {
        let mut combs_l = make_combs(0);
        let mut combs_r = make_combs(STEREO_SPREAD);
        let mut allpass_l = make_allpasses(0);
        let mut allpass_r = make_allpasses(STEREO_SPREAD);
        for f in 0..frames {
            let l_in = s[f * 2];
            let r_in = s[f * 2 + 1];
            let mono_input = (l_in + r_in) * 0.5 * FIXED_GAIN;
            let mut acc_l = 0.0_f32;
            let mut acc_r = 0.0_f32;
            for c in combs_l.iter_mut() {
                acc_l += c.process(mono_input);
            }
            for c in combs_r.iter_mut() {
                acc_r += c.process(mono_input);
            }
            let mut out_l = acc_l;
            let mut out_r = acc_r;
            for a in allpass_l.iter_mut() {
                out_l = a.process(out_l);
            }
            for a in allpass_r.iter_mut() {
                out_r = a.process(out_r);
            }
            s[f * 2] = l_in * dry + out_l * wet;
            s[f * 2 + 1] = r_in * dry + out_r * wet;
        }
    }
    Ok(())
}

/// Comb filter with a one-pole low-pass on the feedback path: the low pass
/// rolls off the highs each time the signal recirculates, which is what
/// gives reverberant tails their characteristic darkening over time.
struct LowpassComb {
    buffer: Vec<f32>,
    pos: usize,
    feedback: f32,
    damp: f32,
    filter_state: f32,
}

impl LowpassComb {
    fn new(size: usize, feedback: f32, damp: f32) -> Self {
        Self {
            buffer: vec![0.0; size],
            pos: 0,
            feedback,
            damp,
            filter_state: 0.0,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let output = self.buffer[self.pos];
        // 1-pole LP: state = output * (1-damp) + state * damp.
        self.filter_state = output * (1.0 - self.damp) + self.filter_state * self.damp;
        self.buffer[self.pos] = input + self.filter_state * self.feedback;
        self.pos = (self.pos + 1) % self.buffer.len();
        output
    }
}

/// Schroeder allpass: gives the diffusion needed after the comb bank without
/// changing the reverb's overall energy decay. Feedback is fixed at 0.5
/// (Jezar's choice; well-behaved for cascaded sections).
struct Allpass {
    buffer: Vec<f32>,
    pos: usize,
    feedback: f32,
}

impl Allpass {
    fn new(size: usize, feedback: f32) -> Self {
        Self {
            buffer: vec![0.0; size],
            pos: 0,
            feedback,
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let bufout = self.buffer[self.pos];
        let output = -input + bufout;
        self.buffer[self.pos] = input + bufout * self.feedback;
        self.pos = (self.pos + 1) % self.buffer.len();
        output
    }
}

fn db_to_linear(db: f32) -> f32 {
    10.0_f32.powf(db / 20.0)
}

fn linear_to_db(x: f32) -> f32 {
    if x <= 0.0 {
        -120.0
    } else {
        20.0 * x.log10()
    }
}

/// Single-pole smoothing coefficient for a given time constant. Standard
/// formula: `alpha = exp(-1 / (tau * sample_rate))`. A zero or negative time
/// constant collapses to zero, which makes the smoother track instantaneously.
fn time_const_coef(time_ms: f32, sample_rate: u32) -> f32 {
    if time_ms <= 0.0 {
        return 0.0;
    }
    (-1000.0 / (time_ms * sample_rate as f32)).exp()
}

fn delay(
    buffer: &mut Vec<f32>,
    channels: u16,
    range: SampleRange,
    params: &DelayParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let ch_u = channels as u64;
    let total_frames = buffer.len() as u64 / ch_u;
    if range.end() > total_frames {
        return Err(DspError::RangeOutsideBuffer {
            range,
            frames: total_frames,
        });
    }
    let frames_in = (range.end() - range.start()) as usize;
    if frames_in == 0 {
        return Ok(());
    }
    let delay_frames = ((params.time_ms / 1000.0) * sample_rate as f32) as usize;
    if delay_frames == 0 {
        return Ok(());
    }
    let feedback = params.feedback.clamp(0.0, 0.99);
    let mix = params.mix.clamp(0.0, 1.0);
    if mix <= 0.0 {
        return Ok(());
    }
    let dry = 1.0 - mix;
    let ch = channels as usize;
    let ping_pong = params.ping_pong && ch == 2;

    // Tail: how many extra frames to process after the selection so the
    // feedback echoes ring out instead of getting cut off at the boundary.
    // For zero feedback that's one delay_frames; otherwise enough repeats
    // for the geometric series to drop below ~-60 dB. Capped at 4 s so a
    // very-high-feedback delay doesn't blow up the source length.
    let tail_repeats = if feedback <= 1e-6 {
        1
    } else {
        let n = (-6.91_f32 / feedback.ln()).ceil() as usize;
        n.clamp(2, 32)
    };
    let max_tail_frames = (sample_rate * 4) as usize;
    let tail_wanted = (delay_frames * tail_repeats).min(max_tail_frames);

    // Bleed the wet tail into existing post-range audio first; if the
    // tail outruns what's already in the buffer, append zeros so the
    // ringing still has somewhere to land.
    let post_avail = (total_frames - range.end()) as usize;
    let extend_by = tail_wanted.saturating_sub(post_avail);
    if extend_by > 0 {
        buffer.extend(std::iter::repeat(0.0_f32).take(extend_by * ch));
    }
    let total_proc = frames_in + tail_wanted;

    let start_sample = (range.start() * ch_u) as usize;
    let work_end = start_sample + total_proc * ch;
    // Snapshot the dry signal *before* we start writing so the output mix
    // sees the original samples (in-range) and the original post-range
    // audio (in-tail) rather than partially-overwritten content.
    let dry_signal: Vec<f32> = buffer[start_sample..work_end].to_vec();
    let work = &mut buffer[start_sample..work_end];

    let mut lines: Vec<Vec<f32>> = (0..ch).map(|_| vec![0.0_f32; delay_frames]).collect();
    let mut head = 0_usize;

    for f in 0..total_proc {
        let in_range = f < frames_in;
        // Snapshot every channel's delayed sample first — both the
        // feedback path and the output mix can read across channels in
        // ping-pong mode, so we want a consistent view of the lines.
        let mut delayed = [0.0_f32; 2];
        for (c, d) in delayed.iter_mut().enumerate().take(ch) {
            *d = lines[c][head];
        }

        for c in 0..ch {
            // Sample fed into the delay line: the live audio while we're
            // inside the range, silence in the tail (don't pump post-range
            // content into the line and create new echoes from it).
            let line_in = if in_range { dry_signal[f * ch + c] } else { 0.0 };
            let cross = if ping_pong { 1 - c } else { c };
            lines[c][head] = line_in + delayed[cross] * feedback;
            // In-range output mixes dry+wet at the configured ratio.
            // In the tail, leave the original post-range audio at unity
            // and add the wet on top — that's the "bleed past selection"
            // behaviour Cool Edit-style delay tails want.
            let out = if in_range {
                dry_signal[f * ch + c] * dry + delayed[cross] * mix
            } else {
                dry_signal[f * ch + c] + delayed[cross] * mix
            };
            work[f * ch + c] = out;
        }

        head = (head + 1) % delay_frames;
    }
    Ok(())
}

/// Tanh waveshaper. Boost the input by `drive_db`, push it through tanh
/// (which soft-saturates around ±1), and optionally lowpass the wet path
/// before mixing back with the dry. With drive=0 dB and mix=1 the output
/// is approximately the input scaled by ~0.76 because tanh's slope at 0
/// is 1 but it compresses the peaks; we apply a make-up gain of
/// `1 / tanh(drive_lin)` so unity drive feels close to unity output.
fn distortion(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    params: &DistortionParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let s = slice_for(buffer, channels, range)?;
    let ch = channels as usize;
    let frames = s.len() / ch;
    if frames == 0 {
        return Ok(());
    }
    let drive = db_to_linear(params.drive_db).max(0.0);
    let mix = params.mix.clamp(0.0, 1.0);
    if mix <= 0.0 {
        return Ok(());
    }
    let dry = 1.0 - mix;

    // Make-up: at small drive, tanh is near-linear so makeup ≈ 1; at high
    // drive, tanh saturates so makeup pulls peaks back into headroom.
    let makeup = if drive > 1e-6 {
        // Use a unity-input reference: how much does tanh compress an
        // input of magnitude 1? Inverse that so a peak input still maps
        // to ~1 at the output.
        let probe = (drive).tanh();
        if probe > 1e-6 { 1.0 / probe } else { 1.0 }
    } else {
        1.0
    };

    // One-pole lowpass on the wet path, per channel. y[n] = a*x + (1-a)*y[n-1].
    // Cutoff frequency uses the standard exponential mapping.
    let lp_alpha = params.tone_hz.map(|hz| {
        let hz = hz.clamp(20.0, sample_rate as f32 * 0.45);
        let rc = 1.0 / (2.0 * PI * hz);
        let dt = 1.0 / sample_rate as f32;
        dt / (rc + dt)
    });
    let mut lp_state = vec![0.0_f32; ch];

    for f in 0..frames {
        for c in 0..ch {
            let x = s[f * ch + c];
            let driven = (x * drive).tanh() * makeup;
            let wet = if let Some(a) = lp_alpha {
                lp_state[c] = a * driven + (1.0 - a) * lp_state[c];
                lp_state[c]
            } else {
                driven
            };
            s[f * ch + c] = x * dry + wet * mix;
        }
    }
    Ok(())
}

/// Stereo chorus. A small bank of `voices` modulated delay lines, each
/// driven by an LFO at `rate_hz` with a phase offset of `2π * v / voices`
/// so the voices stagger. The delay-line length is `centre_ms + depth_ms`
/// (so the modulated tap can sweep from `centre - depth` up to
/// `centre + depth`). Linear interpolation reads the fractional tap.
/// Stereo voices alternate phase polarity for L/R to spread the image.
fn chorus(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    params: &ChorusParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let s = slice_for(buffer, channels, range)?;
    let ch = channels as usize;
    let frames = s.len() / ch;
    if frames == 0 {
        return Ok(());
    }
    let mix = params.mix.clamp(0.0, 1.0);
    if mix <= 0.0 {
        return Ok(());
    }
    let dry = 1.0 - mix;
    let voices = params.voices.clamp(1, 8) as usize;
    let rate = params.rate_hz.max(0.0);
    let depth_ms = params.depth_ms.max(0.0);

    // Centre delay sits 12 ms back so the modulation has room to swing
    // both ways even at zero depth without folding negative.
    let centre_ms = 12.0_f32;
    let max_delay_frames =
        (((centre_ms + depth_ms) / 1000.0) * sample_rate as f32).ceil() as usize + 2;
    if max_delay_frames < 2 {
        return Ok(());
    }

    let centre_frames = (centre_ms / 1000.0) * sample_rate as f32;
    let depth_frames = (depth_ms / 1000.0) * sample_rate as f32;
    let phase_inc = 2.0 * PI * rate / sample_rate as f32;

    let mut lines: Vec<Vec<f32>> = (0..ch).map(|_| vec![0.0_f32; max_delay_frames]).collect();
    let mut head = 0_usize;
    // Per-voice phase counters; staggered by 2π/voices so the LFOs don't
    // line up in time. Stereo channels flip phase polarity (offset by π)
    // so the L/R wets sweep against each other for width.
    let mut phases: Vec<f32> = (0..voices)
        .map(|v| 2.0 * PI * (v as f32) / voices as f32)
        .collect();

    for f in 0..frames {
        // Write input into every per-channel line at the current head.
        for c in 0..ch {
            lines[c][head] = s[f * ch + c];
        }
        // Sum per-voice modulated reads into the wet bus per channel.
        let mut wet = vec![0.0_f32; ch];
        for v in 0..voices {
            for c in 0..ch {
                let polarity = if c == 1 { -1.0 } else { 1.0 };
                let lfo = (phases[v] * polarity).sin();
                let tap = centre_frames + lfo * depth_frames;
                // Read `tap` frames behind the head, with linear interp
                // between the floor and ceil samples.
                let read_pos = head as f32 - tap;
                // Wrap into [0, max_delay_frames).
                let n = max_delay_frames as f32;
                let mut wrapped = read_pos % n;
                if wrapped < 0.0 {
                    wrapped += n;
                }
                let i0 = wrapped.floor() as usize % max_delay_frames;
                let i1 = (i0 + 1) % max_delay_frames;
                let frac = wrapped - wrapped.floor();
                wet[c] += lines[c][i0] * (1.0 - frac) + lines[c][i1] * frac;
            }
        }
        let voice_norm = 1.0 / voices as f32;
        for c in 0..ch {
            let dry_in = s[f * ch + c];
            s[f * ch + c] = dry_in * dry + wet[c] * voice_norm * mix;
        }
        head = (head + 1) % max_delay_frames;
        for p in &mut phases {
            *p += phase_inc;
            if *p > 2.0 * PI {
                *p -= 2.0 * PI;
            }
        }
    }
    Ok(())
}

fn compress(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    params: &CompParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    let s = slice_for(buffer, channels, range)?;
    let ch = channels as usize;
    let frames = s.len() / ch;
    if frames == 0 || params.ratio <= 1.0 {
        return Ok(());
    }
    let attack_coef = time_const_coef(params.attack_ms, sample_rate);
    let release_coef = time_const_coef(params.release_ms, sample_rate);
    let makeup_lin = db_to_linear(params.makeup_db);
    let knee = params.knee_db.max(0.0);
    let inv_ratio = 1.0 / params.ratio;

    // Channel-linked detector: take the maximum absolute sample across
    // channels each frame and run a single envelope. Stereo pumping of one
    // channel based on the other is intentional for a stereo-linked
    // compressor, which is what doc 03 implies.
    let mut env = 0.0_f32;
    for f in 0..frames {
        let mut peak = 0.0_f32;
        for c in 0..ch {
            peak = peak.max(s[f * ch + c].abs());
        }
        let target = peak;
        env = if target > env {
            attack_coef * env + (1.0 - attack_coef) * target
        } else {
            release_coef * env + (1.0 - release_coef) * target
        };

        let env_db = linear_to_db(env);
        let gr_db = static_gain_reduction(env_db, params.threshold_db, inv_ratio, knee);
        let gain = db_to_linear(-gr_db) * makeup_lin;
        for c in 0..ch {
            s[f * ch + c] *= gain;
        }
    }
    Ok(())
}

/// Static gain-reduction curve in dB. Below `threshold - knee/2`: zero
/// reduction. Above `threshold + knee/2`: linear `(env - threshold)(1 - 1/r)`.
/// Inside the knee: a quadratic interpolant that's continuous at both ends
/// and has matching slope.
fn static_gain_reduction(env_db: f32, threshold_db: f32, inv_ratio: f32, knee_db: f32) -> f32 {
    let half_knee = knee_db * 0.5;
    let upper = threshold_db + half_knee;
    let lower = threshold_db - half_knee;
    if env_db <= lower {
        0.0
    } else if env_db >= upper {
        (env_db - threshold_db) * (1.0 - inv_ratio)
    } else if knee_db <= 0.0 {
        // Hard knee
        (env_db - threshold_db) * (1.0 - inv_ratio)
    } else {
        let t = (env_db - lower) / knee_db;
        // Smooth quadratic from 0 at t=0 to overshoot*(1-1/r) at t=1.
        let overshoot = upper - threshold_db;
        let target = overshoot * (1.0 - inv_ratio);
        target * t * t
    }
}

fn limit(
    buffer: &mut [f32],
    channels: u16,
    range: SampleRange,
    params: &LimitParams,
    sample_rate: u32,
) -> Result<(), DspError> {
    // Lookahead-free brick-wall limiter: a fast feedback compressor with a
    // very high ratio at the ceiling, followed by a hard clip to catch
    // anything that slips through the smoothed envelope. lookahead_ms is
    // accepted but not used in this slice.
    let _ = params.lookahead_ms;
    let s = slice_for(buffer, channels, range)?;
    let ch = channels as usize;
    let frames = s.len() / ch;
    if frames == 0 {
        return Ok(());
    }
    let release_coef = time_const_coef(params.release_ms, sample_rate);
    let attack_coef = time_const_coef(0.5, sample_rate); // ~half-millisecond attack
    let ceiling_lin = db_to_linear(params.ceiling_db);
    let mut env = 0.0_f32;

    for f in 0..frames {
        let mut peak = 0.0_f32;
        for c in 0..ch {
            peak = peak.max(s[f * ch + c].abs());
        }
        env = if peak > env {
            attack_coef * env + (1.0 - attack_coef) * peak
        } else {
            release_coef * env + (1.0 - release_coef) * peak
        };
        let gain = if env > ceiling_lin {
            ceiling_lin / env
        } else {
            1.0
        };
        for c in 0..ch {
            let scaled = s[f * ch + c] * gain;
            // Hard clip backstop: under transient overshoot the smoothed
            // envelope can briefly trail the actual peak; clamp to the
            // ceiling so the limiter is genuinely brick-wall.
            s[f * ch + c] = scaled.clamp(-ceiling_lin, ceiling_lin);
        }
    }
    Ok(())
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
        // TimeStretch / PitchShift are now implemented; pick an op that's
        // still stubbed out to verify the dispatch error path.
        let mut samples = vec![0.0; 4];
        let err = apply(
            &Op::Insert {
                at: 0,
                samples_ref: crate::ids::ClipboardRef::new("cb_test"),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap_err();
        assert!(matches!(err, DspError::Unsupported("Insert")));
    }

    fn reverb_params(model: ReverbModel, size: f32, damping: f32, mix: f32) -> ReverbParams {
        ReverbParams {
            model,
            size,
            damping,
            mix,
        }
    }

    #[test]
    fn reverb_mix_zero_is_a_no_op() {
        let mut samples = vec![0.5_f32, -0.5, 0.25, -0.25];
        let original = samples.clone();
        apply(
            &Op::Reverb {
                range: r(0, 4),
                params: reverb_params(ReverbModel::Hall, 0.5, 0.5, 0.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_eq!(samples, original);
    }

    #[test]
    fn reverb_modifies_signal_at_full_wet() {
        // An impulse run through the reverb must NOT come back unchanged when
        // mix is 1.0.
        let mut samples = vec![0.0_f32; 1024];
        samples[0] = 1.0;
        let baseline = samples.clone();
        apply(
            &Op::Reverb {
                range: r(0, 1024),
                params: reverb_params(ReverbModel::Hall, 0.5, 0.5, 1.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_ne!(samples, baseline);
    }

    #[test]
    fn reverb_tail_decays_over_time() {
        // Drive the reverb with a single impulse and check that the energy
        // in successive time windows is non-increasing — the late tail
        // should never be louder than an earlier window.
        let sample_rate = 48_000;
        let frames = sample_rate as usize;
        let mut samples = vec![0.0_f32; frames];
        samples[0] = 1.0;
        apply(
            &Op::Reverb {
                range: r(0, frames as u64),
                params: reverb_params(ReverbModel::Hall, 0.5, 0.5, 1.0),
            },
            &mut samples,
            1,
            sample_rate,
        )
        .unwrap();
        // Skip the first ~10 ms so the early reflections are out of the way,
        // then compare 50-ms windows.
        let window = sample_rate as usize / 20; // 50 ms
        let start = sample_rate as usize / 100; // 10 ms
        let early = rms(&samples[start..start + window]);
        let mid = rms(&samples[start + window * 4..start + window * 5]);
        let late = rms(&samples[start + window * 8..start + window * 9]);
        assert!(early > 0.0, "expected non-silent early window");
        assert!(mid <= early * 1.0, "mid {mid} not <= early {early}");
        assert!(late <= mid * 1.0, "late {late} not <= mid {mid}");
        // The end of the second is much quieter than the beginning.
        assert!(late < early * 0.5, "tail did not decay enough: early={early} late={late}");
    }

    #[test]
    fn reverb_larger_size_produces_louder_late_tail() {
        let sample_rate = 48_000;
        let frames = sample_rate as usize;
        let measure = |size: f32| {
            let mut samples = vec![0.0_f32; frames];
            samples[0] = 1.0;
            apply(
                &Op::Reverb {
                    range: r(0, frames as u64),
                    params: reverb_params(ReverbModel::Hall, size, 0.3, 1.0),
                },
                &mut samples,
                1,
                sample_rate,
            )
            .unwrap();
            // Late window: 700–900 ms.
            let start = sample_rate as usize * 7 / 10;
            let end = sample_rate as usize * 9 / 10;
            rms(&samples[start..end])
        };
        let small = measure(0.0);
        let large = measure(1.0);
        assert!(
            large > small,
            "size=1 late tail ({large}) should exceed size=0 ({small})"
        );
    }

    #[test]
    fn reverb_processes_stereo_independently() {
        // Stereo impulse only on the left channel; the right channel input
        // is silent. With mono summation, both wet outputs will receive
        // some energy (Freeverb sums L+R as input), but the per-channel
        // delay spread means L and R are not bit-identical in the tail.
        let sample_rate = 48_000;
        let frames = sample_rate as usize / 4;
        let mut samples = vec![0.0_f32; frames * 2];
        samples[0] = 1.0; // L impulse
        apply(
            &Op::Reverb {
                range: r(0, frames as u64),
                params: reverb_params(ReverbModel::Hall, 0.5, 0.5, 1.0),
            },
            &mut samples,
            2,
            sample_rate,
        )
        .unwrap();
        let mut diff_count = 0;
        for f in (sample_rate as usize / 100)..frames {
            if (samples[f * 2] - samples[f * 2 + 1]).abs() > 1e-6 {
                diff_count += 1;
            }
        }
        assert!(
            diff_count > 100,
            "stereo spread should make L and R diverge in the tail (got {diff_count} differing frames)"
        );
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
    fn trim_keeps_only_the_range() {
        let mut samples = buf(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        apply(&Op::Trim { range: r(1, 4) }, &mut samples, 1, 48_000).unwrap();
        assert_eq!(samples, vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn trim_in_stereo_drops_outside_frame_pairs() {
        let mut samples = buf(&[1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0]);
        apply(&Op::Trim { range: r(1, 3) }, &mut samples, 2, 48_000).unwrap();
        assert_eq!(samples, vec![2.0, -2.0, 3.0, -3.0]);
    }

    #[test]
    fn trim_full_range_is_identity() {
        let mut samples = buf(&[1.0, 2.0, 3.0]);
        apply(&Op::Trim { range: r(0, 3) }, &mut samples, 1, 48_000).unwrap();
        assert_eq!(samples, vec![1.0, 2.0, 3.0]);
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

    fn delay_params(time_ms: f32, feedback: f32, mix: f32) -> DelayParams {
        DelayParams {
            time_ms,
            feedback,
            mix,
            ping_pong: false,
            feedback_lp_hz: None,
        }
    }

    fn comp_params(threshold_db: f32, ratio: f32) -> CompParams {
        CompParams {
            threshold_db,
            ratio,
            attack_ms: 1.0,
            release_ms: 50.0,
            makeup_db: 0.0,
            knee_db: 0.0,
        }
    }

    #[test]
    fn delay_with_full_wet_mix_shifts_an_impulse() {
        // 1ms delay at 48 kHz = 48 frames. Fully wet (mix=1) and zero
        // feedback should produce a single tap one delay-line later.
        let mut samples = vec![0.0_f32; 200];
        samples[0] = 1.0;
        apply(
            &Op::Delay {
                range: r(0, 200),
                params: delay_params(1.0, 0.0, 1.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert!(samples[0].abs() < 1e-6);
        assert!((samples[48] - 1.0).abs() < 1e-6);
        // No second tap because feedback is zero.
        assert!(samples[96].abs() < 1e-6);
    }

    #[test]
    fn delay_with_feedback_repeats_with_decay() {
        let mut samples = vec![0.0_f32; 400];
        samples[0] = 1.0;
        apply(
            &Op::Delay {
                range: r(0, 400),
                params: delay_params(1.0, 0.5, 1.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        // Each tap is half the previous one (linear, ignoring rounding).
        assert!((samples[48] - 1.0).abs() < 1e-6);
        assert!((samples[96] - 0.5).abs() < 1e-3);
        assert!((samples[144] - 0.25).abs() < 1e-3);
    }

    #[test]
    fn delay_dry_only_passes_signal_unchanged() {
        let mut samples = vec![0.5_f32, -0.25, 0.25];
        let original = samples.clone();
        apply(
            &Op::Delay {
                range: r(0, 3),
                params: delay_params(1.0, 0.5, 0.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_eq!(samples, original);
    }

    #[test]
    fn delay_bleeds_wet_tail_past_selection_end() {
        // Selection covers only the first half (200 frames). Without the
        // tail extension this used to play as gain-only because the wet
        // path needed `delay_frames` headroom past the impulse before
        // anything wet could land. The tail should now ring out into the
        // existing post-range audio.
        let mut samples = vec![0.0_f32; 400];
        samples[0] = 1.0;
        apply(
            &Op::Delay {
                range: r(0, 200),
                params: delay_params(1.0, 0.5, 1.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        // First echo at frame 48 (one delay-line away).
        assert!(
            (samples[48] - 1.0).abs() < 1e-4,
            "first echo missing at frame 48 (got {})",
            samples[48]
        );
        // Second echo at frame 96 — past the selection boundary at 200,
        // still inside the original buffer because tail bleeds past.
        // Actually 96 is still inside the selection; check 240, well
        // beyond the 200 selection boundary, where a feedback echo lands.
        // With feedback=0.5 the echo train is 1.0, 0.5, 0.25, 0.125, ...
        // at frames 48, 96, 144, 192, 240, ...
        assert!(
            samples[240].abs() > 1e-3,
            "tail echo at frame 240 (past selection) lost (got {})",
            samples[240]
        );
    }

    #[test]
    fn delay_ping_pong_swaps_channels() {
        // Stereo impulse on the L channel. With ping-pong, the first echo
        // should appear on R, the second back on L.
        let mut samples = vec![0.0_f32; 400 * 2];
        samples[0] = 1.0; // L
        let mut params = delay_params(1.0, 0.5, 1.0);
        params.ping_pong = true;
        apply(
            &Op::Delay {
                range: r(0, 400),
                params,
            },
            &mut samples,
            2,
            48_000,
        )
        .unwrap();
        // Frame 48: R has the first echo, L is silent.
        assert!(samples[48 * 2].abs() < 1e-6);
        assert!((samples[48 * 2 + 1] - 1.0).abs() < 1e-6);
        // Frame 96: L gets the bounce.
        assert!((samples[96 * 2] - 0.5).abs() < 1e-3);
        assert!(samples[96 * 2 + 1].abs() < 1e-3);
    }

    #[test]
    fn distortion_with_zero_mix_is_identity() {
        let mut samples = buf(&[0.1, -0.2, 0.5, -0.4]);
        let original = samples.clone();
        apply(
            &Op::Distortion {
                range: r(0, 4),
                params: DistortionParams { drive_db: 24.0, tone_hz: None, mix: 0.0 },
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_eq!(samples, original);
    }

    #[test]
    fn distortion_high_drive_clips_peaks() {
        // A loud sine should saturate near ±1 with high drive + full wet.
        let fs = 48_000_u32;
        let frames = 480;
        let mut samples: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / fs as f32;
                0.6 * (2.0 * PI * 440.0 * t).sin()
            })
            .collect();
        apply(
            &Op::Distortion {
                range: r(0, frames as u64),
                params: DistortionParams { drive_db: 30.0, tone_hz: None, mix: 1.0 },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let peak = samples.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        // Should be very close to 1.0 — the make-up gain pulls a saturated
        // signal back into headroom but tanh keeps it bounded.
        assert!(peak > 0.9 && peak <= 1.05, "peak after distortion = {peak}");
    }

    #[test]
    fn chorus_with_zero_mix_is_identity() {
        let mut samples = buf(&[0.1, -0.2, 0.5, -0.4]);
        let original = samples.clone();
        apply(
            &Op::Chorus {
                range: r(0, 4),
                params: ChorusParams { rate_hz: 1.0, depth_ms: 5.0, mix: 0.0, voices: 2 },
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_eq!(samples, original);
    }

    #[test]
    fn chorus_modulates_a_steady_tone() {
        // Run a 200 Hz sine through the chorus; the wet path should shift
        // some energy off the fundamental into nearby sidebands. Crude
        // check: the per-frame magnitude no longer matches the dry sine
        // exactly, beyond what the delay-line warm-up alone would do.
        let fs = 48_000_u32;
        let frames = fs as usize / 4; // 250 ms
        let dry: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f32 / fs as f32;
                0.5 * (2.0 * PI * 200.0 * t).sin()
            })
            .collect();
        let mut samples = dry.clone();
        apply(
            &Op::Chorus {
                range: r(0, frames as u64),
                params: ChorusParams { rate_hz: 1.5, depth_ms: 4.0, mix: 0.5, voices: 2 },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        // Past the delay-line warm-up (~16 ms), the wet sample should
        // diverge from the dry by more than rounding noise.
        let warmup = (fs as usize) / 50; // skip first 20 ms
        let mut diff_count = 0;
        for i in warmup..frames {
            if (samples[i] - dry[i]).abs() > 1e-3 {
                diff_count += 1;
            }
        }
        assert!(
            diff_count > frames / 4,
            "chorus barely modulates the input ({diff_count} divergent frames)"
        );
    }

    #[test]
    fn compressor_below_threshold_passes_signal_unchanged() {
        // -20 dB sine, threshold 0 dB, ratio 4:1. Nothing crosses the
        // threshold so output should equal input.
        let amp = db_to_linear(-20.0);
        let mut samples: Vec<f32> = (0..480)
            .map(|n| amp * (n as f32 / 48.0 * std::f32::consts::TAU).sin())
            .collect();
        let original = samples.clone();
        apply(
            &Op::Compress {
                range: r(0, 480),
                params: comp_params(0.0, 4.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        for (a, b) in samples.iter().zip(original.iter()) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn compressor_steady_state_gain_reduction_matches_ratio() {
        // -10 dB sine, threshold -20 dB, ratio 4:1.
        // Static reduction = 10 dB × (1 - 1/4) = 7.5 dB → output peak ~-17.5 dB.
        let sample_rate = 48_000;
        let amp = db_to_linear(-10.0);
        let mut samples: Vec<f32> = (0..sample_rate as usize)
            .map(|n| amp * (n as f32 / 48.0 * std::f32::consts::TAU).sin())
            .collect();
        let mut p = comp_params(-20.0, 4.0);
        p.attack_ms = 1.0;
        p.release_ms = 100.0;
        apply(
            &Op::Compress {
                range: r(0, sample_rate as u64),
                params: p,
            },
            &mut samples,
            1,
            sample_rate,
        )
        .unwrap();
        // Look at the second half of the buffer where the envelope has
        // settled.
        let tail = &samples[sample_rate as usize / 2..];
        let peak = tail.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        let peak_db = linear_to_db(peak);
        assert!(
            (peak_db + 17.5).abs() < 1.0,
            "expected ~-17.5 dB, got {peak_db}"
        );
    }

    #[test]
    fn compressor_makeup_gain_offsets_reduction() {
        let sample_rate = 48_000;
        let amp = db_to_linear(-10.0);
        let mut samples: Vec<f32> = (0..sample_rate as usize)
            .map(|n| amp * (n as f32 / 48.0 * std::f32::consts::TAU).sin())
            .collect();
        let mut p = comp_params(-20.0, 4.0);
        p.makeup_db = 7.5; // exactly cancels the static gain reduction
        p.attack_ms = 1.0;
        p.release_ms = 100.0;
        apply(
            &Op::Compress {
                range: r(0, sample_rate as u64),
                params: p,
            },
            &mut samples,
            1,
            sample_rate,
        )
        .unwrap();
        let tail = &samples[sample_rate as usize / 2..];
        let peak_db = linear_to_db(tail.iter().fold(0.0_f32, |m, &x| m.max(x.abs())));
        assert!((peak_db + 10.0).abs() < 1.0, "expected ~-10 dB, got {peak_db}");
    }

    #[test]
    fn compressor_unity_ratio_is_a_no_op() {
        let mut samples = vec![1.0_f32; 64];
        let original = samples.clone();
        apply(
            &Op::Compress {
                range: r(0, 64),
                params: comp_params(-30.0, 1.0),
            },
            &mut samples,
            1,
            48_000,
        )
        .unwrap();
        assert_eq!(samples, original);
    }

    #[test]
    fn limiter_keeps_peaks_at_or_below_ceiling() {
        // +6 dB sine, ceiling -1 dB. Output must never exceed the ceiling.
        let sample_rate = 48_000;
        let amp = db_to_linear(6.0);
        let mut samples: Vec<f32> = (0..sample_rate as usize / 4)
            .map(|n| amp * (n as f32 / 48.0 * std::f32::consts::TAU).sin())
            .collect();
        apply(
            &Op::Limit {
                range: r(0, samples.len() as u64),
                params: LimitParams {
                    ceiling_db: -1.0,
                    lookahead_ms: 5.0,
                    release_ms: 50.0,
                },
            },
            &mut samples,
            1,
            sample_rate,
        )
        .unwrap();
        let ceiling = db_to_linear(-1.0);
        let peak = samples.iter().fold(0.0_f32, |m, &x| m.max(x.abs()));
        assert!(peak <= ceiling + 1e-6, "peak {peak} > ceiling {ceiling}");
    }

    #[test]
    fn static_gain_reduction_is_continuous_across_threshold_with_knee() {
        // Soft knee should give zero reduction at threshold - knee/2 and
        // (env - threshold)(1 - 1/r) at threshold + knee/2.
        let inv_ratio = 1.0 / 4.0;
        let lower = static_gain_reduction(-22.0, -20.0, inv_ratio, 4.0);
        let upper = static_gain_reduction(-18.0, -20.0, inv_ratio, 4.0);
        let mid = static_gain_reduction(-20.0, -20.0, inv_ratio, 4.0);
        assert!(lower.abs() < 1e-6);
        assert!((upper - 2.0 * (1.0 - inv_ratio)).abs() < 1e-6);
        // Mid (knee centre) should sit between the two.
        assert!(mid > lower && mid < upper);
    }

    fn sine_at(freq_hz: f32, sample_rate: u32, frames: usize, amp_db: f32) -> Vec<f32> {
        let amp = db_to_linear(amp_db);
        let dt = 1.0 / sample_rate as f32;
        (0..frames)
            .map(|n| amp * (2.0 * PI * freq_hz * n as f32 * dt).sin())
            .collect()
    }

    fn rms(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = samples.iter().map(|&x| (x as f64).powi(2)).sum();
        (sum_sq / samples.len() as f64).sqrt() as f32
    }

    fn rms_ratio_db(out: &[f32], inp: &[f32]) -> f32 {
        let r_out = rms(out);
        let r_in = rms(inp);
        if r_out <= 0.0 || r_in <= 0.0 {
            return -120.0;
        }
        20.0 * (r_out / r_in).log10()
    }

    fn eq_band(kind: EqBandKind, freq: f32, gain_db: f32, q: f32) -> EqBand {
        EqBand {
            kind,
            frequency_hz: freq,
            gain_db,
            q,
            enabled: true,
        }
    }

    #[test]
    fn peak_eq_band_boosts_at_centre_frequency() {
        let fs = 48_000;
        let frames = 8192;
        let input = sine_at(1000.0, fs, frames, -12.0);
        let mut samples = input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: vec![eq_band(EqBandKind::Peak, 1_000.0, 6.0, 1.0)],
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let tail_in = &input[frames / 2..];
        let tail_out = &samples[frames / 2..];
        let gain_db = rms_ratio_db(tail_out, tail_in);
        assert!((gain_db - 6.0).abs() < 0.5, "got {gain_db} dB, expected ~+6");
    }

    #[test]
    fn peak_eq_far_from_centre_is_near_unity() {
        let fs = 48_000;
        let frames = 8192;
        // Boost +12 dB at 100 Hz; check the response at 8 kHz, ~6 octaves up.
        let input = sine_at(8_000.0, fs, frames, -12.0);
        let mut samples = input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: vec![eq_band(EqBandKind::Peak, 100.0, 12.0, 1.0)],
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let gain_db = rms_ratio_db(&samples[frames / 2..], &input[frames / 2..]);
        assert!(gain_db.abs() < 0.5, "got {gain_db} dB, expected ~0");
    }

    #[test]
    fn highpass_attenuates_below_cutoff_and_passes_above() {
        let fs = 48_000;
        let frames = 16_384;
        // Highpass at 1 kHz, Q ≈ Butterworth.
        let band = eq_band(EqBandKind::Highpass, 1_000.0, 0.0, 0.707);

        let low_input = sine_at(100.0, fs, frames, -6.0);
        let mut low = low_input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: vec![band],
                },
            },
            &mut low,
            1,
            fs,
        )
        .unwrap();
        let low_db = rms_ratio_db(&low[frames / 2..], &low_input[frames / 2..]);
        assert!(low_db < -20.0, "100 Hz should be heavily attenuated, got {low_db} dB");

        let high_input = sine_at(10_000.0, fs, frames, -6.0);
        let mut high = high_input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: vec![band],
                },
            },
            &mut high,
            1,
            fs,
        )
        .unwrap();
        let high_db = rms_ratio_db(&high[frames / 2..], &high_input[frames / 2..]);
        assert!(high_db.abs() < 1.0, "10 kHz should pass, got {high_db} dB");
    }

    #[test]
    fn lowshelf_boosts_below_corner_only() {
        let fs = 48_000;
        let frames = 16_384;
        let band = eq_band(EqBandKind::Lowshelf, 200.0, 6.0, 0.707);

        let low_input = sine_at(50.0, fs, frames, -12.0);
        let mut low = low_input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: vec![band],
                },
            },
            &mut low,
            1,
            fs,
        )
        .unwrap();
        let low_db = rms_ratio_db(&low[frames / 2..], &low_input[frames / 2..]);
        assert!((low_db - 6.0).abs() < 1.0, "50 Hz expected +6 dB, got {low_db}");

        let high_input = sine_at(5_000.0, fs, frames, -12.0);
        let mut high = high_input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: vec![band],
                },
            },
            &mut high,
            1,
            fs,
        )
        .unwrap();
        let high_db = rms_ratio_db(&high[frames / 2..], &high_input[frames / 2..]);
        assert!(high_db.abs() < 0.5, "5 kHz expected ~0 dB, got {high_db}");
    }

    #[test]
    fn disabled_band_is_a_no_op() {
        let fs = 48_000;
        let frames = 4096;
        let input = sine_at(1_000.0, fs, frames, -12.0);
        let mut samples = input.clone();
        let mut band = eq_band(EqBandKind::Peak, 1_000.0, 12.0, 2.0);
        band.enabled = false;
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: vec![band],
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        // Bit-equal: chain is empty, no biquad runs at all.
        assert_eq!(samples, input);
    }

    #[test]
    fn two_bands_compose_into_one_response() {
        // Cut at 200 Hz with -6 dB peak, boost at 4 kHz with +6 dB peak.
        let fs = 48_000;
        let frames = 16_384;
        let bands = vec![
            eq_band(EqBandKind::Peak, 200.0, -6.0, 1.0),
            eq_band(EqBandKind::Peak, 4_000.0, 6.0, 1.0),
        ];

        let low_input = sine_at(200.0, fs, frames, -12.0);
        let mut low = low_input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams {
                    bands: bands.clone(),
                },
            },
            &mut low,
            1,
            fs,
        )
        .unwrap();
        let low_db = rms_ratio_db(&low[frames / 2..], &low_input[frames / 2..]);
        assert!((low_db + 6.0).abs() < 1.0, "200 Hz expected -6 dB, got {low_db}");

        let high_input = sine_at(4_000.0, fs, frames, -12.0);
        let mut high = high_input.clone();
        apply(
            &Op::Eq {
                range: r(0, frames as u64),
                params: EqParams { bands },
            },
            &mut high,
            1,
            fs,
        )
        .unwrap();
        let high_db = rms_ratio_db(&high[frames / 2..], &high_input[frames / 2..]);
        assert!((high_db - 6.0).abs() < 1.0, "4 kHz expected +6 dB, got {high_db}");
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

    fn synth_sine(freq_hz: f32, fs: u32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|n| (2.0 * PI * freq_hz * n as f32 / fs as f32).sin())
            .collect()
    }

    /// Find the strongest frequency component below Nyquist by direct DFT
    /// at a small set of probe bins. Cheap and adequate for the test
    /// material we generate (single sine).
    fn dominant_freq(signal: &[f32], fs: u32) -> f32 {
        let n = signal.len();
        if n == 0 {
            return 0.0;
        }
        let mut best_mag = 0.0_f32;
        let mut best_freq = 0.0;
        // 0.5 Hz resolution between 50 Hz and 4 kHz is plenty for the test
        // tones (we drive at 440 Hz / 880 Hz / etc.).
        let mut f = 50.0_f32;
        while f < (fs as f32 / 2.0).min(4_000.0) {
            let omega = 2.0 * PI * f / fs as f32;
            let mut re = 0.0_f32;
            let mut im = 0.0_f32;
            for (k, x) in signal.iter().enumerate() {
                let phi = omega * k as f32;
                re += x * phi.cos();
                im += x * phi.sin();
            }
            let mag = (re * re + im * im).sqrt();
            if mag > best_mag {
                best_mag = mag;
                best_freq = f;
            }
            f += 0.5;
        }
        best_freq
    }

    #[test]
    fn time_stretch_doubles_length_at_ratio_two() {
        let fs = 48_000;
        let frames = 4096;
        let mut samples = synth_sine(440.0, fs, frames);
        apply(
            &Op::TimeStretch {
                range: r(0, frames as u64),
                ratio: 2.0,
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        // Output should be ~2x input length (rounding allowed).
        assert!(
            (samples.len() as i64 - (frames as i64 * 2)).abs() <= 2,
            "expected ~{} frames, got {}",
            frames * 2,
            samples.len(),
        );
    }

    #[test]
    fn time_stretch_halves_length_at_ratio_half() {
        let fs = 48_000;
        let frames = 4096;
        let mut samples = synth_sine(440.0, fs, frames);
        apply(
            &Op::TimeStretch {
                range: r(0, frames as u64),
                ratio: 0.5,
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let expected = frames / 2;
        assert!(
            (samples.len() as i64 - expected as i64).abs() <= 2,
            "expected ~{expected} frames, got {}",
            samples.len(),
        );
    }

    #[test]
    fn time_stretch_preserves_pitch() {
        // A 440 Hz tone stretched by 2× should still be 440 Hz.
        let fs = 48_000;
        let frames = 8192;
        let mut samples = synth_sine(440.0, fs, frames);
        apply(
            &Op::TimeStretch {
                range: r(0, frames as u64),
                ratio: 2.0,
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        // Sample the steady-state interior, away from the ramp-up/down.
        let body = &samples[1024..samples.len() - 1024];
        let f = dominant_freq(body, fs);
        assert!((f - 440.0).abs() < 5.0, "expected ~440 Hz, got {f}");
    }

    #[test]
    fn pitch_shift_preserves_length() {
        let fs = 48_000;
        let frames = 4096;
        let mut samples = synth_sine(440.0, fs, frames);
        apply(
            &Op::PitchShift {
                range: r(0, frames as u64),
                cents: 1200.0,
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        assert!(
            (samples.len() as i64 - frames as i64).abs() <= 2,
            "expected length preserved, got {}",
            samples.len(),
        );
    }

    #[test]
    fn pitch_shift_up_an_octave_doubles_frequency() {
        // 440 Hz shifted up 1200 cents (one octave) → 880 Hz, length unchanged.
        let fs = 48_000;
        let frames = 16_384;
        let mut samples = synth_sine(440.0, fs, frames);
        apply(
            &Op::PitchShift {
                range: r(0, frames as u64),
                cents: 1200.0,
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let body = &samples[2048..samples.len() - 2048];
        let f = dominant_freq(body, fs);
        // Allow a few Hz of tolerance — the OLA's smoothness limits accuracy.
        assert!((f - 880.0).abs() < 10.0, "expected ~880 Hz, got {f}");
    }

    #[test]
    fn pitch_shift_zero_cents_is_near_identity() {
        let fs = 48_000;
        let frames = 4096;
        let original = synth_sine(440.0, fs, frames);
        let mut samples = original.clone();
        apply(
            &Op::PitchShift {
                range: r(0, frames as u64),
                cents: 0.0,
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        assert_eq!(samples.len(), original.len());
        let body = &samples[1024..samples.len() - 1024];
        let f = dominant_freq(body, fs);
        assert!((f - 440.0).abs() < 5.0, "expected ~440 Hz, got {f}");
    }

    #[test]
    fn yin_finds_a440() {
        let fs = 48_000;
        let samples = synth_sine(440.0, fs, 2048);
        let f = yin_pitch(&samples, fs).expect("YIN should detect a clean sine");
        assert!((f - 440.0).abs() < 2.0, "got {f}");
    }

    #[test]
    fn yin_finds_low_voice_freq() {
        let fs = 48_000;
        let samples = synth_sine(120.0, fs, 4096);
        let f = yin_pitch(&samples, fs).expect("YIN should detect 120 Hz");
        assert!((f - 120.0).abs() < 2.0, "got {f}");
    }

    #[test]
    fn yin_returns_none_for_silence() {
        let fs = 48_000;
        let samples = vec![0.0_f32; 2048];
        // Silence's CMND stays at 1.0 so the threshold is never crossed.
        assert!(yin_pitch(&samples, fs).is_none());
    }

    #[test]
    fn snap_chromatic_rounds_to_nearest_semitone() {
        // 442 Hz is between A4 (440) and Bb4 (~466). Closer to A4.
        let snapped = snap_to_scale(442.0, AutotuneScale::Chromatic, 0);
        assert!((snapped - 440.0).abs() < 0.5, "got {snapped}");

        // 460 Hz is closer to Bb4 (~466.16).
        let snapped = snap_to_scale(460.0, AutotuneScale::Chromatic, 0);
        assert!((snapped - 466.16).abs() < 1.0, "got {snapped}");
    }

    #[test]
    fn snap_c_major_skips_accidentals() {
        // 480 Hz isn't in C major (it's between Bb4 and B4). Closer to B4
        // (~494), which is the major-7th of C and is in the scale.
        let snapped = snap_to_scale(480.0, AutotuneScale::Major, 0);
        assert!((snapped - 493.88).abs() < 1.0, "got {snapped}");

        // 442 Hz: in C major both A4 (440) and B4 (494) are candidates;
        // 440 is the nearest.
        let snapped = snap_to_scale(442.0, AutotuneScale::Major, 0);
        assert!((snapped - 440.0).abs() < 0.5, "got {snapped}");
    }

    #[test]
    fn snap_a_minor_includes_correct_degrees() {
        // A4 (440) is the root → unchanged.
        let snapped = snap_to_scale(440.0, AutotuneScale::Minor, 9);
        assert!((snapped - 440.0).abs() < 0.5, "got {snapped}");

        // C5 (~523) is the minor third in A minor, allowed.
        let snapped = snap_to_scale(523.0, AutotuneScale::Minor, 9);
        assert!((snapped - 523.25).abs() < 1.0, "got {snapped}");
    }

    #[test]
    fn autotune_pulls_pitch_toward_scale() {
        // A 442 Hz sine through chromatic autotune at retune_ms=0 should be
        // pulled to 440 Hz.
        let fs = 48_000;
        let frames = 16_384;
        let mut samples = synth_sine(442.0, fs, frames);
        apply(
            &Op::Autotune {
                range: r(0, frames as u64),
                params: AutotuneParams {
                    target: AutotuneTarget::Scale {
                        scale: AutotuneScale::Chromatic,
                        key_pc: 0,
                    },
                    retune_ms: 0.0,
                    preserve_formants: false,
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        assert_eq!(samples.len(), frames);
        let body = &samples[2048..samples.len() - 2048];
        let f = dominant_freq(body, fs);
        // Coarse-grained pitch shifters under-shoot a bit; allow 5 Hz slack
        // so the test isn't flaky on edge resamples.
        assert!((f - 440.0).abs() < 5.0, "expected ~440 Hz, got {f}");
    }

    #[test]
    fn autotune_reference_mode_follows_contour() {
        // Input 440 Hz, reference contour says "target 880". With ratio
        // clamped to [0.25, 4.0] this should arrive close to 880.
        let fs = 48_000;
        let frames = 16_384;
        let mut samples = synth_sine(440.0, fs, frames);
        let hop = ((fs as f32) * 0.025) as u64;
        let num_hops = (frames as u64 / hop) as usize + 1;
        let contour = vec![880.0_f32; num_hops];
        apply(
            &Op::Autotune {
                range: r(0, frames as u64),
                params: AutotuneParams {
                    target: AutotuneTarget::Reference {
                        contour_hz: contour,
                        hop_samples: hop,
                    },
                    retune_ms: 0.0,
                    preserve_formants: false,
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let body = &samples[2048..samples.len() - 2048];
        let f = dominant_freq(body, fs);
        assert!((f - 880.0).abs() < 15.0, "expected ~880 Hz, got {f}");
    }

    #[test]
    fn autotune_spectral_pulls_pitch_with_formants_on() {
        // 442 Hz sine through chromatic autotune with formants enabled.
        // Should still pull to ~440 Hz; the spectral path is just a
        // different implementation, not a different goal.
        let fs = 48_000;
        let frames = 32_768;
        let mut samples = synth_sine(442.0, fs, frames);
        apply(
            &Op::Autotune {
                range: r(0, frames as u64),
                params: AutotuneParams {
                    target: AutotuneTarget::Scale {
                        scale: AutotuneScale::Chromatic,
                        key_pc: 0,
                    },
                    retune_ms: 0.0,
                    preserve_formants: true,
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        // The phase vocoder needs a few frames to build up COLA, so trim
        // generously from both ends.
        let body = &samples[4096..samples.len() - 4096];
        let f = dominant_freq(body, fs);
        assert!((f - 440.0).abs() < 6.0, "expected ~440 Hz, got {f}");
    }

    #[test]
    fn autotune_spectral_octave_up_via_reference() {
        // Use a higher fundamental so the FFT bin resolution doesn't
        // dominate. At 880 Hz an octave-up to 1760 Hz puts the energy
        // around bins 75 vs 38 in a 2048-point FFT, well-resolved.
        let fs = 48_000;
        let frames = 32_768;
        let mut samples = synth_sine(880.0, fs, frames);
        let hop = ((fs as f32) * 0.025) as u64;
        let num_hops = (frames as u64 / hop) as usize + 1;
        let contour = vec![1760.0_f32; num_hops];
        apply(
            &Op::Autotune {
                range: r(0, frames as u64),
                params: AutotuneParams {
                    target: AutotuneTarget::Reference {
                        contour_hz: contour,
                        hop_samples: hop,
                    },
                    retune_ms: 0.0,
                    preserve_formants: true,
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let body = &samples[4096..samples.len() - 4096];
        let f = dominant_freq(body, fs);
        // Phase-vocoder bin remap is approximate — accept ±30 Hz where the
        // FFT bin width is ~23 Hz at 2048-point analysis.
        assert!((f - 1760.0).abs() < 30.0, "expected ~1760 Hz, got {f}");
    }

    #[test]
    fn autotune_spectral_no_pitch_change_is_near_identity() {
        // Reference contour matches the input pitch exactly: ratio≈1, the
        // spectral path should leave the signal untouched in pitch.
        let fs = 48_000;
        let frames = 16_384;
        let mut samples = synth_sine(440.0, fs, frames);
        let hop = ((fs as f32) * 0.025) as u64;
        let num_hops = (frames as u64 / hop) as usize + 1;
        let contour = vec![440.0_f32; num_hops];
        apply(
            &Op::Autotune {
                range: r(0, frames as u64),
                params: AutotuneParams {
                    target: AutotuneTarget::Reference {
                        contour_hz: contour,
                        hop_samples: hop,
                    },
                    retune_ms: 0.0,
                    preserve_formants: true,
                },
            },
            &mut samples,
            1,
            fs,
        )
        .unwrap();
        let body = &samples[4096..samples.len() - 4096];
        let f = dominant_freq(body, fs);
        assert!((f - 440.0).abs() < 4.0, "expected ~440 Hz, got {f}");
    }
}
