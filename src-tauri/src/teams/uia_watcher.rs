use std::collections::HashSet;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};
use windows::core::VARIANT;
use windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationCacheRequest, IUIAutomationCondition,
    IUIAutomationElement, TreeScope_Descendants, UIA_AutomationIdPropertyId,
    UIA_ButtonControlTypeId, UIA_ControlTypePropertyId, UIA_NamePropertyId,
};

use crate::error::{AppError, AppResult};
use crate::models::{DetectedParticipant, TeamsSnapshot};
use crate::teams::process::find_teams_windows;

const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const END_CALL_DEBOUNCE_POLLS: u8 = 2;

const REMOTE_MUTED_SUFFIX: &str = ", muted, Context menu is available";
const REMOTE_UNMUTED_SUFFIX: &str = ", unmuted, Context menu is available";
const SELF_VIDEO_PREFIX: &str = "Myself video, ";

/// RAII guard for COM initialization on the UIA worker thread.
struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init_multithreaded() -> AppResult<Self> {
        // SAFETY: Initializing COM MTA once on this dedicated worker thread.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            Ok(Self {
                should_uninit: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            // Thread already initialized with a different apartment model; do not uninitialize it.
            Ok(Self {
                should_uninit: false,
            })
        } else {
            Err(AppError::Uia(format!(
                "CoInitializeEx(COINIT_MULTITHREADED) failed: {hr:?}"
            )))
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: Paired with a successful `CoInitializeEx` on the same thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Handle to the background Teams UI Automation watcher thread.
/// Automatically signals shutdown and joins the thread on drop.
pub struct TeamsWatcherHandle {
    stop_tx: Sender<()>,
    join_handle: Option<JoinHandle<()>>,
}

impl TeamsWatcherHandle {
    pub fn stop(&mut self) {
        let _ = self.stop_tx.try_send(());
        if let Some(handle) = self.join_handle.take() {
            if let Err(err) = handle.join() {
                warn!("Teams UIA watcher thread panicked during join: {err:?}");
            }
        }
    }
}

impl Drop for TeamsWatcherHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Spawns the dedicated background OS thread that polls `ms-teams.exe` via Windows UI Automation.
pub fn spawn_teams_watcher(tx: UnboundedSender<TeamsSnapshot>) -> TeamsWatcherHandle {
    let (stop_tx, stop_rx) = bounded::<()>(1);

    let join_handle = thread::Builder::new()
        .name("teams-uia-watcher".to_string())
        .spawn(move || {
            info!("Teams UIA watcher thread started");
            if let Err(err) = run_watcher_loop(&tx, &stop_rx) {
                warn!("Teams UIA watcher exited with error: {err}");
            }
            info!("Teams UIA watcher thread stopped");
        })
        .ok();

    TeamsWatcherHandle {
        stop_tx,
        join_handle,
    }
}

struct UiaQueryContext {
    automation: IUIAutomation,
    in_call_cond: IUIAutomationCondition,
    mic_button_cond: IUIAutomationCondition,
    true_cond: IUIAutomationCondition,
    name_cache_request: IUIAutomationCacheRequest,
}

impl UiaQueryContext {
    fn new() -> AppResult<Self> {
        // SAFETY: COM is initialized on the current thread via `ComGuard`.
        unsafe {
            let automation: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;

            let hangup_cond = automation
                .CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from("hangup-button"))?;

            let button_type_cond = automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_ButtonControlTypeId.0 as i32),
            )?;
            let leave_name_cond =
                automation.CreatePropertyCondition(UIA_NamePropertyId, &VARIANT::from("Leave"))?;
            let leave_button_cond =
                automation.CreateAndCondition(&button_type_cond, &leave_name_cond)?;

            let in_call_cond = automation.CreateOrCondition(&hangup_cond, &leave_button_cond)?;

            let mic_button_cond = automation.CreatePropertyCondition(
                UIA_AutomationIdPropertyId,
                &VARIANT::from("microphone-button"),
            )?;

            let true_cond = automation.CreateTrueCondition()?;
            let name_cache_request = automation.CreateCacheRequest()?;
            name_cache_request.AddProperty(UIA_NamePropertyId)?;

            Ok(Self {
                automation,
                in_call_cond,
                mic_button_cond,
                true_cond,
                name_cache_request,
            })
        }
    }
}

