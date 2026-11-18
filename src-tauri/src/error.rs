use serde::Serialize;
use thiserror::Error;

/// Unified domain error type for Teams Local Recorder.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("WAV encoding error: {0}")]
    Wav(#[from] hound::Error),

    #[error("Audio initialization failed: {0}")]
    AudioInit(String),

    #[error("Audio capture error: {0}")]
    AudioCapture(String),

    #[error("Audio resampler error: {0}")]
    Resampler(String),

    #[error("Win32 API error: {0}")]
    Win32(String),

    #[error("UI Automation error: {0}")]
    Uia(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("Invalid recording state transition: {0}")]
    StateTransition(String),

    #[error("Tauri runtime error: {0}")]
    Tauri(String),

    #[error("Coordinator channel closed")]
    ChannelClosed,
}

impl From<tauri::Error> for AppError {
    fn from(err: tauri::Error) -> Self {
        Self::Tauri(err.to_string())
    }
}

#[cfg(windows)]
impl From<windows::core::Error> for AppError {
    fn from(err: windows::core::Error) -> Self {
        Self::Win32(err.to_string())
    }
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
