use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use hound::{SampleFormat, WavSpec, WavWriter};
use ringbuf::traits::{Consumer, Observer};
use ringbuf::HeapCons;
use tracing::{info, warn};

use crate::audio::resampler::TARGET_SAMPLE_RATE;
use crate::error::AppResult;

/// 10 ms interleaving quantum at 48 kHz = 480 stereo frames.
const MIN_TICK_FRAMES: usize = 240;
const MAX_TICK_FRAMES: usize = 4_800; // 100 ms cap against OS scheduling jitter
const MAX_QUEUE_BACKLOG_FRAMES: usize = 9_600; // 200 ms clock-skew ceiling
const TARGET_QUEUE_AFTER_TRIM: usize = 4_800; // 100 ms after trimming skew
const FLUSH_INTERVAL_FRAMES: u64 = (TARGET_SAMPLE_RATE as u64) * 2; // Flush every ~2s
const WRITER_SLEEP: Duration = Duration::from_millis(5);

/// RAII wrapper around `hound::WavWriter` that guarantees `finalize()` runs on drop,
/// ensuring the WAV header chunk sizes are always valid even on abrupt shutdown.
pub struct WavWriterGuard {
    writer: Option<WavWriter<BufWriter<File>>>,
}

impl WavWriterGuard {
    pub fn create(path: &Path) -> AppResult<Self> {
        let spec = WavSpec {
            channels: 2,
            sample_rate: TARGET_SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };
        let writer = WavWriter::create(path, spec)?;
        Ok(Self {
            writer: Some(writer),
        })
    }

    pub fn write_sample(&mut self, sample: i16) -> AppResult<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.write_sample(sample)?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> AppResult<()> {
        if let Some(writer) = self.writer.as_mut() {
            writer.flush()?;
        }
        Ok(())
    }

    pub fn finalize(mut self) -> AppResult<()> {
        if let Some(writer) = self.writer.take() {
            writer.finalize()?;
        }
        Ok(())
    }
}

impl Drop for WavWriterGuard {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            if let Err(err) = writer.finalize() {
                warn!("Failed to finalize WAV header in Drop: {err}");
            }
        }
    }
}

/// Converts a normalized `f32` audio sample in `[-1.0, 1.0]` to 16-bit signed PCM.
#[inline]
pub fn f32_to_i16(sample: f32) -> i16 {
    if !sample.is_finite() {
        return 0;
    }
    (sample.clamp(-1.0, 1.0) * 32767.0_f32).round() as i16
}

/// Pulls `count` frames from a lock-free ring buffer, trimming excessive backlog on clock drift
/// and padding trailing `0.0` silence on underrun so Left and Right stay strictly time-aligned.
pub fn read_aligned_frames(cons: &mut HeapCons<f32>, buf: &mut [f32]) {
    let count = buf.len();
    if count == 0 {
        return;
    }

    let occupied = cons.occupied_len();
    if occupied > MAX_QUEUE_BACKLOG_FRAMES {
        let to_drop = occupied.saturating_sub(TARGET_QUEUE_AFTER_TRIM);
        let _ = cons.skip(to_drop);
    }

    let read = cons.pop_slice(buf);
    if read < count {
        buf[read..].fill(0.0);
    }
}

/// Writes one aligned block of stereo `[Left (Mic), Right (Loopback)]` samples to the WAV file.
/// When `mic_muted` is true, writes `0` to Channel 0 (Left) while continuing to record Channel 1 (Right).
pub fn write_stereo_block(
    writer: &mut WavWriterGuard,
    left_mic: &[f32],
    right_loopback: &[f32],
    mic_muted: bool,
) -> AppResult<()> {
    let frames = left_mic.len().min(right_loopback.len());
    for i in 0..frames {
        let left_sample = if mic_muted { 0 } else { f32_to_i16(left_mic[i]) };
        let right_sample = f32_to_i16(right_loopback[i]);
        writer.write_sample(left_sample)?;
        writer.write_sample(right_sample)?;
    }
    Ok(())
}

