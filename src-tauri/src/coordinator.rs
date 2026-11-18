use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::Local;
use tauri::menu::{CheckMenuItem, MenuItem};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, Wry};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::audio::AudioRecorder;
use crate::error::{AppError, AppResult};
use crate::models::{
    ChannelMapping, DetectedParticipant, MeetingSession, MuteEvent, Participant, RecordingSource,
    RecordingState, RecordingStatus, TeamsSnapshot,
};
use crate::storage::{create_session_dir, write_session_metadata_atomic, SessionPaths};

pub const STATE_UPDATED_EVENT: &str = "recording://state-updated";
pub const PILL_WINDOW_LABEL: &str = "pill";

/// Commands accepted by the async Coordinator task from IPC and System Tray handlers.
pub enum CoordinatorCommand {
    StartManualRecording(oneshot::Sender<AppResult<RecordingState>>),
    StopRecording(oneshot::Sender<AppResult<RecordingState>>),
    TogglePause(oneshot::Sender<AppResult<RecordingState>>),
    ToggleManualMute(oneshot::Sender<AppResult<RecordingState>>),
    SetAutoRecord(bool, oneshot::Sender<AppResult<RecordingState>>),
    ToggleAutoRecord(oneshot::Sender<AppResult<RecordingState>>),
    GetState(oneshot::Sender<RecordingState>),
    Shutdown(oneshot::Sender<()>),
}

/// Tray menu handles updated dynamically whenever recording or auto-record state changes.
pub struct TrayControls {
    pub toggle_recording_item: MenuItem<Wry>,
    pub auto_record_item: CheckMenuItem<Wry>,
}

/// Thread-safe handle stored in Tauri managed state for talking to the Coordinator.
#[derive(Clone)]
pub struct CoordinatorHandle {
    cmd_tx: mpsc::UnboundedSender<CoordinatorCommand>,
    recordings_root: PathBuf,
}

impl CoordinatorHandle {
    pub fn recordings_root(&self) -> &PathBuf {
        &self.recordings_root
    }

    pub async fn get_state(&self) -> AppResult<RecordingState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CoordinatorCommand::GetState(tx))
            .map_err(|_| AppError::ChannelClosed)?;
        rx.await.map_err(|_| AppError::ChannelClosed)
    }

    pub async fn start_manual_recording(&self) -> AppResult<RecordingState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CoordinatorCommand::StartManualRecording(tx))
            .map_err(|_| AppError::ChannelClosed)?;
        rx.await.map_err(|_| AppError::ChannelClosed)?
    }

    pub async fn stop_recording(&self) -> AppResult<RecordingState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CoordinatorCommand::StopRecording(tx))
            .map_err(|_| AppError::ChannelClosed)?;
        rx.await.map_err(|_| AppError::ChannelClosed)?
    }

    pub async fn toggle_pause(&self) -> AppResult<RecordingState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CoordinatorCommand::TogglePause(tx))
            .map_err(|_| AppError::ChannelClosed)?;
        rx.await.map_err(|_| AppError::ChannelClosed)?
    }

    pub async fn toggle_manual_mute(&self) -> AppResult<RecordingState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CoordinatorCommand::ToggleManualMute(tx))
            .map_err(|_| AppError::ChannelClosed)?;
        rx.await.map_err(|_| AppError::ChannelClosed)?
    }

    pub async fn set_auto_record(&self, enabled: bool) -> AppResult<RecordingState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CoordinatorCommand::SetAutoRecord(enabled, tx))
            .map_err(|_| AppError::ChannelClosed)?;
        rx.await.map_err(|_| AppError::ChannelClosed)?
    }

    pub async fn toggle_auto_record(&self) -> AppResult<RecordingState> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(CoordinatorCommand::ToggleAutoRecord(tx))
            .map_err(|_| AppError::ChannelClosed)?;
        rx.await.map_err(|_| AppError::ChannelClosed)?
    }

    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd_tx.send(CoordinatorCommand::Shutdown(tx)).is_ok() {
            let _ = rx.await;
        }
    }
}

struct ActiveSession {
    source: RecordingSource,
    paths: SessionPaths,
    session: MeetingSession,
    participants_map: HashMap<String, Participant>,
    recorder: AudioRecorder,
    is_paused: bool,
    accumulated_active: Duration,
    active_segment_start: Option<Instant>,
    teams_muted: bool,
    manual_muted: bool,
    last_effective_muted: bool,
}

