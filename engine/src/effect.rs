//! Effect kinds and parameter blocks shared by destructive `Op`s and
//! multitrack `EffectInstance`s. Per `02-architecture.md` the parameter shape
//! must be uniform — they're the same effects either way, and the structs
//! below are what gets stored in the project.
//!
//! At this stage these are *parameter records* only. The DSP implementations
//! live in a separate module that will be built when audio processing is
//! wired up; the engine core just owns the data.

use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    Eq,
    Compressor,
    Limiter,
    Reverb,
    Delay,
    NoiseReduction,
    TimeStretch,
    PitchShift,
    DcRemove,
    Reverse,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EqBandKind {
    Highpass,
    Lowpass,
    Lowshelf,
    Highshelf,
    Peak,
    Notch,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EqBand {
    pub kind: EqBandKind,
    pub frequency_hz: f32,
    pub gain_db: f32,
    pub q: f32,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
pub struct EqParams {
    pub bands: Vec<EqBand>,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CompParams {
    pub threshold_db: f32,
    pub ratio: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub makeup_db: f32,
    pub knee_db: f32,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LimitParams {
    pub ceiling_db: f32,
    pub lookahead_ms: f32,
    pub release_ms: f32,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReverbModel {
    Room,
    Hall,
    Plate,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReverbParams {
    pub model: ReverbModel,
    pub size: f32,
    pub damping: f32,
    pub mix: f32,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DelayParams {
    pub time_ms: f32,
    pub feedback: f32,
    pub mix: f32,
    pub ping_pong: bool,
    pub feedback_lp_hz: Option<f32>,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NrParams {
    pub amount_db: f32,
    pub floor_db: f32,
    pub oversubtraction: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub freq_smoothing: f32,
    pub fft_size: u32,
}

/// Track and master inserts hold one of these. Each variant carries the
/// typed parameter record used by the matching destructive op, so an EQ
/// insert keeps its band list intact and a compressor insert keeps all of
/// its parameters in one place rather than as loose floats.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum EffectParams {
    Eq(EqParams),
    Compressor(CompParams),
    Limiter(LimitParams),
    Reverb(ReverbParams),
    Delay(DelayParams),
}

impl EffectParams {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Eq(_) => "eq",
            Self::Compressor(_) => "compressor",
            Self::Limiter(_) => "limiter",
            Self::Reverb(_) => "reverb",
            Self::Delay(_) => "delay",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutotuneScale {
    Chromatic,
    Major,
    Minor,
}

/// What pitch the audio should be pulled to. `Scale` snaps to the nearest
/// note in the chosen scale rooted at `key_pc` (0=C, 1=C#, …, 11=B).
/// `Reference` follows a pre-computed pitch contour produced by running
/// pitch detection on another clip; `contour_hz[i]` is the target frequency
/// for input frame `i * hop_samples` (0.0 means "unvoiced — leave alone").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AutotuneTarget {
    Scale {
        scale: AutotuneScale,
        key_pc: u8,
    },
    Reference {
        contour_hz: Vec<f32>,
        hop_samples: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutotuneParams {
    pub target: AutotuneTarget,
    /// Smoothing time-constant in milliseconds. 0 = instant snap (T-Pain
    /// style); larger values let the pitch glide toward target.
    pub retune_ms: f32,
    /// Reserved — formant preservation isn't implemented yet. The slot is
    /// here so a future cepstral / LPC pass can land without a project-file
    /// migration.
    pub preserve_formants: bool,
}