/// Runs the clock-paced stereo interleaver thread until `stop_flag` is set, then finalizes `recording.wav`.
pub fn run_stereo_writer_thread(
    mut writer: WavWriterGuard,
    mut mic_cons: HeapCons<f32>,
    mut loopback_cons: HeapCons<f32>,
    mic_muted: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
) {
    info!("Stereo WAV writer thread started");

    let mut left_buf = vec![0.0_f32; MAX_TICK_FRAMES];
    let mut right_buf = vec![0.0_f32; MAX_TICK_FRAMES];

    let mut clock_origin = Instant::now();
    let mut clock_frames: u64 = 0;
    let mut frames_since_flush: u64 = 0;
    let mut was_paused = false;

    while !stop_flag.load(Ordering::Acquire) {
        if paused.load(Ordering::Acquire) {
            // Drain both ring buffers while paused so resuming produces fresh, unbuffered audio.
            let mic_avail = mic_cons.occupied_len();
            if mic_avail > 0 {
                let _ = mic_cons.skip(mic_avail);
            }
            let loop_avail = loopback_cons.occupied_len();
            if loop_avail > 0 {
                let _ = loopback_cons.skip(loop_avail);
            }
            was_paused = true;
            thread::sleep(Duration::from_millis(10));
            continue;
        }

        if was_paused {
            clock_origin = Instant::now();
            clock_frames = 0;
            was_paused = false;
        }

        let target_total_frames =
            (clock_origin.elapsed().as_secs_f64() * (TARGET_SAMPLE_RATE as f64)) as u64;
        let due_frames = target_total_frames.saturating_sub(clock_frames) as usize;

        if due_frames < MIN_TICK_FRAMES {
            thread::sleep(WRITER_SLEEP);
            continue;
        }

        let frames_to_write = due_frames.min(MAX_TICK_FRAMES);
        let left_slice = &mut left_buf[..frames_to_write];
        let right_slice = &mut right_buf[..frames_to_write];

        read_aligned_frames(&mut mic_cons, left_slice);
        read_aligned_frames(&mut loopback_cons, right_slice);

        let is_muted = mic_muted.load(Ordering::Acquire);
        if let Err(err) = write_stereo_block(&mut writer, left_slice, right_slice, is_muted) {
            warn!("Error writing stereo samples to WAV (continuing): {err}");
        }

        clock_frames = clock_frames.saturating_add(frames_to_write as u64);
        frames_since_flush = frames_since_flush.saturating_add(frames_to_write as u64);

        if frames_since_flush >= FLUSH_INTERVAL_FRAMES {
            if let Err(err) = writer.flush() {
                warn!("Periodic WAV flush failed: {err}");
            }
            frames_since_flush = 0;
        }
    }

    if let Err(err) = writer.finalize() {
        warn!("Failed to finalize WAV on stop: {err}");
    }

    info!("Stereo WAV writer thread stopped and finalized WAV");
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::WavReader;
    use ringbuf::traits::{Producer, Split};
    use ringbuf::HeapRb;
    use tempfile::tempdir;

    #[test]
    fn interleaves_channels_pads_silence_and_honors_mic_mute() {
        let dir = tempdir().expect("tempdir");
        let wav_path = dir.path().join("recording.wav");

        let rb_mic = HeapRb::<f32>::new(128);
        let rb_loop = HeapRb::<f32>::new(128);
        let (mut mic_prod, mut mic_cons) = rb_mic.split();
        let (mut loop_prod, mut loop_cons) = rb_loop.split();

        // Push 2 mic samples and 4 loopback samples (mic underruns on frames 2 & 3).
        mic_prod.push_slice(&[0.5, 0.5]);
        loop_prod.push_slice(&[-0.25, -0.25, 0.75, 0.75]);

        let mut left = [0.0_f32; 4];
        let mut right = [0.0_f32; 4];
        read_aligned_frames(&mut mic_cons, &mut left);
        read_aligned_frames(&mut loop_cons, &mut right);

        {
            let mut guard = WavWriterGuard::create(&wav_path).expect("create wav");
            // Unmuted block: L=[0.5, 0.5, 0.0, 0.0], R=[-0.25, -0.25, 0.75, 0.75]
            write_stereo_block(&mut guard, &left, &right, false).expect("write unmuted");
            // Muted block: L should be 0 while R continues recording!
            write_stereo_block(&mut guard, &[0.8, 0.8], &[0.5, -0.5], true).expect("write muted");
            guard.finalize().expect("finalize wav");
        }

        let mut reader = WavReader::open(&wav_path).expect("open wav");
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.spec().bits_per_sample, 16);

        let samples: Vec<i16> = reader
            .samples::<i16>()
            .map(|s| s.expect("sample"))
            .collect();
        // 6 stereo frames = 12 i16 samples
        assert_eq!(samples.len(), 12);

        // Frame 0: L ~ +16384, R ~ -8192
        assert!(samples[0] > 16000);
        assert!(samples[1] < -8000);
        // Frame 2 (mic underrun padded with silence): L == 0, R ~ +24575
        assert_eq!(samples[4], 0);
        assert!(samples[5] > 24000);
        // Frame 4 & 5 (mic muted): L == 0, R still captured!
        assert_eq!(samples[8], 0);
        assert!(samples[9] > 16000);
        assert_eq!(samples[10], 0);
        assert!(samples[11] < -16000);
    }
}
