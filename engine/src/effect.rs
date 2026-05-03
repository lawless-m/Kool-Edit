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