impl ActiveSession {
    fn effective_muted(&self) -> bool {
        self.teams_muted || self.manual_muted
    }

    fn elapsed_seconds(&self) -> u64 {
        let current = self
            .active_segment_start
            .map(|start| start.elapsed())
            .unwrap_or(Duration::ZERO);
        (self.accumulated_active + current).as_secs()
    }

    fn sync_participants_vec(&mut self) {
        let mut list: Vec<Participant> = self.participants_map.values().cloned().collect();
        list.sort_by(|a, b| {
            b.is_self
                .cmp(&a.is_self)
                .then_with(|| a.first_seen.cmp(&b.first_seen))
                .then_with(|| a.name.cmp(&b.name))
        });
        self.session.participants = list;
    }

    /// Merges newly observed Teams participants into the deduplicated session map.
    /// Returns `true` if a new participant was added.
    fn merge_participants(&mut self, detected: &[DetectedParticipant]) -> bool {
        let now = Local::now();
        let mut changed = false;

        for item in detected {
            if let Some(existing) = self.participants_map.get_mut(&item.name) {
                existing.last_seen = now;
                if item.is_self && !existing.is_self {
                    existing.is_self = true;
                    changed = true;
                }
            } else {
                self.participants_map.insert(
                    item.name.clone(),
                    Participant {
                        name: item.name.clone(),
                        is_self: item.is_self,
                        first_seen: now,
                        last_seen: now,
                    },
                );
                changed = true;
            }
        }

        if changed {
            self.sync_participants_vec();
        }
        changed
    }

    /// Recomputes effective mute state (`teams_muted || manual_muted`), propagates to audio thread,
    /// and logs a `MuteEvent` if it changed. Returns `true` if effective mute changed.
    fn sync_mute_state(&mut self) -> bool {
        let effective = self.effective_muted();
        self.recorder.set_mic_muted(effective);
        if effective != self.last_effective_muted {
            self.last_effective_muted = effective;
            self.session.mute_events.push(MuteEvent {
                timestamp: Local::now(),
                muted: effective,
            });
            return true;
        }
        false
    }

    fn set_paused(&mut self, paused: bool) {
        if self.is_paused == paused {
            return;
        }
        if paused {
            if let Some(start) = self.active_segment_start.take() {
                self.accumulated_active += start.elapsed();
            }
            self.is_paused = true;
            self.recorder.set_paused(true);
        } else {
            self.active_segment_start = Some(Instant::now());
            self.is_paused = false;
            self.recorder.set_paused(false);
        }
    }

    fn persist_metadata(&mut self) {
        self.session.duration_seconds = self.elapsed_seconds();
        if let Err(err) = write_session_metadata_atomic(&self.paths.metadata_path, &self.session) {
            warn!("Failed to persist meeting-details.json: {err}");
        }
    }
}

struct Coordinator {
    app: AppHandle<Wry>,
    tray: TrayControls,
    recordings_root: PathBuf,
    auto_record_enabled: bool,
    in_teams_call: bool,
    latest_teams_snapshot: TeamsSnapshot,
    /// Prevents auto-record from immediately restarting if the user manually stops an auto recording mid-call.
    suppressed_for_current_call: bool,
    active: Option<ActiveSession>,
    last_error: Option<String>,
}

impl Coordinator {
    fn snapshot_state(&self) -> RecordingState {
        if let Some(active) = self.active.as_ref() {
            let status = if active.is_paused {
                RecordingStatus::Paused
            } else {
                RecordingStatus::Recording
            };
            let participant_names = active
                .session
                .participants
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>();
            RecordingState {
                status,
                source: Some(active.source),
                meeting_title: active.session.meeting_title.clone(),
                elapsed_seconds: active.elapsed_seconds(),
                is_mic_muted: active.effective_muted(),
                is_teams_muted: active.teams_muted,
                is_manual_muted: active.manual_muted,
                participant_count: participant_names.len(),
                participant_names,
                auto_record_enabled: self.auto_record_enabled,
                in_teams_call: self.in_teams_call,
                session_dir: Some(active.paths.session_dir.to_string_lossy().into_owned()),
                last_error: self.last_error.clone(),
            }
        } else {
            RecordingState {
                status: RecordingStatus::Idle,
                source: None,
                meeting_title: String::new(),
                elapsed_seconds: 0,
                is_mic_muted: false,
                is_teams_muted: false,
                is_manual_muted: false,
                participant_count: 0,
                participant_names: Vec::new(),
                auto_record_enabled: self.auto_record_enabled,
                in_teams_call: self.in_teams_call,
                session_dir: None,
                last_error: self.last_error.clone(),
            }
        }
    }

