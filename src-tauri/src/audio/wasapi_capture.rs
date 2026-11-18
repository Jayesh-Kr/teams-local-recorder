use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use ringbuf::traits::Producer;
use ringbuf::HeapProd;
use tracing::{debug, info, warn};
use windows::core::w;
use windows::Win32::Foundation::{HANDLE, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_E_SERVICE_NOT_RUNNING, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

use crate::audio::resampler::{downmix_to_mono_f32, MonoResampler, SampleEncoding, TARGET_SAMPLE_RATE};
use crate::error::{AppError, AppResult};

/// 40 ms shared-mode WASAPI ring buffer duration in 100-nanosecond units.
const BUFFER_DURATION_HNS: i64 = 400_000;
const POLL_SLEEP: Duration = Duration::from_millis(10);
const RETRY_DELAY: Duration = Duration::from_millis(350);

/// Selects whether this capture stream reads the default microphone or default render endpoint loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// Channel 0 (Left): Default Windows Microphone (`eCapture`, `eCommunications` -> `eConsole`).
    Microphone,
    /// Channel 1 (Right): Default Windows Playback Endpoint Loopback (`eRender` + `AUDCLNT_STREAMFLAGS_LOOPBACK`).
    /// Never uses per-process loopback because New Teams uses `AudioCategory_Communications`.
    SystemLoopback,
}

impl CaptureMode {
    pub fn label(self) -> &'static str {
        match self {
            CaptureMode::Microphone => "microphone",
            CaptureMode::SystemLoopback => "endpoint-loopback",
        }
    }
}

/// RAII guard for COM initialization on the WASAPI capture thread.
struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init_multithreaded() -> AppResult<Self> {
        // SAFETY: Initializing COM MTA once on this dedicated capture thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(Self {
                should_uninit: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self {
                should_uninit: false,
            })
        } else {
            Err(AppError::AudioInit(format!(
                "CoInitializeEx failed on WASAPI thread: {hr:?}"
            )))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Paired with a successful `CoInitializeEx` on this same thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// RAII guard for Windows MMCSS `"Pro Audio"` thread priority.
struct MmcssGuard(Option<HANDLE>);

impl MmcssGuard {
    fn register_pro_audio() -> Self {
        let mut task_index = 0u32;
        // SAFETY: Registers current thread with MMCSS "Pro Audio" profile for low-jitter capture.
        match unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) } {
            Ok(handle) if !handle.is_invalid() => Self(Some(handle)),
            Ok(_) => Self(None),
            Err(err) => {
                warn!("AvSetMmThreadCharacteristicsW('Pro Audio') failed (continuing): {err}");
                Self(None)
            }
        }
    }
}

impl Drop for MmcssGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            // SAFETY: Handle came from `AvSetMmThreadCharacteristicsW` on this thread.
            unsafe {
                let _ = AvRevertMmThreadCharacteristics(handle);
            }
        }
    }
}

/// RAII guard freeing memory allocated by WASAPI `GetMixFormat`.
struct CoTaskMemGuard(*mut WAVEFORMATEX);

impl Drop for CoTaskMemGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: Pointer returned by `IAudioClient::GetMixFormat` must be freed with `CoTaskMemFree`.
            unsafe {
                CoTaskMemFree(Some(self.0 as *const c_void));
            }
        }
    }
}

/// RAII guard ensuring `IAudioClient::Stop()` is always invoked on teardown.
struct AudioClientGuard {
    client: IAudioClient,
    started: bool,
}

impl AudioClientGuard {
    fn new(client: IAudioClient) -> Self {
        Self {
            client,
            started: false,
        }
    }

    fn start(&mut self) -> AppResult<()> {
        // SAFETY: `IAudioClient` was initialized prior to calling `Start`.
        unsafe {
            self.client.Start()?;
        }
        self.started = true;
        Ok(())
    }
}

