# Teams Local Recorder

A lightweight, 100% local Windows desktop application built with **Rust**, **Tauri v2**, **WASAPI**, and **Windows UI Automation** that automatically detects active calls in the New Microsoft Teams desktop app (`ms-teams.exe`), records dual-channel stereo audio directly to disk, and writes structured meeting metadata locally with zero cloud uploads.

## Features

- **Automatic Teams Call Detection (Windows UI Automation):**
  - Monitors visible `ms-teams.exe` windows every 1.5 seconds on a dedicated `COINIT_MULTITHREADED` background thread.
  - Detects call start/end (`AutomationId == "hangup-button"` or `Button` named `"Leave"`) with end-of-call debouncing.
  - Tracks Microsoft Teams microphone mute state (`AutomationId == "microphone-button"`), meeting window title, and visible participant tiles (`first_seen` and `last_seen`).
- **Dual-Channel Stereo WASAPI Audio Capture:**
  - **Channel 0 (Left):** Default Windows Microphone (`eCapture`, `eCommunications` with fallback to `eConsole`).
  - **Channel 1 (Right):** Default Windows Playback Endpoint Loopback (`eRender` with `AUDCLNT_STREAMFLAGS_LOOPBACK`, `eCommunications` with fallback to `eConsole`). Uses endpoint loopback (never per-process loopback) so New Teams `AudioCategory_Communications` audio is always captured cleanly.
  - Resamples both streams to **48 kHz, 16-bit PCM Stereo** (`rubato` + lock-free `ringbuf` SPSC queues) with monotonic clock alignment and automatic silence padding so channels never drift out of sync.
  - Honors local mic mute (from Teams or the floating pill): writes silence to Channel 0 (Left) while continuing to capture remote participants on Channel 1 (Right).
  - Streams straight to disk via `hound::WavWriter` with RAII header finalization so multi-hour meetings use minimal RAM and WAV headers stay valid even on abrupt exit.
- **System Tray & Floating Pill UI:**
  - Runs silently in the Windows system tray (`Open Recordings Folder`, `Start Manual Recording` / `Stop Recording`, `Auto-Record: Enabled`, `Quit`).
  - Frameless, transparent, draggable, always-on-top floating pill showing recording pulse, live timer, participant count badge, mic mute toggle, pause/resume, and stop controls.

## Local Output Format

Every recording session is saved under:

```text
%USERPROFILE%\Documents\TeamsRecordings\<YYYY-MM-DD_HH-MM-SS>_<sanitized-meeting-title>\
├── recording.wav
└── meeting-details.json
```

`recording.wav` is 48 kHz, 16-bit, 2-channel Stereo PCM (`Left = local_microphone`, `Right = remote_participants`).

## Development

### Prerequisites

- Windows 10 / 11 with Microsoft Edge WebView2 Runtime
- Rust toolchain (`stable-x86_64-pc-windows-msvc`)
- Node.js 20+

### Run Locally

```powershell
npm install
npm run tauri dev
```

### Build Production Installer / Binary

```powershell
npm run tauri build
```

### Run Verification Checks

```powershell
npm run typecheck
npm run build
cargo test --manifest-path src-tauri/Cargo.toml
```