    fn emit_and_sync_ui(&self) {
        let state = self.snapshot_state();

        if let Err(err) = self.app.emit(STATE_UPDATED_EVENT, &state) {
            warn!("Failed to emit {STATE_UPDATED_EVENT}: {err}");
        }

        let record_label = match state.status {
            RecordingStatus::Idle => "Start Manual Recording",
            RecordingStatus::Recording | RecordingStatus::Paused => "Stop Recording",
        };
        if let Err(err) = self.tray.toggle_recording_item.set_text(record_label) {
            warn!("Failed to update tray record menu label: {err}");
        }

        let auto_label = if state.auto_record_enabled {
            "Auto-Record: Enabled"
        } else {
            "Auto-Record: Disabled"
        };
        let _ = self.tray.auto_record_item.set_text(auto_label);
        let _ = self
            .tray
            .auto_record_item
            .set_checked(state.auto_record_enabled);
    }

    fn show_pill_window(&self) {
        if let Some(window) = self.app.get_webview_window(PILL_WINDOW_LABEL) {
            position_pill_top_center(&window);
            if let Err(err) = window.show() {
                warn!("Failed to show floating pill window: {err}");
            }
        }
    }

    fn hide_pill_window(&self) {
        if let Some(window) = self.app.get_webview_window(PILL_WINDOW_LABEL) {
            if let Err(err) = window.hide() {
                warn!("Failed to hide floating pill window: {err}");
            }
        }
    }

    fn start_recording(
        &mut self,
        source: RecordingSource,
        title: String,
        initial_teams_muted: bool,
        initial_participants: &[DetectedParticipant],
    ) -> AppResult<RecordingState> {
        if self.active.is_some() {
            return Err(AppError::StateTransition(
                "A recording session is already active".to_string(),
            ));
        }

        let started_at = Local::now();
        let paths = create_session_dir(&self.recordings_root, started_at, &title)?;
        let recorder = AudioRecorder::start(paths.audio_path.clone(), initial_teams_muted)?;

        let mut mute_events = Vec::new();
        if initial_teams_muted {
            mute_events.push(MuteEvent {
                timestamp: started_at,
                muted: true,
            });
        }

        let session = MeetingSession {
            session_id: Uuid::new_v4().to_string(),
            meeting_title: title,
            started_at,
            ended_at: None,
            duration_seconds: 0,
            audio_file: "recording.wav".to_string(),
            channels: ChannelMapping::default(),
            participants: Vec::new(),
            mute_events,
        };

        let mut active = ActiveSession {
            source,
            paths,
            session,
            participants_map: HashMap::new(),
            recorder,
            is_paused: false,
            accumulated_active: Duration::ZERO,
            active_segment_start: Some(Instant::now()),
            teams_muted: initial_teams_muted,
            manual_muted: false,
            last_effective_muted: initial_teams_muted,
        };

        active.merge_participants(initial_participants);
        active.persist_metadata();

        info!(
            source = ?source,
            dir = %active.paths.session_dir.display(),
            "Recording session started"
        );

        self.last_error = None;
        self.active = Some(active);
        self.show_pill_window();
        self.emit_and_sync_ui();

        Ok(self.snapshot_state())
    }