impl Drop for AudioClientGuard {
    fn drop(&mut self) {
        if self.started {
            // SAFETY: Stopping a started `IAudioClient` during RAII teardown.
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ParsedFormat {
    sample_rate: u32,
    channels: usize,
    encoding: SampleEncoding,
    block_align: usize,
}

/// Runs a resilient WASAPI capture loop on the current dedicated OS thread until `stop_flag` is set.
pub fn run_capture_thread(
    mode: CaptureMode,
    mut producer: HeapProd<f32>,
    stop_flag: Arc<AtomicBool>,
) {
    let _com = match ComGuard::init_multithreaded() {
        Ok(guard) => guard,
        Err(err) => {
            warn!(mode = mode.label(), "Failed to initialize COM: {err}");
            return;
        }
    };
    let _mmcss = MmcssGuard::register_pro_audio();

    info!(mode = mode.label(), "WASAPI capture thread started");

    while !stop_flag.load(Ordering::Acquire) {
        match run_single_device_session(mode, &mut producer, &stop_flag) {
            Ok(()) => break,
            Err(err) => {
                if stop_flag.load(Ordering::Acquire) {
                    break;
                }
                warn!(
                    mode = mode.label(),
                    "WASAPI capture session interrupted ({err}); retrying device open"
                );
                sleep_interruptible(RETRY_DELAY, &stop_flag);
            }
        }
    }

    info!(mode = mode.label(), "WASAPI capture thread stopped");
}

fn run_single_device_session(
    mode: CaptureMode,
    producer: &mut HeapProd<f32>,
    stop_flag: &AtomicBool,
) -> AppResult<()> {
    let device = acquire_default_endpoint(mode)?;

    // SAFETY: Activating `IAudioClient` on a valid `IMMDevice` within our COM apartment.
    let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };

    let mix_fmt_ptr = unsafe { audio_client.GetMixFormat()? };
    if mix_fmt_ptr.is_null() {
        return Err(AppError::AudioInit(format!(
            "GetMixFormat returned null for {}",
            mode.label()
        )));
    }
    let _fmt_guard = CoTaskMemGuard(mix_fmt_ptr);
    let parsed_fmt = parse_wave_format(mix_fmt_ptr)?;

    let stream_flags = match mode {
        CaptureMode::Microphone => 0u32,
        CaptureMode::SystemLoopback => AUDCLNT_STREAMFLAGS_LOOPBACK,
    };

    // SAFETY: `mix_fmt_ptr` is valid for the duration of `_fmt_guard`.
    unsafe {
        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            stream_flags,
            BUFFER_DURATION_HNS,
            0,
            mix_fmt_ptr,
            None,
        )?;
    }

    let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };
    let mut client_guard = AudioClientGuard::new(audio_client);
    client_guard.start()?;

    debug!(
        mode = mode.label(),
        sample_rate = parsed_fmt.sample_rate,
        channels = parsed_fmt.channels,
        encoding = ?parsed_fmt.encoding,
        "WASAPI stream active"
    );

    let mut resampler = MonoResampler::new(parsed_fmt.sample_rate, TARGET_SAMPLE_RATE);
    let mut mono_scratch: Vec<f32> = Vec::with_capacity(4096);
    let mut resampled_scratch: Vec<f32> = Vec::with_capacity(4096);

    while !stop_flag.load(Ordering::Acquire) {
        let mut drained_any = false;

        loop {
            if stop_flag.load(Ordering::Acquire) {
                return Ok(());
            }

            let packet_frames = match unsafe { capture_client.GetNextPacketSize() } {
                Ok(n) => n,
                Err(err) if is_device_lost_error(&err) => {
                    return Err(AppError::AudioCapture(format!(
                        "Audio endpoint invalidated for {}: {err}",
                        mode.label()
                    )));
                }
                Err(err) => {
                    return Err(AppError::AudioCapture(format!(
                        "GetNextPacketSize failed for {}: {err}",
                        mode.label()
                    )));
                }
            };

            if packet_frames == 0 {
                break;
            }
            drained_any = true;

            let mut data_ptr: *mut u8 = ptr::null_mut();
            let mut num_frames_available: u32 = 0;
            let mut flags: u32 = 0;

            match unsafe {
                capture_client.GetBuffer(
                    &mut data_ptr,
                    &mut num_frames_available,
                    &mut flags,
                    None,
                    None,
                )
            } {
                Ok(()) => {}
                Err(err) if is_device_lost_error(&err) => {
                    return Err(AppError::AudioCapture(format!(
                        "Audio endpoint lost during GetBuffer for {}: {err}",
                        mode.label()
                    )));
                }
                Err(err) => {
                    return Err(AppError::AudioCapture(format!(
                        "GetBuffer failed for {}: {err}",
                        mode.label()
                    )));
                }
            }

            mono_scratch.clear();
            let is_silent = (flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)) != 0;
            if is_silent || data_ptr.is_null() {
                mono_scratch.resize(num_frames_available as usize, 0.0);
            } else {
                let byte_len = (num_frames_available as usize) * parsed_fmt.block_align;
                // SAFETY: WASAPI guarantees `data_ptr` is readable for `num_frames_available * block_align` bytes until `ReleaseBuffer`.
                let raw_slice = unsafe { std::slice::from_raw_parts(data_ptr, byte_len) };
                downmix_to_mono_f32(
                    raw_slice,
                    parsed_fmt.channels,
                    parsed_fmt.encoding,
                    &mut mono_scratch,
                );
            }

            // Always release the WASAPI buffer immediately after copying/converting.
            if let Err(err) = unsafe { capture_client.ReleaseBuffer(num_frames_available) } {
                return Err(AppError::AudioCapture(format!(
                    "ReleaseBuffer failed for {}: {err}",
                    mode.label()
                )));
            }

            resampled_scratch.clear();
            resampler.process(&mono_scratch, &mut resampled_scratch);

            if !resampled_scratch.is_empty() {
                let _ = producer.push_slice(&resampled_scratch);
            }
        }

