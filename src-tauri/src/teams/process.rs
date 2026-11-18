use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};

const TEAMS_EXE_NAME: &str = "ms-teams.exe";
const DEFAULT_MEETING_TITLE: &str = "Untitled Teams Meeting";

/// Top-level visible window verified to belong to `ms-teams.exe`.
#[derive(Debug, Clone)]
pub struct TeamsWindow {
    pub hwnd: isize,
    pub pid: u32,
    pub raw_title: String,
    pub meeting_title: String,
}

/// RAII wrapper ensuring Win32 process handles are always closed.
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: Handle was returned by OpenProcess and is owned exclusively by HandleGuard.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Extracts a clean meeting title from a `ms-teams.exe` top-level window title.
pub fn extract_meeting_title(raw_title: &str) -> String {
    let trimmed = raw_title.trim();
    let stripped = trimmed
        .strip_suffix(" | Microsoft Teams")
        .or_else(|| trimmed.strip_suffix("| Microsoft Teams"))
        .unwrap_or(trimmed)
        .trim();

    if stripped.is_empty() || stripped.eq_ignore_ascii_case("Microsoft Teams") {
        DEFAULT_MEETING_TITLE.to_string()
    } else {
        stripped.to_string()
    }
}

/// Enumerates all visible top-level windows owned by `ms-teams.exe`.
pub fn find_teams_windows() -> Vec<TeamsWindow> {
    let mut windows_found: Vec<TeamsWindow> = Vec::new();
    let lparam = LPARAM(&mut windows_found as *mut Vec<TeamsWindow> as isize);

    // SAFETY: `enum_windows_proc` only reads the pointer during the synchronous `EnumWindows` call.
    unsafe {
        let _ = EnumWindows(Some(enum_windows_proc), lparam);
    }

    windows_found
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }

    let mut pid: u32 = 0;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return BOOL(1);
    }

    if !is_teams_process(pid) {
        return BOOL(1);
    }

    let raw_title = read_window_title(hwnd);
    let meeting_title = extract_meeting_title(&raw_title);

    let out_ptr = lparam.0 as *mut Vec<TeamsWindow>;
    if let Some(list) = out_ptr.as_mut() {
        list.push(TeamsWindow {
            hwnd: hwnd.0 as isize,
            pid,
            raw_title,
            meeting_title,
        });
    }

    BOOL(1)
}

fn is_teams_process(pid: u32) -> bool {
    // SAFETY: `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` is read-only and guarded by `HandleGuard`.
    let handle = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) if !h.is_invalid() => HandleGuard(h),
        _ => return false,
    };

    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;

    // SAFETY: `buf` is valid for `len` u16 elements and `handle.0` is an open process handle.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    };

    if ok.is_err() || len == 0 {
        return false;
    }

    let os_path = OsString::from_wide(&buf[..len as usize]);
    Path::new(&os_path)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.eq_ignore_ascii_case(TEAMS_EXE_NAME))
        .unwrap_or(false)
}

fn read_window_title(hwnd: HWND) -> String {
    // SAFETY: `GetWindowTextLengthW` and `GetWindowTextW` safely accept any top-level HWND.
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (len as usize) + 1];
        let copied = GetWindowTextW(hwnd, &mut buf);
        if copied <= 0 {
            return String::new();
        }
        String::from_utf16_lossy(&buf[..copied as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_meeting_title_from_teams_window() {
        assert_eq!(
            extract_meeting_title("Daily Standup | Microsoft Teams"),
            "Daily Standup"
        );
        assert_eq!(
            extract_meeting_title("Architecture Review (Q4) | Microsoft Teams"),
            "Architecture Review (Q4)"
        );
        assert_eq!(
            extract_meeting_title("Microsoft Teams"),
            "Untitled Teams Meeting"
        );
        assert_eq!(
            extract_meeting_title("   | Microsoft Teams   "),
            "Untitled Teams Meeting"
        );
        assert_eq!(extract_meeting_title(""), "Untitled Teams Meeting");
    }
}
