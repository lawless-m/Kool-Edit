//! Spectral selection and edit data. Per `01-feature-spec.md` spectral edits
//! bake on apply (no re-editable spectral layer in v1), so these types are
//! purely the description of an edit applied to STFT bins.

use crate::range::SampleRange;

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum WindowKind {
    Hann,
    Hamming,
    Blackman,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub struct StftParams {
    pub fft_size: u32,
    pub hop_size: u32,
    pub window: WindowKind,
}

impl StftParams {
    /// Project default per `02-architecture.md`: 2048 / 512 with Hann.
    pub const DEFAULT: Self = Self {
        fft_size: 2048,
        hop_size: 512,
        window: WindowKind::Hann,
    };
}

/// Time-frequency selection. Time is in sample frames; frequency in hertz.
/// Lasso and wand selections aren't yet expressible — only rectangular for now;
/// the doc 03 type is intentionally extensible so the variant will grow.
#[derive(Clone, Debug, PartialEq)]
pub enum TimeFreqRegion {
    Rect {
        time: SampleRange,
        freq_low_hz: f32,
        freq_high_hz: f32,
    },
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum SpectralOp {
    Attenuate { db: f32 },
    Amplify { db: f32 },
    Silence,
    Repair,
}
