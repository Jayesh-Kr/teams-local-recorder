# Teams Local Recorder

Teams Local Recorder is a lightweight, local-only Windows desktop application built with **Rust** and **Tauri v2** (with a minimal Vanilla TypeScript and Vite frontend). It runs quietly in the Windows system tray, automatically detects active calls in the New Microsoft Teams desktop application (`ms-teams.exe`), records dual-channel stereo audio directly to disk, and saves structured meeting metadata locally. Nothing is ever uploaded to a server.

---

## Table of Contents

1. [How the Application Works](#how-the-application-works)
   - [High-Level Architecture](#high-level-architecture)
   - [Automatic Microsoft Teams Detection (UI Automation)](#automatic-microsoft-teams-detection-ui-automation)
   - [Dual-Channel Stereo Audio Capture (WASAPI)](#dual-channel-stereo-audio-capture-wasapi)
   - [Mute vs. Pause Behavior](#mute-vs-pause-behavior)
   - [System Tray and Floating Pill Controls](#system-tray-and-floating-pill-controls)
2. [What the Application Stores](#what-the-application-stores)
   - [Directory and File Layout](#directory-and-file-layout)
   - [Audio File (`recording.wav`)](#audio-file-recordingwav)
   - [Meeting Metadata (`meeting-details.json`)](#meeting-metadata-meeting-detailsjson)
3. [Project Structure](#project-structure)
4. [Running Locally](#running-locally)
   - [Prerequisites](#prerequisites)
   - [Setup and Development Mode](#setup-and-development-mode)
   - [Enabling Debug Logs](#enabling-debug-logs)
5. [Building for Production](#building-for-production)
6. [Testing and Verification](#testing-and-verification)
7. [Troubleshooting](#troubleshooting)

---

## How the Application Works

### High-Level Architecture

The application separates low-level Windows API calls from application logic using dedicated OS threads and an asynchronous Tokio coordinator:

```text
+-----------------------------------+       TeamsSnapshot (mpsc)
| Windows UI Automation Thread      | ---------------------------------+
| (COINIT_MULTITHREADED, 1.5s poll) |                                  |
+-----------------------------------+                                  v
                                                       +-------------------------------+
+-----------------------------------+   ringbuf SPSC   | Async Tokio Coordinator       |
| WASAPI Mic Thread ("Pro Audio")   | ------------+    | - Recording state machine     |
| eCapture -> Downmix -> 48kHz      |             |    | - Participant & mute tracking |
+-----------------------------------+             v    | - Atomic JSON persistence     |
                                        +--------------------+ - Pill & tray updates   |
+-----------------------------------+   | Stereo WAV Writer  | <-----------------------+
| WASAPI Loopback Thread ("ProAudio")|-->| 10ms clock-synced  |        |        ^
| eRender Loopback -> Downmix -> 48k|   | -> recording.wav   |        |        |
+-----------------------------------+   +--------------------+        v        |
                                                       +-------------------------------+
                                                       | Tauri v2 System Tray & Pill   |
                                                       | (WebView2, Vanilla TS + CSS)  |
                                                       +-------------------------------+
```

All `unsafe` Win32, COM, UI Automation, and WASAPI calls are isolated inside `src-tauri/src/teams/` and `src-tauri/src/audio/`. Every OS resource is wrapped in an RAII guard implementing `Drop` (`ComGuard`, `MmcssGuard`, `AudioClientGuard`, `CoTaskMemGuard`, and `WavWriterGuard`) so audio files are finalized cleanly even if a call ends abruptly or the application exits.

---

### Automatic Microsoft Teams Detection (UI Automation)

A dedicated background OS thread (`teams-uia-watcher`) initializes COM (`COINIT_MULTITHREADED`) and polls visible Microsoft Teams windows every **1.5 seconds**:

1. **Process Verification:** Enumerates all visible top-level Windows HWNDs (`EnumWindows`) and verifies the owning executable via `OpenProcess` + `QueryFullProcessImageNameW` matches `ms-teams.exe`. Because New Teams opens a separate top-level window for active calls while keeping the main chat window open, all `ms-teams.exe` windows are checked.
2. **Active Call Detection:** Searches each `ms-teams.exe` window's UI Automation tree for an element with `AutomationId == "hangup-button"` or a `Button` control named `"Leave"`.
   - **Call Start (`false -> true`):** When Auto-Record is enabled, recording starts immediately and the floating pill appears at the top-center of the primary monitor.
   - **Call End (`true -> false`):** Debounced across **2 consecutive missed polls (~3 seconds)** so brief window redraws or pop-out transitions do not split a meeting into multiple recordings. Once confirmed ended, the WAV header is finalized, `meeting-details.json` is flushed, and the pill hides.
   - **Manual Stop Suppression:** If you manually click Stop on the pill while still in an auto-recorded Teams call, the app stops that session and will not auto-restart until the current call finishes and a new call begins.
3. **Mic Mute Detection:** Queries `AutomationId == "microphone-button"`:
   - Accessible name `"Unmute mic"` means the microphone is currently **muted**.
   - Accessible name `"Mute mic"` means the microphone is currently **unmuted**.
4. **Meeting Title Extraction:** Reads the active Teams call window title (`GetWindowTextW`) and strips the trailing `" | Microsoft Teams"`. Falls back to `"Untitled Teams Meeting"` if blank or generic.
5. **Participant Extraction:** Uses a batched `IUIAutomationCacheRequest` for `UIA_NamePropertyId` to minimize cross-process COM overhead and parses visible stage/roster tiles:
   - Remote participant tiles ending in `", muted, Context menu is available"` or `", unmuted, Context menu is available"` have the suffix stripped to extract the display name (for example, `"Shruthi (Guest)"`).
   - The self tile starting with `"Myself video, "` extracts the second comma-separated token (for example, `"Jayesh Krishna"`) and marks `is_self: true`.

---

### Dual-Channel Stereo Audio Capture (WASAPI)

Audio recording runs on three dedicated OS threads to prevent audio glitches and keep memory usage minimal during multi-hour meetings:

1. **Microphone Capture Thread (`wasapi-mic-capture`):**
   - Captures the default Windows microphone (`eCapture`), preferring the `eCommunications` role (used by Teams headsets when configured) and falling back to `eConsole`.
   - Registers with Windows MMCSS `"Pro Audio"` thread priority (`AvSetMmThreadCharacteristicsW`).
2. **Playback Endpoint Loopback Thread (`wasapi-loopback-capture`):**
   - Captures the default Windows playback device (`eRender`) in shared mode with `AUDCLNT_STREAMFLAGS_LOOPBACK`, preferring `eCommunications` with fallback to `eConsole`.
   - **Why Endpoint Loopback (not Per-Process Loopback):** New Teams routes call audio under `AudioCategory_Communications`, which outputs silence under Windows per-process WASAPI loopback (`ActivateAudioInterfaceAsync`). Using endpoint loopback gated by active call detection reliably captures remote participants.
3. **Downmixing and Resampling:**
   - Supports Windows mix formats in 32-bit IEEE Float, 16-bit PCM, 24-bit PCM, and 32-bit PCM with any channel count, downmixing each stream to mono `f32`.
   - Streams already at `48000 Hz` pass through with zero overhead; other sample rates (such as `44100 Hz`, `16000 Hz` Bluetooth HFP, or `96000 Hz`) are resampled to `48000 Hz` using `rubato::FftFixedIn` (with a linear fallback).
4. **Lock-Free Ring Buffer and Clock-Aligned Stereo Writer (`stereo-wav-writer`):**
   - Both capture threads push 48 kHz mono `f32` samples into separate lock-free `ringbuf` SPSC queues (2 seconds capacity per channel).
   - Because Windows WASAPI loopback emits zero packets during playback silence and microphone/playback hardware clocks can drift slightly over long meetings, the writer thread paces output on a monotonic wall-clock (`Instant`) schedule:
     - If either channel temporarily underruns or is silent, the writer pads `0.0` samples so Left and Right remain time-aligned.
     - If hardware clock skew builds up more than 200 ms of backlog in a queue, excess oldest samples are trimmed to bound latency.
     - Interleaved `[Left, Right]` 16-bit PCM frames stream directly to `recording.wav` via `hound::WavWriter` and flush every ~2 seconds.
   - **Device Change Recovery:** If you switch headsets or audio devices mid-call (`AUDCLNT_E_DEVICE_INVALIDATED`), the affected capture thread logs a warning and transparently re-opens the new default endpoint without stopping the recording.

---

### Mute vs. Pause Behavior

The floating pill provides two distinct audio controls:

- **Microphone Mute Button (or Muting Inside Teams):**
  - Mutes **only Channel 0 (Left / Local Microphone)** by writing `0` samples to the left channel.
  - **Channel 1 (Right / Remote Participants) continues recording normally**, so other people speaking in the meeting are still captured while you are muted.
  - Effective mute is active whenever either Teams is muted or the pill's local mute toggle is enabled (`teams_muted || manual_muted`). Every change is logged in `mute_events`.
- **Pause Button:**
  - Pauses the entire recording session. No frames are written to `recording.wav` while paused, the ring buffers are drained so resuming has no stale audio burst, and the recording duration timer pauses until you resume.

---

### System Tray and Floating Pill Controls

- **System Tray Icon:** The app starts silently in the Windows notification area (system tray) with no taskbar button. Right-clicking the icon opens the menu:
  - **Open Recordings Folder:** Opens `%USERPROFILE%\Documents\TeamsRecordings` in Windows Explorer.
  - **Start Manual Recording / Stop Recording:** Manually starts a recording (even outside of Teams) or stops the active session.
  - **Auto-Record: Enabled / Disabled:** Toggles automatic recording when a Microsoft Teams call is detected.
  - **Quit:** Stops the UIA watcher, finalizes any active WAV file and JSON metadata, and exits cleanly.
  - Left-clicking the tray icon while a recording is active hides or restores the floating pill.
- **Floating Pill Overlay:** A frameless, transparent, always-on-top widget (`368 x 58 px`) that appears automatically when recording begins. It can be dragged anywhere on screen and displays:
  - Pulsing red recording dot (turns steady amber when paused)
  - Elapsed recording timer (`MM:SS`, expanding to `HH:MM:SS` after 1 hour)
  - Participant count badge (hover to see the meeting title and full list of detected participant names)
  - Local Mic Mute toggle button
  - Pause / Resume button
  - Stop & Save button

---

## What the Application Stores

### Directory and File Layout

Every recording session is stored locally under:

```text
%USERPROFILE%\Documents\TeamsRecordings\<YYYY-MM-DD_HH-MM-SS>_<sanitized-meeting-title>\
├── recording.wav
└── meeting-details.json
```

Example path:

```text
C:\Users\YourName\Documents\TeamsRecordings\2026-11-18_11-38-39_Meeting-with-Jayesh-Krishna\
├── recording.wav
└── meeting-details.json
```

**Folder naming rules:**
- `<YYYY-MM-DD_HH-MM-SS>` is the local start timestamp of the recording.
- `<sanitized-meeting-title>` strips Windows-forbidden filename characters (`< > : " / \ | ? *` and control characters), collapses whitespace to hyphens, guards against Windows reserved device names (`CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`), and caps length at 64 characters. If two recordings start within the same second with the same title, a numeric suffix (`_2`, `_3`, ...) is appended.

---

### Audio File (`recording.wav`)

| Property | Value |
| :--- | :--- |
| Format | Uncompressed PCM WAV (RIFF) |
| Sample Rate | `48000 Hz` |
| Bit Depth | `16-bit signed integer` |
| Channels | `2` (Stereo) |
| Channel 0 (Left) | Local microphone (`local_microphone`) |
| Channel 1 (Right) | Remote playback endpoint loopback (`remote_participants`) |

Keeping your voice on the Left channel and remote participants on the Right channel makes downstream transcription, speaker separation (diarization), and post-processing straightforward because local and remote audio are never mixed together.

---

### Meeting Metadata (`meeting-details.json`)

The metadata file is written atomically (written to `meeting-details.json.tmp`, synced to disk, and renamed) at recording start, whenever participants join or mute state changes, and finalized when recording stops.

#### Example File

```json
{
  "session_id": "697abd6d-c6fd-43a8-9ce4-40fab2748474",
  "meeting_title": "Meeting with Jayesh Krishna",
  "started_at": "2026-11-18T11:38:39.322501500+05:30",
  "ended_at": "2026-11-18T11:41:25.849553600+05:30",
  "duration_seconds": 166,
  "audio_file": "recording.wav",
  "channels": {
    "left": "local_microphone",
    "right": "remote_participants"
  },
  "participants": [
    {
      "name": "Jayesh Krishna",
      "is_self": true,
      "first_seen": "2026-11-18T11:39:30.242880500+05:30",
      "last_seen": "2026-11-18T11:41:22.186532700+05:30"
    },
    {
      "name": "Shruthi (Guest)",
      "is_self": false,
      "first_seen": "2026-11-18T11:39:35.609375600+05:30",
      "last_seen": "2026-11-18T11:40:19.907273+05:30"
    }
  ],
  "mute_events": [
    {
      "timestamp": "2026-11-18T11:40:07.976869700+05:30",
      "muted": true
    }
  ]
}
```

#### Field-by-Field Reference

| Field | Type | Meaning |
| :--- | :--- | :--- |
| `session_id` | `string` (UUID v4) | Unique identifier generated for this recording session. |
| `meeting_title` | `string` | Meeting title extracted from the Teams call window (with `" \| Microsoft Teams"` stripped), or `"Manual Recording"` / `"Untitled Teams Meeting"` if unavailable. |
| `started_at` | `string` (ISO 8601) | Local date and time (with timezone offset) when recording started. |
| `ended_at` | `string \| null` (ISO 8601) | Local date and time when recording stopped and finalized (`null` while a session is still actively recording). |
| `duration_seconds` | `integer` | Total active recorded audio duration in seconds (excludes any paused intervals). |
| `audio_file` | `string` | Relative filename of the companion audio file (`"recording.wav"`). |
| `channels.left` | `string` | Describes Channel 0 (`"local_microphone"`). |
| `channels.right` | `string` | Describes Channel 1 (`"remote_participants"`). |
| `participants` | `array` | Deduplicated list of all participants observed in the Teams UI during the meeting, sorted with `is_self: true` first, then by `first_seen`. |
| `participants[].name` | `string` | Display name parsed from the Teams participant tile. |
| `participants[].is_self` | `boolean` | `true` if the entry corresponds to the local user (`"Myself video, ..."` tile), `false` for remote participants. |
| `participants[].first_seen` | `string` (ISO 8601) | Exact timestamp when the participant was **first detected** on the Teams call stage/roster (indicates when they joined or first appeared). |
| `participants[].last_seen` | `string` (ISO 8601) | Most recent timestamp when the participant was **still visible** on the Teams call UI (updated on each 1.5s poll). Comparing `first_seen` and `last_seen` against `started_at` and `ended_at` shows who joined late, left early, and how long they were present. |
| `mute_events` | `array` | Chronological log of effective local microphone mute state transitions during the session. |
| `mute_events[].timestamp` | `string` (ISO 8601) | Local timestamp when the mute transition occurred. |
| `mute_events[].muted` | `boolean` | `true` when Channel 0 (Left) switched to muted (writing silence), `false` when unmuted. |

---

## Project Structure

```text
desktop-team-recorder/
├── src-tauri/
│   ├── Cargo.toml                   # Rust dependencies and crate metadata
│   ├── build.rs                     # Tauri build script
│   ├── tauri.conf.json              # Tauri v2 window, bundle, and build config
│   ├── capabilities/
│   │   └── default.json             # Window permissions for the floating pill
│   ├── icons/                       # App and tray icons (PNG and ICO)
│   └── src/
│       ├── main.rs                  # Windows binary entry point
│       ├── lib.rs                   # Tauri setup, system tray, IPC commands
│       ├── error.rs                 # Unified AppError enum (thiserror)
│       ├── models.rs                # Serde models for sessions, state, and UIA snapshots
│       ├── coordinator.rs           # Async state machine linking UIA, audio, storage, and UI
│       ├── audio/
│       │   ├── mod.rs               # Safe AudioRecorder RAII handle
│       │   ├── wasapi_capture.rs    # Isolated unsafe WASAPI Mic & Endpoint Loopback capture
│       │   ├── resampler.rs         # Multi-channel downmixing and 48 kHz resampling (rubato)
│       │   └── stereo_writer.rs     # Clock-synced ringbuf consumer and WAV writer
│       ├── teams/
│       │   ├── mod.rs               # Public Teams watcher exports
│       │   ├── process.rs           # Win32 HWND enumeration and ms-teams.exe verification
│       │   └── uia_watcher.rs       # Windows UI Automation poller, debouncer, and tile parser
│       └── storage/
│           ├── mod.rs               # Public storage exports
│           └── session_fs.rs        # Session directories, filename sanitization, atomic JSON
├── src/
│   ├── index.html                   # Floating pill markup
│   ├── styles.css                   # Dark glassmorphic pill styles
│   └── main.ts                      # Tauri event listener and command bindings
├── package.json                     # Frontend scripts and Tauri CLI/API packages
├── tsconfig.json                    # TypeScript compiler configuration
├── vite.config.ts                   # Vite bundler configuration
└── README.md
```

---

## Running Locally

### Prerequisites

1. **Operating System:** Windows 10 or Windows 11 (x64).
2. **Microsoft Edge WebView2 Runtime:** Pre-installed on Windows 11 and most Windows 10 systems (required by Tauri v2).
3. **Visual Studio C++ Build Tools:** Required by Rust MSVC targets (install "Desktop development with C++" via Visual Studio Installer).
4. **Rust Toolchain:** `stable-x86_64-pc-windows-msvc` (install via [rustup](https://rustup.rs/)).
5. **Node.js:** Version 20 or newer (with `npm`).

Verify your local toolchain before building:

```powershell
rustc --version
cargo --version
node --version
npm --version
```

### Setup and Development Mode

1. Install frontend dependencies (including `@tauri-apps/cli` and `@tauri-apps/api`):

   ```powershell
   npm install
   ```

2. Start the desktop app in development mode (launches Vite dev server on `http://localhost:1420` and compiles/runs the Rust Tauri app):

   ```powershell
   npm run tauri dev
   ```

3. Once started, the app runs in the **Windows system tray** (look in the taskbar notification area or the hidden icons chevron). Right-click the tray icon to start a manual recording, open the recordings folder, toggle auto-recording, or quit.

### Enabling Debug Logs

To view verbose logs (including Teams UIA poll transitions, WASAPI mix formats, and audio device recovery events), set `RUST_LOG` before launching:

```powershell
$env:RUST_LOG="teams_local_recorder_lib=debug,info"
npm run tauri dev
```

---

## Building for Production

To compile an optimized release binary and generate Windows installers (`.msi` and `.exe` NSIS setup):

```powershell
npm run tauri build
```

This runs `npm run build` (`tsc --noEmit && vite build`) to bundle the frontend into `dist/`, then compiles the Rust backend in `--release` mode and packages the outputs:

- **Standalone Release Executable:**
  `src-tauri\target\release\teams-local-recorder.exe`
- **Windows MSI Installer:**
  `src-tauri\target\release\bundle\msi\`
- **Windows NSIS Setup Installer:**
  `src-tauri\target\release\bundle\nsis\`

If you only want to compile the Rust executable without creating installers:

```powershell
npm run build
cargo build --release --manifest-path src-tauri/Cargo.toml
```

---

## Testing and Verification

Run all automated frontend and Rust checks:

```powershell
# TypeScript static typecheck
npm run typecheck

# Frontend production bundle
npm run build

# Fast Rust compilation check
cargo check --manifest-path src-tauri/Cargo.toml

# Run Rust unit tests (storage, Teams parsing, resampling, stereo WAV interleaving & mute)
cargo test --manifest-path src-tauri/Cargo.toml
```

---

## Troubleshooting

- **App launches, but no window appears:** This is expected. The app lives in the Windows system tray while idle and automatically displays the floating pill only when a recording starts (either automatically on a Teams call or via **Start Manual Recording** in the tray menu).
- **Teams call is active, but recording does not auto-start:**
  - Verify **Auto-Record: Enabled** is checked in the system tray menu.
  - Ensure you are using the **New Microsoft Teams Desktop app** (`ms-teams.exe`), not Teams in a web browser.
  - Note that if you clicked Stop manually during the current call, auto-recording stays suppressed until that call ends.
- **No remote audio or wrong audio device captured:**
  - Teams Local Recorder captures the Windows default **Communications** device (`eCommunications`), falling back to the default **Console** device (`eConsole`). Confirm that your active headset or speakers are set as the Default Communications Device in Windows Sound Settings (`mmsys.cpl`), and that Teams is using the same audio devices.
- **Participants list is missing someone:**
  - Windows UI Automation can only inspect participant tiles currently rendered by Teams on the stage or open People roster. Minimizing Teams completely or hiding tiles off-screen can prevent UIA from seeing those tiles until they appear on screen.