        if !drained_any {
            thread::sleep(POLL_SLEEP);
        }
    }

    Ok(())
}

fn acquire_default_endpoint(mode: CaptureMode) -> AppResult<IMMDevice> {
    // SAFETY: Creating `MMDeviceEnumerator` in the current COM apartment.
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let data_flow = match mode {
        CaptureMode::Microphone => eCapture,
        CaptureMode::SystemLoopback => eRender,
    };

    // Prefer `eCommunications` (Teams call headset if configured), falling back to `eConsole`.
    unsafe {
        enumerator
            .GetDefaultAudioEndpoint(data_flow, eCommunications)
            .or_else(|_| enumerator.GetDefaultAudioEndpoint(data_flow, eConsole))
            .map_err(|err| {
                AppError::AudioInit(format!(
                    "No default {} audio endpoint available: {err}",
                    mode.label()
                ))
            })
    }
}

fn parse_wave_format(fmt_ptr: *const WAVEFORMATEX) -> AppResult<ParsedFormat> {
    // SAFETY: `fmt_ptr` is non-null and points to a valid `WAVEFORMATEX` returned by WASAPI.
    let base = unsafe { ptr::read_unaligned(fmt_ptr) };
    let sample_rate = base.nSamplesPerSec;
    let channels = base.nChannels as usize;
    let bits = base.wBitsPerSample;
    let block_align = base.nBlockAlign as usize;
    let format_tag = base.wFormatTag as u32;

    if sample_rate == 0 || channels == 0 || block_align == 0 {
        return Err(AppError::AudioInit(
            "WASAPI returned invalid zero-valued mix format".to_string(),
        ));
    }

    let encoding = if format_tag == WAVE_FORMAT_IEEE_FLOAT {
        SampleEncoding::Float32
    } else if format_tag == WAVE_FORMAT_PCM {
        match bits {
            16 => SampleEncoding::Pcm16,
            24 => SampleEncoding::Pcm24,
            32 => SampleEncoding::Pcm32,
            other => {
                return Err(AppError::AudioInit(format!(
                    "Unsupported PCM bit depth: {other}"
                )));
            }
        }
    } else if format_tag == WAVE_FORMAT_EXTENSIBLE {
        // SAFETY: When `wFormatTag == WAVE_FORMAT_EXTENSIBLE`, WASAPI allocates a full `WAVEFORMATEXTENSIBLE`.
        let ext = unsafe { ptr::read_unaligned(fmt_ptr as *const WAVEFORMATEXTENSIBLE) };
        let sub_format = ext.SubFormat;
        if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
            SampleEncoding::Float32
        } else if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
            match bits {
                16 => SampleEncoding::Pcm16,
                24 => SampleEncoding::Pcm24,
                32 => SampleEncoding::Pcm32,
                other => {
                    return Err(AppError::AudioInit(format!(
                        "Unsupported extensible PCM bit depth: {other}"
                    )));
                }
            }
        } else {
            return Err(AppError::AudioInit(format!(
                "Unsupported WAVEFORMATEXTENSIBLE SubFormat: {sub_format:?}"
            )));
        }
    } else {
        return Err(AppError::AudioInit(format!(
            "Unsupported WASAPI wFormatTag: {format_tag}"
        )));
    };

    Ok(ParsedFormat {
        sample_rate,
        channels,
        encoding,
        block_align,
    })
}

fn is_device_lost_error(err: &windows::core::Error) -> bool {
    let code = err.code();
    code == AUDCLNT_E_DEVICE_INVALIDATED || code == AUDCLNT_E_SERVICE_NOT_RUNNING
}

fn sleep_interruptible(total: Duration, stop_flag: &AtomicBool) {
    let step = Duration::from_millis(25);
    let mut elapsed = Duration::ZERO;
    while elapsed < total && !stop_flag.load(Ordering::Acquire) {
        let remaining = total.saturating_sub(elapsed);
        let chunk = remaining.min(step);
        thread::sleep(chunk);
        elapsed += chunk;
    }
}
