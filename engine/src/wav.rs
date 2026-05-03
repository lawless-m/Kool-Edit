//! WAV file decoding.
//!
//! Wraps `hound` so callers don't need to know about its sample-format
//! variants. Returns interleaved float32 samples in the [-1, 1] range, plus
//! the metadata the engine needs to register the source.

use std::io::Cursor;

#[derive(Debug)]
pub struct DecodedWav {
    pub channel_count: u16,
    pub sample_rate: u32,
    /// Interleaved samples in [-1.0, 1.0]. `samples.len() == frames * channels`.
    pub samples: Vec<f32>,
    pub frames: u64,
}

#[derive(Debug)]
pub enum WavError {
    Read(hound::Error),
    UnsupportedBitDepth(u16),
    UnsupportedChannelCount(u16),
}

impl std::fmt::Display for WavError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WavError::Read(e) => write!(f, "wav read error: {e}"),
            WavError::UnsupportedBitDepth(b) => write!(f, "unsupported bit depth: {b}"),
            WavError::UnsupportedChannelCount(c) => {
                write!(f, "unsupported channel count: {c} (only mono and stereo)")
            }
        }
    }
}

impl std::error::Error for WavError {}

impl From<hound::Error> for WavError {
    fn from(e: hound::Error) -> Self {
        WavError::Read(e)
    }
}

pub fn decode(bytes: &[u8]) -> Result<DecodedWav, WavError> {
    let cursor = Cursor::new(bytes);
    let mut reader = hound::WavReader::new(cursor)?;
    let spec = reader.spec();

    if spec.channels == 0 || spec.channels > 2 {
        return Err(WavError::UnsupportedChannelCount(spec.channels));
    }

    let samples = match spec.sample_format {
        hound::SampleFormat::Float => match spec.bits_per_sample {
            32 => reader
                .samples::<f32>()
                .collect::<Result<Vec<f32>, _>>()?,
            other => return Err(WavError::UnsupportedBitDepth(other)),
        },
        hound::SampleFormat::Int => match spec.bits_per_sample {
            16 => reader
                .samples::<i16>()
                .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
                .collect::<Result<Vec<f32>, _>>()?,
            24 => reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / 8_388_608.0))
                .collect::<Result<Vec<f32>, _>>()?,
            32 => reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / i32::MAX as f32))
                .collect::<Result<Vec<f32>, _>>()?,
            other => return Err(WavError::UnsupportedBitDepth(other)),
        },
    };

    let frames = (samples.len() as u64) / spec.channels as u64;
    Ok(DecodedWav {
        channel_count: spec.channels,
        sample_rate: spec.sample_rate,
        samples,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth_wav(spec: hound::WavSpec, samples: &[f32]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::<u8>::new());
        {
            let mut writer = hound::WavWriter::new(&mut buf, spec).unwrap();
            match spec.sample_format {
                hound::SampleFormat::Float => {
                    for &s in samples {
                        writer.write_sample(s).unwrap();
                    }
                }
                hound::SampleFormat::Int => {
                    for &s in samples {
                        let scaled = (s * i16::MAX as f32) as i16;
                        writer.write_sample(scaled).unwrap();
                    }
                }
            }
            writer.finalize().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn decodes_mono_float32_wav() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44_100,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let bytes = synth_wav(spec, &[0.0, 0.5, -0.5, 1.0, -1.0]);
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.channel_count, 1);
        assert_eq!(decoded.sample_rate, 44_100);
        assert_eq!(decoded.frames, 5);
        assert_eq!(decoded.samples.len(), 5);
        assert!((decoded.samples[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn decodes_stereo_int16_wav() {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        // 4 frames × 2 channels = 8 interleaved samples
        let bytes = synth_wav(
            spec,
            &[0.0, 0.0, 0.5, -0.5, -0.25, 0.25, 1.0, -1.0],
        );
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.channel_count, 2);
        assert_eq!(decoded.sample_rate, 48_000);
        assert_eq!(decoded.frames, 4);
        assert_eq!(decoded.samples.len(), 8);
        assert!((decoded.samples[2] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn rejects_unsupported_channel_count() {
        // hound can't write 6-channel without our help, so build the smallest
        // possible 4-channel WAV manually via hound and then decode.
        let spec = hound::WavSpec {
            channels: 6,
            sample_rate: 48_000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let bytes = synth_wav(spec, &[0.0; 12]);
        let err = decode(&bytes).unwrap_err();
        assert!(matches!(err, WavError::UnsupportedChannelCount(6)));
    }
}
