pub mod resampler;
pub mod stereo_writer;
pub mod wasapi_capture;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ringbuf::traits::Split;
use ringbuf::HeapRb;
use tracing::warn;

use crate::audio::resampler::TARGET_SAMPLE_RATE;
use crate::audio::stereo_writer::{run_stereo_writer_thread, WavWriterGuard};
use crate::audio::wasapi_capture::{run_capture_thread, CaptureMode};
use crate::error::{AppError, AppResult};

/// 2 seconds of mono 48 kHz capacity per lock-free SPSC ring buffer.
const RING_BUFFER_CAPACITY: usize = (TARGET_SAMPLE_RATE as usize) * 2;

/// Safe, RAII handle to an active dual-channel stereo recording session.
/// Dropping `AudioRecorder` cleanly stops all capture threads and finalizes `recording.wav`.
pub struct AudioRecorder {
    stop_flag: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    mic_thread: Option<JoinHandle<()>>,
    loopback_thread: Option<JoinHandle<()>>,
    writer_thread: Option<JoinHandle<()>>,
}

impl AudioRecorder {
    /// Opens `wav_path` and spawns dedicated OS threads for Mic capture (Left),
    /// Endpoint Loopback capture (Right), and stereo WAV disk streaming.
    pub fn start(wav_path: PathBuf, initial_mic_muted: bool) -> AppResult<Self> {
        // Create the WAV writer synchronously so filesystem errors fail fast.
        let writer_guard = WavWriterGuard::create(&wav_path)?;

        let stop_flag = Arc::new(AtomicBool::new(false));
        let mic_muted = Arc::new(AtomicBool::new(initial_mic_muted));
        let paused = Arc::new(AtomicBool::new(false));

        let (mic_prod, mic_cons) = HeapRb::<f32>::new(RING_BUFFER_CAPACITY).split();
        let (loop_prod, loop_cons) = HeapRb::<f32>::new(RING_BUFFER_CAPACITY).split();

        let mic_stop = Arc::clone(&stop_flag);
        let mic_thread = thread::Builder::new()
            .name("wasapi-mic-capture".to_string())
            .spawn(move || run_capture_thread(CaptureMode::Microphone, mic_prod, mic_stop))
            .map_err(|e| AppError::AudioInit(format!("Failed to spawn mic thread: {e}")))?;

        let loop_stop = Arc::clone(&stop_flag);
        let loopback_thread = thread::Builder::new()
            .name("wasapi-loopback-capture".to_string())
            .spawn(move || run_capture_thread(CaptureMode::SystemLoopback, loop_prod, loop_stop))
            .map_err(|e| AppError::AudioInit(format!("Failed to spawn loopback thread: {e}")))?;

        let writer_stop = Arc::clone(&stop_flag);
        let writer_muted = Arc::clone(&mic_muted);
        let writer_paused = Arc::clone(&paused);
        let writer_thread = thread::Builder::new()
            .name("stereo-wav-writer".to_string())
            .spawn(move || {
                run_stereo_writer_thread(
                    writer_guard,
                    mic_cons,
                    loop_cons,
                    writer_muted,
                    writer_paused,
                    writer_stop,
                );
            })
            .map_err(|e| AppError::AudioInit(format!("Failed to spawn writer thread: {e}")))?;

        Ok(Self {
            stop_flag,
            mic_muted,
            paused,
            mic_thread: Some(mic_thread),
            loopback_thread: Some(loopback_thread),
            writer_thread: Some(writer_thread),
        })
    }

    /// Updates local microphone mute state.
    /// When muted, Channel 0 (Left) writes silence while Channel 1 (Right / Remote) continues recording.
    pub fn set_mic_muted(&self, muted: bool) {
        self.mic_muted.store(muted, Ordering::Release);
    }

    /// Pauses or resumes writing frames to disk.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    /// Stops all audio threads and waits for `recording.wav` to be finalized on disk.
    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        self.stop_flag.store(true, Ordering::Release);

        if let Some(handle) = self.mic_thread.take() {
            if let Err(err) = handle.join() {
                warn!("Mic capture thread panicked on join: {err:?}");
            }
        }
        if let Some(handle) = self.loopback_thread.take() {
            if let Err(err) = handle.join() {
                warn!("Loopback capture thread panicked on join: {err:?}");
            }
        }
        if let Some(handle) = self.writer_thread.take() {
            if let Err(err) = handle.join() {
                warn!("Stereo writer thread panicked on join: {err:?}");
            }
        }
    }
}

impl Drop for AudioRecorder {
    fn drop(&mut self) {
        self.stop_inner();
    }
}