    async fn stop_recording(&mut self, manual_trigger: bool) -> AppResult<RecordingState> {
        let Some(mut active) = self.active.take() else {
            return Ok(self.snapshot_state());
        };

        if manual_trigger && self.in_teams_call && active.source == RecordingSource::AutoTeams {
            self.suppressed_for_current_call = true;
        }

        let final_duration = active.elapsed_seconds();
        let ended_at = Local::now();
        let metadata_path = active.paths.metadata_path.clone();
        let session_dir = active.paths.session_dir.clone();

        active.session.ended_at = Some(ended_at);
        active.session.duration_seconds = final_duration;
        active.sync_participants_vec();

        // Finalize WAV and join capture threads off the async Tokio executor.
        let recorder = active.recorder;
        if let Err(err) = tokio::task::spawn_blocking(move || recorder.stop()).await {
            warn!("Blocking join for audio recorder stop failed: {err}");
        }

        if let Err(err) = write_session_metadata_atomic(&metadata_path, &active.session) {
            error!("Failed to write final meeting-details.json: {err}");
            self.last_error = Some(err.to_string());
        } else {
            info!(
                duration_seconds = final_duration,
                dir = %session_dir.display(),
                "Recording session finalized"
            );
        }

        self.hide_pill_window();
        self.emit_and_sync_ui();

        Ok(self.snapshot_state())
    }

    async fn handle_teams_snapshot(&mut self, snapshot: TeamsSnapshot) {
        let was_in_call = self.in_teams_call;
        self.in_teams_call = snapshot.is_in_call;
        self.latest_teams_snapshot = snapshot.clone();

        if !was_in_call && snapshot.is_in_call {
            info!(
                title = ?snapshot.meeting_title,
                "Detected Microsoft Teams call start"
            );
            self.suppressed_for_current_call = false;

            if self.auto_record_enabled && self.active.is_none() {
                let title = snapshot
                    .meeting_title
                    .clone()
                    .unwrap_or_else(|| "Untitled Teams Meeting".to_string());
                if let Err(err) = self.start_recording(
                    RecordingSource::AutoTeams,
                    title,
                    snapshot.is_mic_muted,
                    &snapshot.participants,
                ) {
                    error!("Auto-record start failed: {err}");
                    self.last_error = Some(err.to_string());
                    self.emit_and_sync_ui();
                }
                return;
            }
        } else if was_in_call && !snapshot.is_in_call {
            info!("Detected Microsoft Teams call end");
            self.suppressed_for_current_call = false;

            if self
                .active
                .as_ref()
                .map(|a| a.source == RecordingSource::AutoTeams)
                .unwrap_or(false)
            {
                let _ = self.stop_recording(false).await;
                return;
            }
        } else if snapshot.is_in_call
            && self.auto_record_enabled
            && !self.suppressed_for_current_call
            && self.active.is_none()
        {
            // User enabled Auto-Record while already in an active Teams call.
            let title = snapshot
                .meeting_title
                .clone()
                .unwrap_or_else(|| "Untitled Teams Meeting".to_string());
            if let Err(err) = self.start_recording(
                RecordingSource::AutoTeams,
                title,
                snapshot.is_mic_muted,
                &snapshot.participants,
            ) {
                error!("Auto-record start failed: {err}");
                self.last_error = Some(err.to_string());
                self.emit_and_sync_ui();
            }
            return;
        }

        // Enrich any active session (AutoTeams or Manual) with live Teams mute, title & participants.
        if snapshot.is_in_call {
            if let Some(active) = self.active.as_mut() {
                let mut metadata_dirty = false;

                if active.teams_muted != snapshot.is_mic_muted {
                    active.teams_muted = snapshot.is_mic_muted;
                    if active.sync_mute_state() {
                        metadata_dirty = true;
                    }
                }

                if let Some(ref title) = snapshot.meeting_title {
                    let is_generic = active.session.meeting_title == "Untitled Teams Meeting"
                        || active.session.meeting_title == "Manual Recording";
                    if is_generic && title != "Untitled Teams Meeting" {
                        active.session.meeting_title = title.clone();
                        metadata_dirty = true;
                    }
                }

                if !snapshot.participants.is_empty()
                    && active.merge_participants(&snapshot.participants)
                {
                    metadata_dirty = true;
                }

                if metadata_dirty {
                    active.persist_metadata();
                }
                self.emit_and_sync_ui();
            }
        }
    }

