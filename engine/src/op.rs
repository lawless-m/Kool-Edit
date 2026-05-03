//! Source-level destructive operations. Mirrors the `Op` enum in
//! `03-data-model.md`.
//!
//! Operations are pure data: they describe an edit, they don't perform it.
//! The DSP module turns an `Op` into samples; the engine core just stores them.

use crate::effect::{
    CompParams, DelayParams, EqParams, LimitParams, NrParams, ReverbParams,
};
use crate::ids::{ClipboardRef, ProfileId};
use crate::range::SampleRange;
use crate::spectral::{SpectralOp, StftParams, TimeFreqRegion};

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FadeShape {
    Linear,
    Logarithmic,
    Exponential,
    SCurve,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum FadeDirection {
    In,
    Out,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum NormTarget {
    Peak,
    Rms,
    LufsIntegrated,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum NoiseColor {
    White,
    Pink,
    Brown,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ToneShape {
    Sine,
    Square,
    Saw,
    Triangle,
}

#[derive(Clone, Debug, PartialEq)]
pub enum GeneratorParams {
    Silence,
    Tone {
        shape: ToneShape,
        frequency_hz: f32,
        amplitude_db: f32,
    },
    Noise {
        color: NoiseColor,
        amplitude_db: f32,
    },
    Dtmf {
        digits: String,
    },
    Sweep {
        start_hz: f32,
        end_hz: f32,
        amplitude_db: f32,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Silence {
        range: SampleRange,
    },
    Gain {
        range: SampleRange,
        db: f32,
    },
    Fade {
        range: SampleRange,
        shape: FadeShape,
        direction: FadeDirection,
    },
    Normalize {
        range: SampleRange,
        target: NormTarget,
        value_db: f32,
    },
    Reverse {
        range: SampleRange,
    },
    DcRemove {
        range: SampleRange,
    },

    Cut {
        range: SampleRange,
    },
    Insert {
        at: u64,
        samples_ref: ClipboardRef,
    },
    PasteMix {
        at: u64,
        samples_ref: ClipboardRef,
    },
    PasteOver {
        at: u64,
        samples_ref: ClipboardRef,
        crossfade_samples: u64,
    },

    Eq {
        range: SampleRange,
        params: EqParams,
    },
    Compress {
        range: SampleRange,
        params: CompParams,
    },
    Limit {
        range: SampleRange,
        params: LimitParams,
    },
    Reverb {
        range: SampleRange,
        params: ReverbParams,
    },
    Delay {
        range: SampleRange,
        params: DelayParams,
    },

    TimeStretch {
        range: SampleRange,
        ratio: f32,
    },
    PitchShift {
        range: SampleRange,
        cents: f32,
    },

    NoiseReduce {
        range: SampleRange,
        profile: ProfileId,
        params: NrParams,
    },

    SpectralEdit {
        region: TimeFreqRegion,
        operation: SpectralOp,
        stft: StftParams,
    },

    Generate {
        at: u64,
        length: u64,
        params: GeneratorParams,
    },
}
