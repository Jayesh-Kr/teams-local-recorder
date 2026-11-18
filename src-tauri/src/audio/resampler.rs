use rubato::{FftFixedIn, Resampler};
use tracing::warn;

/// Unified output sample rate for both microphone and playback loopback channels.
pub const TARGET_SAMPLE_RATE: u32 = 48_000;

/// Supported input PCM/float formats reported by WASAPI `GetMixFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleEncoding {
    Float32,
    Pcm16,
    Pcm24,
    Pcm32,
}

/// Converts raw interleaved WASAPI sample bytes into normalized mono `f32` samples in `[-1.0, 1.0]`.
pub fn downmix_to_mono_f32(
    raw_bytes: &[u8],
    channels: usize,
    encoding: SampleEncoding,
    out: &mut Vec<f32>,
) {
    if channels == 0 || raw_bytes.is_empty() {
        return;
    }

    let bytes_per_sample = match encoding {
        SampleEncoding::Float32 | SampleEncoding::Pcm32 => 4,
        SampleEncoding::Pcm24 => 3,
        SampleEncoding::Pcm16 => 2,
    };
    let frame_bytes = bytes_per_sample * channels;
    if frame_bytes == 0 {
        return;
    }

    let frame_count = raw_bytes.len() / frame_bytes;
    out.reserve(frame_count);
    let inv_channels = 1.0_f32 / (channels as f32);

    for frame in raw_bytes.chunks_exact(frame_bytes) {
        let mut sum = 0.0_f32;
        for sample_bytes in frame.chunks_exact(bytes_per_sample) {
            let sample = match encoding {
                SampleEncoding::Float32 => {
                    let v = f32::from_le_bytes([
                        sample_bytes[0],
                        sample_bytes[1],
                        sample_bytes[2],
                        sample_bytes[3],
                    ]);
                    if v.is_finite() {
                        v
                    } else {
                        0.0
                    }
                }
                SampleEncoding::Pcm16 => {
                    let v = i16::from_le_bytes([sample_bytes[0], sample_bytes[1]]);
                    (v as f32) / 32768.0_f32
                }
                SampleEncoding::Pcm24 => {
                    // Sign-extend 24-bit little-endian signed integer to i32.
                    let sign = if (sample_bytes[2] & 0x80) != 0 {
                        0xFF
                    } else {
                        0x00
                    };
                    let v = i32::from_le_bytes([
                        sample_bytes[0],
                        sample_bytes[1],
                        sample_bytes[2],
                        sign,
                    ]);
                    (v as f32) / 8_388_608.0_f32
                }
                SampleEncoding::Pcm32 => {
                    let v = i32::from_le_bytes([
                        sample_bytes[0],
                        sample_bytes[1],
                        sample_bytes[2],
                        sample_bytes[3],
                    ]);
                    (v as f32) / 2_147_483_648.0_f32
                }
            };
            sum += sample;
        }
        out.push((sum * inv_channels).clamp(-1.0, 1.0));
    }
}

enum ResamplerImpl {
    PassThrough,
    Fft {
        resampler: FftFixedIn<f32>,
        chunk_size: usize,
        pending: Vec<f32>,
        scratch_out: Vec<Vec<f32>>,
    },
    Linear {
        step: f64,
        pos: f64,
        prev_sample: f32,
        has_prev: bool,
    },
}

/// Converts a mono `f32` audio stream from an arbitrary device sample rate to 48 kHz.
pub struct MonoResampler {
    inner: ResamplerImpl,
}