    async fn handle_command(&mut self, cmd: CoordinatorCommand) -> bool {
        match cmd {
            CoordinatorCommand::GetState(reply) => {
                let _ = reply.send(self.snapshot_state());
            }
            CoordinatorCommand::StartManualRecording(reply) => {
                let title = if self.in_teams_call {
                    self.latest_teams_snapshot
                        .meeting_title
                        .clone()
                        .unwrap_or_else(|| "Manual Recording".to_string())
                } else {
                    "Manual Recording".to_string()
                };
                let initial_muted = if self.in_teams_call {
                    self.latest_teams_snapshot.is_mic_muted
                } else {
                    false
                };
                let participants = if self.in_teams_call {
                    self.latest_teams_snapshot.participants.clone()
                } else {
                    Vec::new()
                };
                let res = self.start_recording(
                    RecordingSource::Manual,
                    title,
                    initial_muted,
                    &participants,
                );
                if let Err(ref err) = res {
                    self.last_error = Some(err.to_string());
                    self.emit_and_sync_ui();
                }
                let _ = reply.send(res);
            }
            CoordinatorCommand::StopRecording(reply) => {
                let res = self.stop_recording(true).await;
                let _ = reply.send(res);
            }
            CoordinatorCommand::TogglePause(reply) => {
                let res = if let Some(active) = self.active.as_mut() {
                    let next_paused = !active.is_paused;
                    active.set_paused(next_paused);
                    self.emit_and_sync_ui();
                    Ok(self.snapshot_state())
                } else {
                    Err(AppError::StateTransition(
                        "Cannot pause when not recording".to_string(),
                    ))
                };
                let _ = reply.send(res);
            }
            CoordinatorCommand::ToggleManualMute(reply) => {
                let res = if let Some(active) = self.active.as_mut() {
                    active.manual_muted = !active.manual_muted;
                    if active.sync_mute_state() {
                        active.persist_metadata();
                    }
                    self.emit_and_sync_ui();
                    Ok(self.snapshot_state())
                } else {
                    Err(AppError::StateTransition(
                        "Cannot toggle mute when not recording".to_string(),
                    ))
                };
                let _ = reply.send(res);
            }
            CoordinatorCommand::SetAutoRecord(enabled, reply) => {
                self.auto_record_enabled = enabled;
                if enabled {
                    self.suppressed_for_current_call = false;
                }
                self.emit_and_sync_ui();
                let _ = reply.send(Ok(self.snapshot_state()));
            }
            CoordinatorCommand::ToggleAutoRecord(reply) => {
                self.auto_record_enabled = !self.auto_record_enabled;
                if self.auto_record_enabled {
                    self.suppressed_for_current_call = false;
                }
                self.emit_and_sync_ui();
                let _ = reply.send(Ok(self.snapshot_state()));
            }
            CoordinatorCommand::Shutdown(reply) => {
                info!("Coordinator shutting down; finalizing any active recording");
                let _ = self.stop_recording(false).await;
                let _ = reply.send(());
                return false;
            }
        }
        true
    }
}

/// Spawns the async Coordinator task and returns its command handle.
pub fn spawn_coordinator(
    app: AppHandle<Wry>,
    tray: TrayControls,
    recordings_root: PathBuf,
    mut teams_rx: mpsc::UnboundedReceiver<TeamsSnapshot>,
) -> CoordinatorHandle {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel();

    let handle = CoordinatorHandle {
        cmd_tx,
        recordings_root: recordings_root.clone(),
    };

    tauri::async_runtime::spawn(async move {
        let mut coordinator = Coordinator {
            app,
            tray,
            recordings_root,
            auto_record_enabled: true,
            in_teams_call: false,
            latest_teams_snapshot: TeamsSnapshot::default(),
            suppressed_for_current_call: false,
            active: None,
            last_error: None,
        };

        coordinator.emit_and_sync_ui();

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                Some(cmd) = cmd_rx.recv() => {
                    if !coordinator.handle_command(cmd).await {
                        break;
                    }
                }
                Some(snapshot) = teams_rx.recv() => {
                    coordinator.handle_teams_snapshot(snapshot).await;
                }
                _ = tick.tick() => {
                    if coordinator.active.is_some() {
                        coordinator.emit_and_sync_ui();
                    }
                }
            }
        }
    });

    handle
}

/// Positions the floating pill cleanly at top-center of the primary monitor.
fn position_pill_top_center(window: &tauri::WebviewWindow<Wry>) {
    if let Ok(Some(monitor)) = window.primary_monitor() {
        let mon_pos = monitor.position();
        let mon_size = monitor.size();
        if let Ok(win_size) = window.outer_size() {
            let x = mon_pos.x + ((mon_size.width as i32) - (win_size.width as i32)) / 2;
            let y = mon_pos.y + 18;
            let _ = window.set_position(PhysicalPosition::new(x, y));
        }
    }
}