fn run_watcher_loop(
    tx: &UnboundedSender<TeamsSnapshot>,
    stop_rx: &crossbeam_channel::Receiver<()>,
) -> AppResult<()> {
    let _com = ComGuard::init_multithreaded()?;
    let mut ctx: Option<UiaQueryContext> = None;

    let mut confirmed_in_call = false;
    let mut missed_call_polls: u8 = 0;
    let mut last_known_mic_muted = false;
    let mut last_known_title: Option<String> = None;

    loop {
        if ctx.is_none() {
            match UiaQueryContext::new() {
                Ok(created) => ctx = Some(created),
                Err(err) => {
                    warn!("Failed to initialize IUIAutomation context, will retry: {err}");
                }
            }
        }

        if let Some(uia) = ctx.as_ref() {
            match poll_teams_once(uia, last_known_mic_muted) {
                Ok(raw_snapshot) => {
                    if raw_snapshot.is_in_call {
                        confirmed_in_call = true;
                        missed_call_polls = 0;
                        last_known_mic_muted = raw_snapshot.is_mic_muted;
                        if raw_snapshot.meeting_title.is_some() {
                            last_known_title = raw_snapshot.meeting_title.clone();
                        }

                        if tx.send(raw_snapshot).is_err() {
                            break;
                        }
                    } else if confirmed_in_call {
                        missed_call_polls = missed_call_polls.saturating_add(1);
                        debug!(
                            missed_call_polls,
                            threshold = END_CALL_DEBOUNCE_POLLS,
                            "Teams call element momentarily missing; debouncing"
                        );
                        if missed_call_polls >= END_CALL_DEBOUNCE_POLLS {
                            confirmed_in_call = false;
                            missed_call_polls = 0;
                            last_known_mic_muted = false;
                            last_known_title = None;
                            if tx.send(TeamsSnapshot::default()).is_err() {
                                break;
                            }
                        } else {
                            // Keep emitting active state with last known metadata while debouncing.
                            let held = TeamsSnapshot {
                                is_in_call: true,
                                is_mic_muted: last_known_mic_muted,
                                meeting_title: last_known_title.clone(),
                                participants: Vec::new(),
                            };
                            if tx.send(held).is_err() {
                                break;
                            }
                        }
                    } else if tx.send(TeamsSnapshot::default()).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    warn!("UIA poll error (recovering gracefully): {err}");
                    // Recreate UIA context on next tick in case COM proxy became stale.
                    ctx = None;
                }
            }
        }

        match stop_rx.recv_timeout(POLL_INTERVAL) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => continue,
        }
    }

    Ok(())
}

fn poll_teams_once(uia: &UiaQueryContext, previous_mic_muted: bool) -> AppResult<TeamsSnapshot> {
    let teams_windows = find_teams_windows();
    if teams_windows.is_empty() {
        return Ok(TeamsSnapshot::default());
    }

    for win in teams_windows {
        let hwnd = HWND(win.hwnd as *mut core::ffi::c_void);
        // SAFETY: `uia.automation` lives on this COM-initialized thread and `hwnd` is checked gracefully.
        let root = match unsafe { uia.automation.ElementFromHandle(hwnd) } {
            Ok(el) => el,
            Err(_) => continue,
        };

        let in_call_el = unsafe { root.FindFirst(TreeScope_Descendants, &uia.in_call_cond) };
        let is_in_call = matches!(in_call_el, Ok(_));

        if !is_in_call {
            continue;
        }

        let is_mic_muted = read_mic_mute_state(&root, &uia.mic_button_cond)
            .unwrap_or(previous_mic_muted);

        let participants = read_participants(&root, &uia.true_cond, &uia.name_cache_request);

        return Ok(TeamsSnapshot {
            is_in_call: true,
            is_mic_muted,
            meeting_title: Some(win.meeting_title),
            participants,
        });
    }

    Ok(TeamsSnapshot::default())
}

fn read_mic_mute_state(
    root: &IUIAutomationElement,
    mic_button_cond: &IUIAutomationCondition,
) -> Option<bool> {
    // SAFETY: Querying descendant element and reading its `CurrentName` BSTR on the UIA thread.
    unsafe {
        let mic_btn = root.FindFirst(TreeScope_Descendants, mic_button_cond).ok()?;
        let name = mic_btn.CurrentName().ok()?.to_string();
        parse_mic_button_muted(&name)
    }
}