impl MonoResampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        if input_rate == 0 || output_rate == 0 || input_rate == output_rate {
            return Self {
                inner: ResamplerImpl::PassThrough,
            };
        }

        const CHUNK_SIZE: usize = 1024;
        match FftFixedIn::<f32>::new(
            input_rate as usize,
            output_rate as usize,
            CHUNK_SIZE,
            2,
            1,
        ) {
            Ok(resampler) => {
                let scratch_out = resampler.output_buffer_allocate(true);
                Self {
                    inner: ResamplerImpl::Fft {
                        resampler,
                        chunk_size: CHUNK_SIZE,
                        pending: Vec::with_capacity(CHUNK_SIZE * 2),
                        scratch_out,
                    },
                }
            }
            Err(err) => {
                warn!(
                    input_rate,
                    output_rate,
                    "FftFixedIn init failed ({err}); falling back to linear resampler"
                );
                Self {
                    inner: ResamplerImpl::Linear {
                        step: (input_rate as f64) / (output_rate as f64),
                        pos: 0.0,
                        prev_sample: 0.0,
                        has_prev: false,
                    },
                }
            }
        }
    }

    /// Resamples `input` mono samples and appends the resulting 48 kHz samples to `output`.
    pub fn process(&mut self, input: &[f32], output: &mut Vec<f32>) {
        if input.is_empty() {
            return;
        }

        match &mut self.inner {
            ResamplerImpl::PassThrough => {
                output.extend_from_slice(input);
            }
            ResamplerImpl::Fft {
                resampler,
                chunk_size,
                pending,
                scratch_out,
            } => {
                pending.extend_from_slice(input);
                while pending.len() >= *chunk_size {
                    let chunk = &pending[..*chunk_size];
                    let in_slice = [chunk];
                    match resampler.process_into_buffer(&in_slice, scratch_out, None) {
                        Ok((_, out_len)) => {
                            output.extend_from_slice(&scratch_out[0][..out_len]);
                        }
                        Err(err) => {
                            warn!("Rubato resampling error (dropping chunk): {err}");
                        }
                    }
                    pending.drain(..*chunk_size);
                }
            }
            ResamplerImpl::Linear {
                step,
                pos,
                prev_sample,
                has_prev,
            } => {
                if !*has_prev {
                    *prev_sample = input[0];
                    *has_prev = true;
                }

                // Samples indexed as: index 0 -> prev_sample, index 1..=len -> input[i-1]
                let total = input.len();
                let mut cursor = *pos;
                while cursor < total as f64 {
                    let idx0 = cursor.floor() as usize;
                    let frac = (cursor - (idx0 as f64)) as f32;
                    let s0 = if idx0 == 0 {
                        *prev_sample
                    } else {
                        input[idx0 - 1]
                    };
                    let s1 = if idx0 < total {
                        input[idx0]
                    } else {
                        input[total - 1]
                    };
                    output.push(s0 + (s1 - s0) * frac);
                    cursor += *step;
                }
                *pos = cursor - (total as f64);
                *prev_sample = input[total - 1];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_stereo_pcm16_to_mono() {
        // Frame 1: L=+16384 (0.5), R=-16384 (-0.5) => mono 0.0
        // Frame 2: L=+16384 (0.5), R=+16384 (+0.5) => mono 0.5
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&16384_i16.to_le_bytes());
        bytes.extend_from_slice(&(-16384_i16).to_le_bytes());
        bytes.extend_from_slice(&16384_i16.to_le_bytes());
        bytes.extend_from_slice(&16384_i16.to_le_bytes());

        let mut mono = Vec::new();
        downmix_to_mono_f32(&bytes, 2, SampleEncoding::Pcm16, &mut mono);

        assert_eq!(mono.len(), 2);
        assert!(mono[0].abs() < 1e-4);
        assert!((mono[1] - 0.5).abs() < 1e-4);
    }

    #[test]
    fn passthrough_resampler_preserves_48khz_samples() {
        let mut resampler = MonoResampler::new(48_000, TARGET_SAMPLE_RATE);
        let input = vec![0.1, -0.2, 0.3, 0.4];
        let mut output = Vec::new();
        resampler.process(&input, &mut output);
        assert_eq!(output, input);
    }

    #[test]
    fn resamples_44100_to_48000_with_expected_length_ratio() {
        let mut resampler = MonoResampler::new(44_100, TARGET_SAMPLE_RATE);
        let input_len = 44_100usize;
        let input: Vec<f32> = (0..input_len)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * (i as f32) / 44_100.0).sin())
            .collect();

        let mut output = Vec::new();
        for chunk in input.chunks(441) {
            resampler.process(chunk, &mut output);
        }

        // Expect close to 48,000 samples (minus up to one pending FFT input chunk).
        assert!(output.len() > 46_000 && output.len() <= 48_500);
    }
}
