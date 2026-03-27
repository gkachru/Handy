use crate::actions::{
    DualStreamState, IncrementalSessionState, MistralSessionState, StreamingTranslatorState,
};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::shortcut;
use crate::TranscriptionCoordinator;
use log::info;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

// Re-export all utility modules for easy access
// pub use crate::audio_feedback::*;
pub use crate::clipboard::*;
pub use crate::overlay::*;
pub use crate::tray::*;

/// Centralized cancellation function that can be called from anywhere in the app.
/// Handles cancelling both recording and transcription operations and updates UI state.
pub async fn cancel_current_operation(app: &AppHandle) {
    info!("Initiating operation cancellation...");

    // Unregister the cancel shortcut asynchronously
    shortcut::unregister_cancel_shortcut(app);

    // Stop any active incremental transcription session
    {
        let state = app.state::<IncrementalSessionState>();
        let mut guard = state.0.lock().unwrap();
        if let Some(mut session) = guard.take() {
            let tm = app.state::<Arc<TranscriptionManager>>();
            session.stop(&tm);
            info!("Incremental transcription session cancelled");
        }
    }

    // Stop any active streaming translator
    {
        let translator_state = app.state::<StreamingTranslatorState>();
        let mut guard = translator_state.0.lock().unwrap();
        if let Some(mut translator) = guard.take() {
            translator.stop();
            info!("Streaming translator cancelled");
        }
    }

    // Stop any active dual-stream components (system audio session + coordinator + capture)
    // Take the dual-stream session out of the lock first, then await stop outside the lock.
    #[cfg(target_os = "macos")]
    let dual_stream_sys_session = {
        let dual_state = app.state::<DualStreamState>();
        let mut guard = dual_state.0.lock().unwrap();
        if let Some(dual) = guard.take() {
            let (sys_session, mut coordinator, mut sys_capture) = dual;
            coordinator.stop();
            sys_capture.stop();
            info!("Dual-stream components cancelled");
            Some(sys_session)
        } else {
            None
        }
    };
    #[cfg(not(target_os = "macos"))]
    {
        let dual_state = app.state::<DualStreamState>();
        let mut guard = dual_state.0.lock().unwrap();
        if guard.take().is_some() {
            info!("Dual-stream components cancelled");
        }
    }

    // Stop any active Mistral streaming session (take it out of the lock, then await)
    let mistral_session = {
        let session_state = app.state::<MistralSessionState>();
        let session = session_state.0.lock().unwrap().take();
        session
    };

    // Properly stop Mistral sessions with await (sets stop_signal + waits for cleanup)
    if let Some(mut session) = mistral_session {
        session.stop().await;
        info!("Mistral streaming session cancelled");
    }
    #[cfg(target_os = "macos")]
    if let Some(mut sys_session) = dual_stream_sys_session {
        sys_session.stop().await;
        info!("Dual-stream system session cancelled");
    }

    // Disable audio streaming
    let audio_manager = app.state::<Arc<AudioRecordingManager>>();
    audio_manager.disable_audio_streaming();

    // Stop system audio capture before cancelling recording — the recorder's
    // consumer thread may be blocked on sample_rx.recv(). Stopping the capture
    // disconnects the channel so the thread can exit.
    audio_manager.stop_system_audio_capture();

    // Cancel any ongoing recording
    let recording_was_active = audio_manager.is_recording();
    audio_manager.cancel_recording();

    // Update tray icon and hide overlay
    change_tray_icon(app, crate::tray::TrayIconState::Idle);
    hide_recording_overlay(app);

    // Unload model if immediate unload is enabled
    let tm = app.state::<Arc<TranscriptionManager>>();
    tm.maybe_unload_immediately("cancellation");

    // Notify coordinator so it can keep lifecycle state coherent.
    if let Some(coordinator) = app.try_state::<TranscriptionCoordinator>() {
        coordinator.notify_cancel(recording_was_active);
    }

    info!("Operation cancellation completed - returned to idle state");
}

/// Check if using the Wayland display server protocol
#[cfg(target_os = "linux")]
pub fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v.to_lowercase() == "wayland")
            .unwrap_or(false)
}

/// Check if running on KDE Plasma desktop environment
#[cfg(target_os = "linux")]
pub fn is_kde_plasma() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.to_uppercase().contains("KDE"))
        .unwrap_or(false)
        || std::env::var("KDE_SESSION_VERSION").is_ok()
}

/// Check if running on KDE Plasma with Wayland
#[cfg(target_os = "linux")]
pub fn is_kde_wayland() -> bool {
    is_wayland() && is_kde_plasma()
}
