pub mod audio;
pub mod coordinator;
pub mod error;
pub mod models;
pub mod storage;
pub mod teams;

use std::process::Command;
use std::sync::Mutex;

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, State, WindowEvent};
use tokio::sync::mpsc;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::coordinator::{
    spawn_coordinator, CoordinatorHandle, TrayControls, PILL_WINDOW_LABEL,
};
use crate::error::{AppError, AppResult};
use crate::models::{RecordingState, RecordingStatus};
use crate::storage::{default_recordings_root, ensure_recordings_root};
use crate::teams::{spawn_teams_watcher, TeamsWatcherHandle};

const MENU_OPEN_FOLDER: &str = "open_recordings_folder";
const MENU_TOGGLE_RECORD: &str = "toggle_manual_recording";
const MENU_AUTO_RECORD: &str = "toggle_auto_record";
const MENU_QUIT: &str = "quit";

struct AppState {
    coordinator: CoordinatorHandle,
    watcher: Mutex<Option<TeamsWatcherHandle>>,
}

#[tauri::command]
async fn get_recording_state(state: State<'_, AppState>) -> AppResult<RecordingState> {
    state.coordinator.get_state().await
}

#[tauri::command]
async fn start_manual_recording(state: State<'_, AppState>) -> AppResult<RecordingState> {
    state.coordinator.start_manual_recording().await
}

#[tauri::command]
async fn stop_recording(state: State<'_, AppState>) -> AppResult<RecordingState> {
    state.coordinator.stop_recording().await
}

#[tauri::command]
async fn toggle_pause(state: State<'_, AppState>) -> AppResult<RecordingState> {
    state.coordinator.toggle_pause().await
}

#[tauri::command]
async fn toggle_local_mute(state: State<'_, AppState>) -> AppResult<RecordingState> {
    state.coordinator.toggle_manual_mute().await
}

#[tauri::command]
async fn set_auto_record(enabled: bool, state: State<'_, AppState>) -> AppResult<RecordingState> {
    state.coordinator.set_auto_record(enabled).await
}

#[tauri::command]
async fn open_recordings_dir(state: State<'_, AppState>) -> AppResult<()> {
    open_folder_in_explorer(state.coordinator.recordings_root())
}

fn open_folder_in_explorer(path: &std::path::Path) -> AppResult<()> {
    ensure_recordings_root(path)?;
    Command::new("explorer.exe")
        .arg(path)
        .spawn()
        .map_err(|e| AppError::Storage(format!("Failed to launch Explorer: {e}")))?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("teams_local_recorder_lib=info,info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

pub fn run() {
    init_tracing();
    info!("Starting Teams Local Recorder");

    let builder = tauri::Builder::default()
        .setup(|app| {
            let recordings_root = default_recordings_root()?;
            ensure_recordings_root(&recordings_root)?;

            // Build system tray menu
            let open_item = MenuItem::with_id(
                app,
                MENU_OPEN_FOLDER,
                "Open Recordings Folder",
                true,
                None::<&str>,
            )?;
            let toggle_recording_item = MenuItem::with_id(
                app,
                MENU_TOGGLE_RECORD,
                "Start Manual Recording",
                true,
                None::<&str>,
            )?;
            let auto_record_item = CheckMenuItem::with_id(
                app,
                MENU_AUTO_RECORD,
                "Auto-Record: Enabled",
                true,
                true,
                None::<&str>,
            )?;
            let separator = PredefinedMenuItem::separator(app)?;
            let quit_item = MenuItem::with_id(app, MENU_QUIT, "Quit", true, None::<&str>)?;

            let tray_menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &toggle_recording_item,
                    &auto_record_item,
                    &separator,
                    &quit_item,
                ],
            )?;

            let tray_controls = TrayControls {
                toggle_recording_item,
                auto_record_item,
            };

            let (teams_tx, teams_rx) = mpsc::unbounded_channel();
            let watcher = spawn_teams_watcher(teams_tx);
            let coordinator = spawn_coordinator(
                app.handle().clone(),
                tray_controls,
                recordings_root.clone(),
                teams_rx,
            );

            app.manage(AppState {
                coordinator: coordinator.clone(),
                watcher: Mutex::new(Some(watcher)),
            });

            // Build Windows system tray icon
            let icon = Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
            let tray_coord = coordinator.clone();
            let tray_root = recordings_root.clone();

            TrayIconBuilder::with_id("main-tray")
                .icon(icon)
                .tooltip("Teams Local Recorder")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app_handle, event| {
                    let id = event.id().as_ref();
                    match id {
                        MENU_OPEN_FOLDER => {
                            if let Err(err) = open_folder_in_explorer(&tray_root) {
                                warn!("Failed to open recordings folder: {err}");
                            }
                        }
                        MENU_TOGGLE_RECORD => {
                            let coord = tray_coord.clone();
                            tauri::async_runtime::spawn(async move {
                                match coord.get_state().await {
                                    Ok(s) if s.status == RecordingStatus::Idle => {
                                        if let Err(err) = coord.start_manual_recording().await {
                                            warn!("Manual recording start failed: {err}");
                                        }
                                    }
                                    Ok(_) => {
                                        if let Err(err) = coord.stop_recording().await {
                                            warn!("Manual recording stop failed: {err}");
                                        }
                                    }
                                    Err(err) => warn!("Failed to query recorder state: {err}"),
                                }
                            });
                        }
                        MENU_AUTO_RECORD => {
                            let coord = tray_coord.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Err(err) = coord.toggle_auto_record().await {
                                    warn!("Failed to toggle auto-record: {err}");
                                }
                            });
                        }
                        MENU_QUIT => {
                            let app_for_quit = app_handle.clone();
                            tauri::async_runtime::spawn(async move {
                                let state = app_for_quit.state::<AppState>();
                                if let Ok(mut guard) = state.watcher.lock() {
                                    if let Some(mut w) = guard.take() {
                                        w.stop();
                                    }
                                }
                                state.coordinator.shutdown().await;
                                app_for_quit.exit(0);
                            });
                        }
                        _ => {}
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app_handle = tray.app_handle().clone();
                        tauri::async_runtime::spawn(async move {
                            let state = app_handle.state::<AppState>();
                            if let Ok(rec_state) = state.coordinator.get_state().await {
                                if rec_state.status != RecordingStatus::Idle {
                                    if let Some(win) =
                                        app_handle.get_webview_window(PILL_WINDOW_LABEL)
                                    {
                                        if win.is_visible().unwrap_or(false) {
                                            let _ = win.hide();
                                        } else {
                                            let _ = win.show();
                                        }
                                    }
                                }
                            }
                        });
                    }
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Keep app running silently in system tray if pill close is triggered.
            if window.label() == PILL_WINDOW_LABEL {
                if let WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_recording_state,
            start_manual_recording,
            stop_recording,
            toggle_pause,
            toggle_local_mute,
            set_auto_record,
            open_recordings_dir,
        ]);

    if let Err(err) = builder.run(tauri::generate_context!()) {
        tracing::error!("Teams Local Recorder exited with error: {err}");
    }
}
