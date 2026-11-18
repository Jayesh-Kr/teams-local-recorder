use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local};

use crate::error::{AppError, AppResult};
use crate::models::MeetingSession;

const DEFAULT_FALLBACK_TITLE: &str = "Untitled-Teams-Meeting";
const MAX_SANITIZED_LEN: usize = 64;

/// Filesystem paths belonging to a single recorded meeting session.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub session_dir: PathBuf,
    pub audio_path: PathBuf,
    pub metadata_path: PathBuf,
}

/// Resolves `%USERPROFILE%\Documents\TeamsRecordings`.
pub fn default_recordings_root() -> AppResult<PathBuf> {
    let user_profile = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .ok_or_else(|| {
            AppError::Storage(
                "Unable to resolve USERPROFILE environment variable for recordings directory"
                    .to_string(),
            )
        })?;

    Ok(PathBuf::from(user_profile)
        .join("Documents")
        .join("TeamsRecordings"))
}

/// Ensures the root recordings folder exists.
pub fn ensure_recordings_root(root: &Path) -> AppResult<()> {
    fs::create_dir_all(root)?;
    Ok(())
}

/// Sanitizes a Teams meeting title so it is safe as part of a Windows directory name.
pub fn sanitize_meeting_title(title: &str) -> String {
    let mut cleaned = String::with_capacity(title.len().min(MAX_SANITIZED_LEN));
    let mut prev_sep = true;

    for ch in title.trim().chars() {
        let is_forbidden = ch.is_control()
            || matches!(
                ch,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            );

        if is_forbidden || ch.is_whitespace() {
            if !prev_sep && !cleaned.is_empty() {
                cleaned.push('-');
                prev_sep = true;
            }
        } else if ch == '-' || ch == '_' {
            if !prev_sep && !cleaned.is_empty() {
                cleaned.push(ch);
                prev_sep = true;
            }
        } else {
            cleaned.push(ch);
            prev_sep = false;
        }

        if cleaned.chars().count() >= MAX_SANITIZED_LEN {
            break;
        }
    }

    let trimmed = cleaned
        .trim_matches(|c: char| c == '.' || c == '-' || c == '_' || c.is_whitespace())
        .to_string();

    if trimmed.is_empty() {
        return DEFAULT_FALLBACK_TITLE.to_string();
    }

    if is_reserved_windows_name(&trimmed) {
        return format!("meeting-{trimmed}");
    }

    trimmed
}

fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

/// Creates `<root>/<YYYY-MM-DD_HH-MM-SS>_<sanitized-meeting-title>/` and returns session file paths.
pub fn create_session_dir(
    root: &Path,
    started_at: DateTime<Local>,
    meeting_title: &str,
) -> AppResult<SessionPaths> {
    ensure_recordings_root(root)?;

    let timestamp = started_at.format("%Y-%m-%d_%H-%M-%S").to_string();
    let sanitized = sanitize_meeting_title(meeting_title);
    let base_name = format!("{timestamp}_{sanitized}");

    let mut candidate = root.join(&base_name);
    let mut suffix = 2u32;
    while candidate.exists() {
        candidate = root.join(format!("{base_name}_{suffix}"));
        suffix = suffix.saturating_add(1);
    }

    fs::create_dir_all(&candidate)?;

    Ok(SessionPaths {
        audio_path: candidate.join("recording.wav"),
        metadata_path: candidate.join("meeting-details.json"),
        session_dir: candidate,
    })
}

/// Writes `meeting-details.json` atomically via a temporary sibling file and rename.
pub fn write_session_metadata_atomic(path: &Path, session: &MeetingSession) -> AppResult<()> {
    let parent = path.parent().ok_or_else(|| {
        AppError::Storage(format!(
            "Metadata path has no parent directory: {}",
            path.display()
        ))
    })?;
    fs::create_dir_all(parent)?;

    let json = serde_json::to_string_pretty(session)?;
    let tmp_path = parent.join("meeting-details.json.tmp");

    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(json.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }

    fs::rename(&tmp_path, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ChannelMapping, MuteEvent, Participant};
    use tempfile::tempdir;

    #[test]
    fn sanitizes_windows_forbidden_chars_and_reserved_names() {
        assert_eq!(sanitize_meeting_title("Daily Standup"), "Daily-Standup");
        assert_eq!(
            sanitize_meeting_title("Q4: Budget / Review * <Draft>?"),
            "Q4-Budget-Review-Draft"
        );
        assert_eq!(sanitize_meeting_title("   ...   "), "Untitled-Teams-Meeting");
        assert_eq!(sanitize_meeting_title("CON"), "meeting-CON");
        assert_eq!(sanitize_meeting_title("aux.txt"), "meeting-aux.txt");
    }

    #[test]
    fn creates_timestamped_dir_and_handles_collisions() {
        let dir = tempdir().expect("tempdir");
        let now = Local::now();

        let p1 = create_session_dir(dir.path(), now, "Daily Standup").expect("session 1");
        let p2 = create_session_dir(dir.path(), now, "Daily Standup").expect("session 2");

        assert!(p1.session_dir.exists());
        assert!(p2.session_dir.exists());
        assert_ne!(p1.session_dir, p2.session_dir);
        assert!(p1.audio_path.ends_with("recording.wav"));
        assert!(p1.metadata_path.ends_with("meeting-details.json"));
    }

    #[test]
    fn atomic_metadata_write_creates_and_replaces_json() {
        let dir = tempdir().expect("tempdir");
        let now = Local::now();
        let paths = create_session_dir(dir.path(), now, "Design Sync").expect("session dir");

        let mut session = MeetingSession {
            session_id: "test-uuid".to_string(),
            meeting_title: "Design Sync".to_string(),
            started_at: now,
            ended_at: None,
            duration_seconds: 0,
            audio_file: "recording.wav".to_string(),
            channels: ChannelMapping::default(),
            participants: vec![Participant {
                name: "Jayesh Krishna".to_string(),
                is_self: true,
                first_seen: now,
                last_seen: now,
            }],
            mute_events: vec![MuteEvent {
                timestamp: now,
                muted: true,
            }],
        };

        write_session_metadata_atomic(&paths.metadata_path, &session).expect("initial write");

        session.ended_at = Some(now);
        session.duration_seconds = 42;
        write_session_metadata_atomic(&paths.metadata_path, &session).expect("update write");

        let raw = fs::read_to_string(&paths.metadata_path).expect("read json");
        let parsed: MeetingSession = serde_json::from_str(&raw).expect("parse json");
        assert_eq!(parsed.duration_seconds, 42);
        assert_eq!(parsed.participants.len(), 1);
        assert!(!paths.session_dir.join("meeting-details.json.tmp").exists());
    }
}