fn read_participants(
    root: &IUIAutomationElement,
    true_cond: &IUIAutomationCondition,
    cache_request: &IUIAutomationCacheRequest,
) -> Vec<DetectedParticipant> {
    let mut seen = HashSet::new();
    let mut participants = Vec::new();

    // SAFETY: `FindAllBuildCache` fetches all descendant names in a single batched COM call.
    let elements = match unsafe {
        root.FindAllBuildCache(TreeScope_Descendants, true_cond, cache_request)
    } {
        Ok(arr) => arr,
        Err(err) => {
            debug!("Failed to query Teams participant elements: {err}");
            return participants;
        }
    };

    let len = unsafe { elements.Length().unwrap_or(0) };
    for idx in 0..len {
        let name = unsafe {
            let Ok(elem) = elements.GetElement(idx) else {
                continue;
            };
            let Ok(bstr) = elem.CachedName() else {
                continue;
            };
            bstr.to_string()
        };

        if let Some(participant) = parse_participant_tile(&name) {
            if seen.insert(participant.name.clone()) {
                participants.push(participant);
            }
        }
    }

    participants
}

/// Parses the Microsoft Teams microphone button accessible name into a mute boolean.
/// - `"Unmute mic"` -> `Some(true)` (mic is currently muted)
/// - `"Mute mic"` -> `Some(false)` (mic is currently unmuted)
pub fn parse_mic_button_muted(button_name: &str) -> Option<bool> {
    let normalized = button_name.trim().to_ascii_lowercase();
    if normalized == "unmute mic" || normalized.starts_with("unmute") {
        Some(true)
    } else if normalized == "mute mic" || normalized.starts_with("mute") {
        Some(false)
    } else {
        None
    }
}

/// Parses a Microsoft Teams participant stage/roster tile accessible name.
/// - Remote tiles end with `", muted, Context menu is available"` or
///   `", unmuted, Context menu is available"`.
/// - Self tile starts with `"Myself video, "` where the second comma-separated token is the user's name.
pub fn parse_participant_tile(element_name: &str) -> Option<DetectedParticipant> {
    let trimmed = element_name.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(rest) = trimmed.strip_prefix(SELF_VIDEO_PREFIX) {
        let name = rest.split(',').next()?.trim();
        if !name.is_empty() {
            return Some(DetectedParticipant {
                name: name.to_string(),
                is_self: true,
            });
        }
        return None;
    }

    if let Some(name) = trimmed
        .strip_suffix(REMOTE_MUTED_SUFFIX)
        .or_else(|| trimmed.strip_suffix(REMOTE_UNMUTED_SUFFIX))
    {
        let clean = name.trim();
        if !clean.is_empty() {
            return Some(DetectedParticipant {
                name: clean.to_string(),
                is_self: false,
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mic_button_names() {
        assert_eq!(parse_mic_button_muted("Unmute mic"), Some(true));
        assert_eq!(parse_mic_button_muted("Mute mic"), Some(false));
        assert_eq!(parse_mic_button_muted("Unmute"), Some(true));
        assert_eq!(parse_mic_button_muted("Mute"), Some(false));
        assert_eq!(parse_mic_button_muted("More options"), None);
    }

    #[test]
    fn parses_remote_and_self_participant_tiles() {
        assert_eq!(
            parse_participant_tile("Shruthi (Guest), muted, Context menu is available"),
            Some(DetectedParticipant {
                name: "Shruthi (Guest)".to_string(),
                is_self: false,
            })
        );
        assert_eq!(
            parse_participant_tile("Alex Rivera, unmuted, Context menu is available"),
            Some(DetectedParticipant {
                name: "Alex Rivera".to_string(),
                is_self: false,
            })
        );
        assert_eq!(
            parse_participant_tile("Myself video, Jayesh Krishna, muted"),
            Some(DetectedParticipant {
                name: "Jayesh Krishna".to_string(),
                is_self: true,
            })
        );
        assert_eq!(
            parse_participant_tile("Myself video, Jayesh Krishna"),
            Some(DetectedParticipant {
                name: "Jayesh Krishna".to_string(),
                is_self: true,
            })
        );
        assert_eq!(parse_participant_tile("Leave"), None);
        assert_eq!(parse_participant_tile(""), None);
    }
}
