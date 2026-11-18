use std::path::PathBuf;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// Channel layout metadata written to `meeting-details.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChannelMapping {
    pub left: String,
    pub right: String,
}

impl Default for ChannelMapping {
    fn default() -> Self {
        Self {
            left: "local_microphone".to_string(),
            right: "remote_participants".to_string(),
        }
    }
}

/// Participant observed during a Microsoft Teams meeting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Participant {
    pub name: String,
    pub is_self: bool,
    pub first_seen: DateTime<Local>,
    pub last_seen: DateTime<Local>,
}

/// Local microphone mute state transition recorded during a session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MuteEvent {
    pub timestamp: DateTime<Local>,
    pub muted: bool,
}

/// Persistent session metadata stored in `meeting-details.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MeetingSession {
    pub session_id: String,
    pub meeting_title: String,
    pub started_at: DateTime<Local>,
    pub ended_at: Option<DateTime<Local>>,
    pub duration_seconds: u64,
    pub audio_file: String,
    pub channels: ChannelMapping,
    pub participants: Vec<Participant>,
    pub mute_events: Vec<MuteEvent>,
}

/// Current lifecycle status of the recorder.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    Idle,
    Recording,
    Paused,
}

/// How the active recording session was started.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecordingSource {
    AutoTeams,
    Manual,
}

/// Live UI state emitted to the floating pill via `recording://state-updated`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordingState {
    pub status: RecordingStatus,
    pub source: Option<RecordingSource>,
    pub meeting_title: String,
    pub elapsed_seconds: u64,
    pub is_mic_muted: bool,
    pub is_teams_muted: bool,
    pub is_manual_muted: bool,
    pub participant_count: usize,
    pub participant_names: Vec<String>,
    pub auto_record_enabled: bool,
    pub in_teams_call: bool,
    pub session_dir: Option<String>,
    pub last_error: Option<String>,
}

impl Default for RecordingState {
    fn default() -> Self {
        Self {
            status: RecordingStatus::Idle,
            source: None,
            meeting_title: String::new(),
            elapsed_seconds: 0,
            is_mic_muted: false,
            is_teams_muted: false,
            is_manual_muted: false,
            participant_count: 0,
            participant_names: Vec::new(),
            auto_record_enabled: true,
            in_teams_call: false,
            session_dir: None,
            last_error: None,
        }
    }
}

/// Runtime application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub auto_record_enabled: bool,
    pub recordings_root: PathBuf,
}

/// A participant parsed from a Teams UI Automation tile.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DetectedParticipant {
    pub name: String,
    pub is_self: bool,
}

/// Periodic snapshot emitted by the UI Automation Teams watcher thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TeamsSnapshot {
    pub is_in_call: bool,
    pub is_mic_muted: bool,
    pub meeting_title: Option<String>,
    pub participants: Vec<DetectedParticipant>,
}
