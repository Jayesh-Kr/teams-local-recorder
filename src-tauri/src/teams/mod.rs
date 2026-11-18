pub mod process;
pub mod uia_watcher;

pub use process::{extract_meeting_title, find_teams_windows, TeamsWindow};
pub use uia_watcher::{
    parse_mic_button_muted, parse_participant_tile, spawn_teams_watcher, TeamsWatcherHandle,
};
