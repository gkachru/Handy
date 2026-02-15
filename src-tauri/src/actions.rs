#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
use crate::apple_intelligence;
use crate::audio_feedback::{play_feedback_sound, play_feedback_sound_blocking, SoundType};
use crate::managers::audio::AudioRecordingManager;
use crate::managers::dual_stream::DualStreamCoordinator;
use crate::managers::history::HistoryManager;
use crate::managers::incremental_transcription::IncrementalTranscriptionSession;
use crate::managers::mistral_realtime::MistralRealtimeSession;
use crate::managers::model::{EngineType, ModelManager};
use crate::managers::streaming_translator::{self, StreamingTranslator};
use crate::managers::transcription::TranscriptionManager;
use crate::settings::{get_settings, AudioSource, AppSettings, APPLE_INTELLIGENCE_PROVIDER_ID};
use crate::shortcut;
use crate::tray::{change_tray_icon, TrayIconState};
use crate::utils::{
    self, show_processing_overlay, show_recording_overlay, show_streaming_overlay,
    show_streaming_overlay_ext, show_transcribing_overlay,
};
use crate::ManagedToggleState;
use ferrous_opencc::{config::BuiltinConfig, OpenCC};
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::AppHandle;
use tauri::Manager;

/// Managed state for an active Mistral streaming session
pub struct MistralSessionState(pub Mutex<Option<MistralRealtimeSession>>);

/// Managed state for an active streaming translator
pub struct StreamingTranslatorState(pub Mutex<Option<StreamingTranslator>>);

/// Managed state for an active incremental (VAD-segmented) transcription session
pub struct IncrementalSessionState(pub Mutex<Option<IncrementalTranscriptionSession>>);

/// Managed state for dual-stream mode (mic + system audio).
/// Holds the second Mistral session (system audio), the coordinator, and the system audio capture.
#[cfg(target_os = "macos")]
pub struct DualStreamState(
    pub Mutex<
        Option<(
            MistralRealtimeSession,
            DualStreamCoordinator,
            crate::audio_toolkit::system_audio::SystemAudioCapture,
        )>,
    >,
);

#[cfg(not(target_os = "macos"))]
pub struct DualStreamState(pub Mutex<Option<()>>);

// Shortcut Action Trait
pub trait ShortcutAction: Send + Sync {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str);
}

// Transcribe Action
struct TranscribeAction {
    post_process: bool,
}

async fn post_process_transcription(settings: &AppSettings, transcription: &str) -> Option<String> {
    let provider = match settings.active_post_process_provider().cloned() {
        Some(provider) => provider,
        None => {
            debug!("Post-processing enabled but no provider is selected");
            return None;
        }
    };

    let model = settings
        .post_process_models
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    if model.trim().is_empty() {
        debug!(
            "Post-processing skipped because provider '{}' has no model configured",
            provider.id
        );
        return None;
    }

    let selected_prompt_id = match &settings.post_process_selected_prompt_id {
        Some(id) => id.clone(),
        None => {
            debug!("Post-processing skipped because no prompt is selected");
            return None;
        }
    };

    let prompt = match settings
        .post_process_prompts
        .iter()
        .find(|prompt| prompt.id == selected_prompt_id)
    {
        Some(prompt) => prompt.prompt.clone(),
        None => {
            debug!(
                "Post-processing skipped because prompt '{}' was not found",
                selected_prompt_id
            );
            return None;
        }
    };

    if prompt.trim().is_empty() {
        debug!("Post-processing skipped because the selected prompt is empty");
        return None;
    }

    debug!(
        "Starting LLM post-processing with provider '{}' (model: {})",
        provider.id, model
    );

    // Replace ${output} variable in the prompt with the actual text
    let processed_prompt = prompt.replace("${output}", transcription);
    debug!("Processed prompt length: {} chars", processed_prompt.len());

    if provider.id == APPLE_INTELLIGENCE_PROVIDER_ID {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            if !apple_intelligence::check_apple_intelligence_availability() {
                debug!("Apple Intelligence selected but not currently available on this device");
                return None;
            }

            let token_limit = model.trim().parse::<i32>().unwrap_or(0);
            return match apple_intelligence::process_text(&processed_prompt, token_limit) {
                Ok(result) => {
                    if result.trim().is_empty() {
                        debug!("Apple Intelligence returned an empty response");
                        None
                    } else {
                        debug!(
                            "Apple Intelligence post-processing succeeded. Output length: {} chars",
                            result.len()
                        );
                        Some(result)
                    }
                }
                Err(err) => {
                    error!("Apple Intelligence post-processing failed: {}", err);
                    None
                }
            };
        }

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            debug!("Apple Intelligence provider selected on unsupported platform");
            return None;
        }
    }

    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .cloned()
        .unwrap_or_default();

    // Send the chat completion request
    match crate::llm_client::send_chat_completion(&provider, api_key, &model, processed_prompt)
        .await
    {
        Ok(Some(content)) => {
            // Strip invisible Unicode characters that some LLMs (e.g., Qwen) may insert
            let content = content
                .replace('\u{200B}', "") // Zero-Width Space
                .replace('\u{200C}', "") // Zero-Width Non-Joiner
                .replace('\u{200D}', "") // Zero-Width Joiner
                .replace('\u{FEFF}', ""); // Byte Order Mark / Zero-Width No-Break Space
            debug!(
                "LLM post-processing succeeded for provider '{}'. Output length: {} chars",
                provider.id,
                content.len()
            );
            Some(content)
        }
        Ok(None) => {
            error!("LLM API response has no content");
            None
        }
        Err(e) => {
            error!(
                "LLM post-processing failed for provider '{}': {}. Falling back to original transcription.",
                provider.id,
                e
            );
            None
        }
    }
}

async fn maybe_convert_chinese_variant(
    settings: &AppSettings,
    transcription: &str,
) -> Option<String> {
    // Check if language is set to Simplified or Traditional Chinese
    let is_simplified = settings.selected_language == "zh-Hans";
    let is_traditional = settings.selected_language == "zh-Hant";

    if !is_simplified && !is_traditional {
        debug!("selected_language is not Simplified or Traditional Chinese; skipping translation");
        return None;
    }

    debug!(
        "Starting Chinese translation using OpenCC for language: {}",
        settings.selected_language
    );

    // Use OpenCC to convert based on selected language
    let config = if is_simplified {
        // Convert Traditional Chinese to Simplified Chinese
        BuiltinConfig::Tw2sp
    } else {
        // Convert Simplified Chinese to Traditional Chinese
        BuiltinConfig::S2twp
    };

    match OpenCC::from_config(config) {
        Ok(converter) => {
            let converted = converter.convert(transcription);
            debug!(
                "OpenCC translation completed. Input length: {}, Output length: {}",
                transcription.len(),
                converted.len()
            );
            Some(converted)
        }
        Err(e) => {
            error!("Failed to initialize OpenCC converter: {}. Falling back to original transcription.", e);
            None
        }
    }
}

impl ShortcutAction for TranscribeAction {
    fn start(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        let start_time = Instant::now();
        debug!("TranscribeAction::start called for binding: {}", binding_id);

        // Load model in the background
        let tm = app.state::<Arc<TranscriptionManager>>();
        tm.initiate_model_load();

        let binding_id = binding_id.to_string();
        let rm = app.state::<Arc<AudioRecordingManager>>();

        // Check if using Mistral API for streaming mode
        let settings = get_settings(app);
        let is_mistral_api = settings.selected_model == "mistral-voxtral-realtime"
            && !settings.mistral_api_key.is_empty();

        // Check if the selected model supports incremental (VAD-segmented) streaming
        let supports_incremental = if settings.realtime_transcription_enabled {
            let mm = app.state::<Arc<ModelManager>>();
            if let Some(info) = mm.get_model_info(&settings.selected_model) {
                matches!(
                    info.engine_type,
                    EngineType::Parakeet | EngineType::Moonshine
                )
            } else {
                false
            }
        } else {
            false
        };

        let is_dual_stream = is_mistral_api && settings.audio_source == AudioSource::Mixed;

        if is_mistral_api {
            change_tray_icon(app, TrayIconState::Recording);
            show_streaming_overlay(app);
        } else if supports_incremental {
            change_tray_icon(app, TrayIconState::Recording);
            // No translation panel for local incremental models (English-only)
            show_streaming_overlay_ext(app, false);
        } else {
            change_tray_icon(app, TrayIconState::Recording);
            show_recording_overlay(app);
        }

        let is_always_on = settings.always_on_microphone;
        debug!("Microphone mode - always_on: {}", is_always_on);

        let mut recording_started = false;
        if is_always_on {
            // Always-on mode: Play audio feedback immediately, then apply mute after sound finishes
            debug!("Always-on mode: Playing audio feedback immediately");
            let rm_clone = Arc::clone(&rm);
            let app_clone = app.clone();
            // The blocking helper exits immediately if audio feedback is disabled,
            // so we can always reuse this thread to ensure mute happens right after playback.
            std::thread::spawn(move || {
                play_feedback_sound_blocking(&app_clone, SoundType::Start);
                rm_clone.apply_mute();
            });

            recording_started = rm.try_start_recording(&binding_id);
            debug!("Recording started: {}", recording_started);
        } else {
            // On-demand mode: Start recording first, then play audio feedback, then apply mute
            // This allows the microphone to be activated before playing the sound
            debug!("On-demand mode: Starting recording first, then audio feedback");
            let recording_start_time = Instant::now();
            if rm.try_start_recording(&binding_id) {
                recording_started = true;
                debug!("Recording started in {:?}", recording_start_time.elapsed());
                // Small delay to ensure microphone stream is active
                let app_clone = app.clone();
                let rm_clone = Arc::clone(&rm);
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    debug!("Handling delayed audio feedback/mute sequence");
                    // Helper handles disabled audio feedback by returning early, so we reuse it
                    // to keep mute sequencing consistent in every mode.
                    play_feedback_sound_blocking(&app_clone, SoundType::Start);
                    rm_clone.apply_mute();
                });
            } else {
                debug!("Failed to start recording");
            }
        }

        if recording_started {
            // Dynamically register the cancel shortcut in a separate task to avoid deadlock
            shortcut::register_cancel_shortcut(app);

            if is_dual_stream {
                // Dual-stream mode: mic + system audio via two parallel Mistral sessions
                #[cfg(target_os = "macos")]
                {
                    // Ensure system audio output is unmuted — we need to capture it
                    rm.ensure_unmuted();

                    if let Some(mic_rx) = rm.enable_audio_streaming() {
                        // Start system audio capture
                        let mut sys_capture =
                            crate::audio_toolkit::system_audio::SystemAudioCapture::new();
                        match sys_capture.start() {
                            Ok((sys_rx_raw, sample_rate)) => {
                                // Create resampler bridge: 48kHz (or native) -> 16kHz
                                let (sys_tx_16k, sys_rx_16k) = std::sync::mpsc::channel();
                                std::thread::spawn(move || {
                                    use crate::audio_toolkit::audio::FrameResampler;
                                    let mut resampler = FrameResampler::new(
                                        sample_rate as usize,
                                        16000,
                                        std::time::Duration::from_millis(30),
                                    );
                                    while let Ok(samples) = sys_rx_raw.recv() {
                                        resampler.push(&samples, |frame| {
                                            let _ = sys_tx_16k.send(frame.to_vec());
                                        });
                                    }
                                    // Flush remaining
                                    resampler.finish(|frame| {
                                        let _ = sys_tx_16k.send(frame.to_vec());
                                    });
                                });

                                // Create two silent Mistral sessions
                                let mic_session = MistralRealtimeSession::start_silent(
                                    app.clone(),
                                    settings.mistral_api_key.clone(),
                                    mic_rx,
                                );
                                let sys_session = MistralRealtimeSession::start_silent(
                                    app.clone(),
                                    settings.mistral_api_key.clone(),
                                    sys_rx_16k,
                                );

                                // Create coordinator
                                let coordinator = DualStreamCoordinator::start(
                                    app.clone(),
                                    mic_session.get_text_ref(),
                                    sys_session.get_text_ref(),
                                );

                                // Start streaming translator on coordinator's merged text
                                if settings.streaming_translation_enabled {
                                    let text_ref = coordinator.get_text_ref();
                                    let translator = StreamingTranslator::start(
                                        app.clone(),
                                        settings.mistral_api_key.clone(),
                                        text_ref,
                                    );
                                    let translator_state = app.state::<StreamingTranslatorState>();
                                    *translator_state.0.lock().unwrap() = Some(translator);
                                    info!("Streaming translator started (dual-stream)");
                                }

                                // Store mic session in MistralSessionState (reuse existing state)
                                let session_state = app.state::<MistralSessionState>();
                                *session_state.0.lock().unwrap() = Some(mic_session);

                                // Store system session + coordinator + capture in DualStreamState
                                let dual_state = app.state::<DualStreamState>();
                                *dual_state.0.lock().unwrap() =
                                    Some((sys_session, coordinator, sys_capture));

                                info!("Dual-stream mode started (mic + system audio)");
                            }
                            Err(e) => {
                                error!("Failed to start system audio capture: {}", e);
                                // Fall back to mic-only Mistral session
                                let session = MistralRealtimeSession::start(
                                    app.clone(),
                                    settings.mistral_api_key.clone(),
                                    mic_rx,
                                );
                                if settings.streaming_translation_enabled {
                                    let text_ref = session.get_text_ref();
                                    let translator = StreamingTranslator::start(
                                        app.clone(),
                                        settings.mistral_api_key.clone(),
                                        text_ref,
                                    );
                                    let translator_state = app.state::<StreamingTranslatorState>();
                                    *translator_state.0.lock().unwrap() = Some(translator);
                                }
                                let session_state = app.state::<MistralSessionState>();
                                *session_state.0.lock().unwrap() = Some(session);
                                warn!("Fell back to mic-only Mistral session");
                            }
                        }
                    } else {
                        error!("Failed to enable audio streaming for dual-stream mode");
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    error!("Dual-stream mode is only supported on macOS");
                }
            } else if is_mistral_api {
                // Mistral API streaming mode (single stream)
                if let Some(audio_rx) = rm.enable_audio_streaming() {
                    let session = MistralRealtimeSession::start(
                        app.clone(),
                        settings.mistral_api_key.clone(),
                        audio_rx,
                    );

                    // Start streaming translator if enabled
                    if settings.streaming_translation_enabled {
                        let text_ref = session.get_text_ref();
                        let translator = StreamingTranslator::start(
                            app.clone(),
                            settings.mistral_api_key.clone(),
                            text_ref,
                        );
                        let translator_state = app.state::<StreamingTranslatorState>();
                        *translator_state.0.lock().unwrap() = Some(translator);
                        info!("Streaming translator started");
                    }

                    let session_state = app.state::<MistralSessionState>();
                    *session_state.0.lock().unwrap() = Some(session);
                    info!("Mistral realtime streaming session started");
                } else {
                    error!("Failed to enable audio streaming for Mistral API");
                }
            } else if supports_incremental {
                // VAD-segmented incremental streaming for fast local models
                if let Some(audio_rx) = rm.enable_audio_streaming() {
                    let vad_path = app
                        .path()
                        .resolve(
                            "resources/models/silero_vad_v4.onnx",
                            tauri::path::BaseDirectory::Resource,
                        )
                        .map(|p| p.to_string_lossy().to_string());

                    match vad_path {
                        Ok(path) => {
                            let session = IncrementalTranscriptionSession::start(
                                app.clone(),
                                Arc::clone(&tm),
                                audio_rx,
                                path,
                            );
                            let state = app.state::<IncrementalSessionState>();
                            *state.0.lock().unwrap() = Some(session);
                            info!("Incremental transcription session started");
                        }
                        Err(e) => {
                            error!(
                                "Failed to resolve VAD model path for incremental session: {}",
                                e
                            );
                            rm.disable_audio_streaming();
                        }
                    }
                } else {
                    error!("Failed to enable audio streaming for incremental transcription");
                }
            }
        }

        debug!(
            "TranscribeAction::start completed in {:?}",
            start_time.elapsed()
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, _shortcut_str: &str) {
        // Unregister the cancel shortcut when transcription stops
        shortcut::unregister_cancel_shortcut(app);

        let stop_time = Instant::now();
        debug!("TranscribeAction::stop called for binding: {}", binding_id);

        let ah = app.clone();
        let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
        let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
        let hm = Arc::clone(&app.state::<Arc<HistoryManager>>());

        // Check if we're in Mistral streaming mode
        let settings = get_settings(app);
        let is_mistral_api = settings.selected_model == "mistral-voxtral-realtime"
            && !settings.mistral_api_key.is_empty();

        // Check if we have an active incremental session
        let incremental_session = {
            let state = app.state::<IncrementalSessionState>();
            let session = state.0.lock().unwrap().take();
            session
        };
        let has_incremental = incremental_session.is_some();

        if !is_mistral_api && !has_incremental {
            change_tray_icon(app, TrayIconState::Transcribing);
            show_transcribing_overlay(app);
        }

        // Unmute before playing audio feedback so the stop sound is audible
        rm.remove_mute();

        // Play audio feedback for recording stop
        play_feedback_sound(app, SoundType::Stop);

        let binding_id = binding_id.to_string(); // Clone binding_id for the async task
        let post_process = self.post_process;

        tauri::async_runtime::spawn(async move {
            let binding_id = binding_id.clone(); // Clone for the inner async task
            debug!(
                "Starting async transcription task for binding: {}",
                binding_id
            );

            if let Some(mut session) = incremental_session {
                // Incremental (VAD-segmented) streaming mode: stop session, do final full-buffer transcription
                rm.disable_audio_streaming();
                session.stop(&tm);

                // Show "Transcribing..." briefly for the final full-buffer pass
                change_tray_icon(&ah, TrayIconState::Transcribing);
                show_transcribing_overlay(&ah);

                let stop_recording_time = Instant::now();
                if let Some(samples) = rm.stop_recording(&binding_id) {
                    debug!(
                        "Recording stopped and samples retrieved in {:?}, sample count: {}",
                        stop_recording_time.elapsed(),
                        samples.len()
                    );

                    let transcription_time = Instant::now();
                    let samples_clone = samples.clone();

                    // Final full-buffer transcription for best quality
                    let transcription_result = match tm.transcribe(samples) {
                        Ok(transcription) if !transcription.is_empty() => {
                            debug!(
                                "Final full-buffer transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                transcription
                            );
                            transcription
                        }
                        Ok(_) => {
                            // Empty transcription — fall back to accumulated segment preview
                            let preview = session.get_accumulated_text();
                            debug!(
                                "Final transcription empty, falling back to segment preview: '{}'",
                                preview
                            );
                            preview
                        }
                        Err(e) => {
                            warn!(
                                "Final transcription failed: {}, falling back to segment preview",
                                e
                            );
                            session.get_accumulated_text()
                        }
                    };

                    if !transcription_result.is_empty() {
                        let settings = get_settings(&ah);
                        let mut final_text = transcription_result.clone();
                        let mut post_processed_text: Option<String> = None;
                        let mut post_process_prompt: Option<String> = None;

                        // Chinese variant conversion
                        if let Some(converted_text) =
                            maybe_convert_chinese_variant(&settings, &transcription_result).await
                        {
                            final_text = converted_text;
                        }

                        // LLM post-processing
                        if post_process {
                            show_processing_overlay(&ah);
                        }
                        let processed = if post_process {
                            post_process_transcription(&settings, &final_text).await
                        } else {
                            None
                        };
                        if let Some(processed_text) = processed {
                            post_processed_text = Some(processed_text.clone());
                            final_text = processed_text;

                            if let Some(prompt_id) = &settings.post_process_selected_prompt_id {
                                if let Some(prompt) = settings
                                    .post_process_prompts
                                    .iter()
                                    .find(|p| &p.id == prompt_id)
                                {
                                    post_process_prompt = Some(prompt.prompt.clone());
                                }
                            }
                        } else if final_text != transcription_result {
                            post_processed_text = Some(final_text.clone());
                        }

                        // Save to history
                        let hm_clone = Arc::clone(&hm);
                        let transcription_for_history = transcription_result.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = hm_clone
                                .save_transcription(
                                    samples_clone,
                                    transcription_for_history,
                                    post_processed_text,
                                    post_process_prompt,
                                )
                                .await
                            {
                                error!("Failed to save transcription to history: {}", e);
                            }
                        });

                        // Paste the final text
                        let ah_clone = ah.clone();
                        let paste_time = Instant::now();
                        ah.run_on_main_thread(move || {
                            match utils::paste(final_text, ah_clone.clone()) {
                                Ok(()) => {
                                    debug!("Text pasted successfully in {:?}", paste_time.elapsed())
                                }
                                Err(e) => error!("Failed to paste transcription: {}", e),
                            }
                            utils::hide_recording_overlay(&ah_clone);
                            change_tray_icon(&ah_clone, TrayIconState::Idle);
                        })
                        .unwrap_or_else(|e| {
                            error!("Failed to run paste on main thread: {:?}", e);
                            utils::hide_recording_overlay(&ah);
                            change_tray_icon(&ah, TrayIconState::Idle);
                        });
                    } else {
                        utils::hide_recording_overlay(&ah);
                        change_tray_icon(&ah, TrayIconState::Idle);
                    }
                } else {
                    debug!("No samples retrieved from recording stop");
                    utils::hide_recording_overlay(&ah);
                    change_tray_icon(&ah, TrayIconState::Idle);
                }
            } else if is_mistral_api {
                // Check for dual-stream state first
                let dual_state_data = {
                    let ds = ah.state::<DualStreamState>();
                    let data = ds.0.lock().unwrap().take();
                    data
                };

                #[cfg(target_os = "macos")]
                let is_dual = dual_state_data.is_some();
                #[cfg(not(target_os = "macos"))]
                let is_dual = false;

                if is_dual {
                    // Dual-stream stop flow
                    #[cfg(target_os = "macos")]
                    {
                        rm.disable_audio_streaming();

                        // Stop the periodic translator
                        {
                            let ts = ah.state::<StreamingTranslatorState>();
                            let mut guard = ts.0.lock().unwrap();
                            if let Some(mut translator) = guard.take() {
                                translator.stop();
                            }
                        }

                        let (mut sys_session, mut coordinator, mut sys_capture) =
                            dual_state_data.unwrap();

                        // Stop system audio capture
                        sys_capture.stop();

                        // Stop coordinator and get segments
                        let segments = coordinator.stop();

                        // Stop both Mistral sessions
                        let mic_session = {
                            let session_state = ah.state::<MistralSessionState>();
                            let s = session_state.0.lock().unwrap().take();
                            s
                        };
                        let mic_final = if let Some(mut session) = mic_session {
                            session.stop().await
                        } else {
                            String::new()
                        };
                        let sys_final = sys_session.stop().await;

                        // Stop recording and get samples (for history)
                        let samples = rm.stop_recording(&binding_id).unwrap_or_default();

                        // Build chronological merged final text
                        let merged_text = DualStreamCoordinator::build_final_text(
                            &segments, &mic_final, &sys_final,
                        );

                        if !merged_text.is_empty() {
                            let settings = get_settings(&ah);
                            let mut text = merged_text.clone();

                            // Final high-quality translation if streaming translation is enabled
                            if settings.streaming_translation_enabled {
                                let is_english =
                                    streaming_translator::is_likely_english(&merged_text);

                                if is_english {
                                    info!("Final text detected as English, skipping translation");
                                } else if let Some(translated) =
                                    streaming_translator::translate_with_mistral(
                                        &settings.mistral_api_key,
                                        "mistral-medium-latest",
                                        &merged_text,
                                    )
                                    .await
                                {
                                    info!(
                                        "Final dual-stream translation completed ({} -> {} chars)",
                                        merged_text.len(),
                                        translated.len()
                                    );
                                    text = translated;
                                } else {
                                    warn!("Final translation failed, using original text");
                                }
                            }

                            // Apply LLM post-processing if applicable
                            let mut post_processed_text: Option<String> = None;
                            let mut post_process_prompt_text: Option<String> = None;

                            if post_process {
                                show_processing_overlay(&ah);
                                if let Some(processed) =
                                    post_process_transcription(&settings, &text).await
                                {
                                    post_processed_text = Some(processed.clone());
                                    text = processed;

                                    if let Some(prompt_id) =
                                        &settings.post_process_selected_prompt_id
                                    {
                                        if let Some(prompt) = settings
                                            .post_process_prompts
                                            .iter()
                                            .find(|p| &p.id == prompt_id)
                                        {
                                            post_process_prompt_text =
                                                Some(prompt.prompt.clone());
                                        }
                                    }
                                }
                            }

                            if post_processed_text.is_none() && text != merged_text {
                                post_processed_text = Some(text.clone());
                            }

                            // Save to history
                            let hm_clone = Arc::clone(&hm);
                            let transcription_for_history =
                                if settings.streaming_translation_enabled && text != merged_text {
                                    format!("{}\n\n---\n\n{}", merged_text, text)
                                } else {
                                    merged_text.clone()
                                };
                            let pp_text = post_processed_text.clone();
                            let pp_prompt = post_process_prompt_text.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Err(e) = hm_clone
                                    .save_transcription(
                                        samples,
                                        transcription_for_history,
                                        pp_text,
                                        pp_prompt,
                                    )
                                    .await
                                {
                                    error!("Failed to save transcription to history: {}", e);
                                }
                            });

                            // Paste the final text
                            let ah_clone = ah.clone();
                            let paste_time = Instant::now();
                            ah.run_on_main_thread(move || {
                                match utils::paste(text, ah_clone.clone()) {
                                    Ok(()) => debug!(
                                        "Text pasted successfully in {:?}",
                                        paste_time.elapsed()
                                    ),
                                    Err(e) => error!("Failed to paste transcription: {}", e),
                                }
                                utils::hide_recording_overlay(&ah_clone);
                                change_tray_icon(&ah_clone, TrayIconState::Idle);
                            })
                            .unwrap_or_else(|e| {
                                error!("Failed to run paste on main thread: {:?}", e);
                                utils::hide_recording_overlay(&ah);
                                change_tray_icon(&ah, TrayIconState::Idle);
                            });
                        } else {
                            utils::hide_recording_overlay(&ah);
                            change_tray_icon(&ah, TrayIconState::Idle);
                        }
                    }
                } else {
                    // Single-stream Mistral stop flow
                    rm.disable_audio_streaming();

                    // Stop the periodic translator
                    {
                        let ts = ah.state::<StreamingTranslatorState>();
                        let mut guard = ts.0.lock().unwrap();
                        if let Some(mut translator) = guard.take() {
                            translator.stop();
                        }
                    }

                    let session = {
                        let session_state = ah.state::<MistralSessionState>();
                        let session = session_state.0.lock().unwrap().take();
                        session
                    };
                    let final_text = if let Some(mut session) = session {
                        session.stop().await
                    } else {
                        String::new()
                    };

                    // Stop recording and get samples (for history)
                    let samples = rm.stop_recording(&binding_id).unwrap_or_default();

                    if !final_text.is_empty() {
                        let settings = get_settings(&ah);
                        let mut text = final_text.clone();

                        // Final high-quality translation if streaming translation is enabled
                        if settings.streaming_translation_enabled {
                            let is_english =
                                streaming_translator::is_likely_english(&final_text);

                            if is_english {
                                info!("Final text detected as English, skipping translation");
                            } else if let Some(translated) =
                                streaming_translator::translate_with_mistral(
                                    &settings.mistral_api_key,
                                    "mistral-medium-latest",
                                    &final_text,
                                )
                                .await
                            {
                                info!(
                                    "Final translation completed ({} chars -> {} chars)",
                                    final_text.len(),
                                    translated.len()
                                );
                                text = translated;
                            } else {
                                warn!("Final translation failed, using original text");
                            }
                        }

                        // Apply Chinese variant conversion (only if not translated)
                        if !settings.streaming_translation_enabled {
                            if let Some(converted) =
                                maybe_convert_chinese_variant(&settings, &text).await
                            {
                                text = converted;
                            }
                        }

                        // Apply LLM post-processing if applicable
                        let mut post_processed_text: Option<String> = None;
                        let mut post_process_prompt_text: Option<String> = None;

                        if post_process {
                            show_processing_overlay(&ah);
                            if let Some(processed) =
                                post_process_transcription(&settings, &text).await
                            {
                                post_processed_text = Some(processed.clone());
                                text = processed;

                                if let Some(prompt_id) =
                                    &settings.post_process_selected_prompt_id
                                {
                                    if let Some(prompt) = settings
                                        .post_process_prompts
                                        .iter()
                                        .find(|p| &p.id == prompt_id)
                                    {
                                        post_process_prompt_text = Some(prompt.prompt.clone());
                                    }
                                }
                            }
                        }

                        // Always store the final text as post-processed if it differs
                        // from the original (e.g. translation was applied)
                        if post_processed_text.is_none() && text != final_text {
                            post_processed_text = Some(text.clone());
                        }

                        // Save to history — concatenate original + translation
                        let hm_clone = Arc::clone(&hm);
                        let transcription_for_history =
                            if settings.streaming_translation_enabled && text != final_text {
                                format!("{}\n\n---\n\n{}", final_text, text)
                            } else {
                                final_text.clone()
                            };
                        let pp_text = post_processed_text.clone();
                        let pp_prompt = post_process_prompt_text.clone();
                        tauri::async_runtime::spawn(async move {
                            if let Err(e) = hm_clone
                                .save_transcription(
                                    samples,
                                    transcription_for_history,
                                    pp_text,
                                    pp_prompt,
                                )
                                .await
                            {
                                error!("Failed to save transcription to history: {}", e);
                            }
                        });

                        // Paste the final text
                        let ah_clone = ah.clone();
                        let paste_time = Instant::now();
                        ah.run_on_main_thread(move || {
                            match utils::paste(text, ah_clone.clone()) {
                                Ok(()) => {
                                    debug!(
                                        "Text pasted successfully in {:?}",
                                        paste_time.elapsed()
                                    )
                                }
                                Err(e) => error!("Failed to paste transcription: {}", e),
                            }
                            utils::hide_recording_overlay(&ah_clone);
                            change_tray_icon(&ah_clone, TrayIconState::Idle);
                        })
                        .unwrap_or_else(|e| {
                            error!("Failed to run paste on main thread: {:?}", e);
                            utils::hide_recording_overlay(&ah);
                            change_tray_icon(&ah, TrayIconState::Idle);
                        });
                    } else {
                        // Stop recording even if no text
                        utils::hide_recording_overlay(&ah);
                        change_tray_icon(&ah, TrayIconState::Idle);
                    }
                }
            } else {
                // Standard batch transcription mode
                let stop_recording_time = Instant::now();
                if let Some(samples) = rm.stop_recording(&binding_id) {
                    debug!(
                        "Recording stopped and samples retrieved in {:?}, sample count: {}",
                        stop_recording_time.elapsed(),
                        samples.len()
                    );

                    let transcription_time = Instant::now();
                    let samples_clone = samples.clone(); // Clone for history saving
                    match tm.transcribe(samples) {
                        Ok(transcription) => {
                            debug!(
                                "Transcription completed in {:?}: '{}'",
                                transcription_time.elapsed(),
                                transcription
                            );
                            if !transcription.is_empty() {
                                let settings = get_settings(&ah);
                                let mut final_text = transcription.clone();
                                let mut post_processed_text: Option<String> = None;
                                let mut post_process_prompt: Option<String> = None;

                                // First, check if Chinese variant conversion is needed
                                if let Some(converted_text) =
                                    maybe_convert_chinese_variant(&settings, &transcription).await
                                {
                                    final_text = converted_text;
                                }

                                // Then apply LLM post-processing if this is the post-process hotkey
                                // Uses final_text which may already have Chinese conversion applied
                                if post_process {
                                    show_processing_overlay(&ah);
                                }
                                let processed = if post_process {
                                    post_process_transcription(&settings, &final_text).await
                                } else {
                                    None
                                };
                                if let Some(processed_text) = processed {
                                    post_processed_text = Some(processed_text.clone());
                                    final_text = processed_text;

                                    // Get the prompt that was used
                                    if let Some(prompt_id) =
                                        &settings.post_process_selected_prompt_id
                                    {
                                        if let Some(prompt) = settings
                                            .post_process_prompts
                                            .iter()
                                            .find(|p| &p.id == prompt_id)
                                        {
                                            post_process_prompt = Some(prompt.prompt.clone());
                                        }
                                    }
                                } else if final_text != transcription {
                                    // Chinese conversion was applied but no LLM post-processing
                                    post_processed_text = Some(final_text.clone());
                                }

                                // Save to history with post-processed text and prompt
                                let hm_clone = Arc::clone(&hm);
                                let transcription_for_history = transcription.clone();
                                tauri::async_runtime::spawn(async move {
                                    if let Err(e) = hm_clone
                                        .save_transcription(
                                            samples_clone,
                                            transcription_for_history,
                                            post_processed_text,
                                            post_process_prompt,
                                        )
                                        .await
                                    {
                                        error!("Failed to save transcription to history: {}", e);
                                    }
                                });

                                // Paste the final text (either processed or original)
                                let ah_clone = ah.clone();
                                let paste_time = Instant::now();
                                ah.run_on_main_thread(move || {
                                    match utils::paste(final_text, ah_clone.clone()) {
                                        Ok(()) => debug!(
                                            "Text pasted successfully in {:?}",
                                            paste_time.elapsed()
                                        ),
                                        Err(e) => {
                                            error!("Failed to paste transcription: {}", e)
                                        }
                                    }
                                    // Hide the overlay after transcription is complete
                                    utils::hide_recording_overlay(&ah_clone);
                                    change_tray_icon(&ah_clone, TrayIconState::Idle);
                                })
                                .unwrap_or_else(|e| {
                                    error!("Failed to run paste on main thread: {:?}", e);
                                    utils::hide_recording_overlay(&ah);
                                    change_tray_icon(&ah, TrayIconState::Idle);
                                });
                            } else {
                                utils::hide_recording_overlay(&ah);
                                change_tray_icon(&ah, TrayIconState::Idle);
                            }
                        }
                        Err(err) => {
                            debug!("Global Shortcut Transcription error: {}", err);
                            utils::hide_recording_overlay(&ah);
                            change_tray_icon(&ah, TrayIconState::Idle);
                        }
                    }
                } else {
                    debug!("No samples retrieved from recording stop");
                    utils::hide_recording_overlay(&ah);
                    change_tray_icon(&ah, TrayIconState::Idle);
                }
            }

            // Clear toggle state now that transcription is complete
            if let Ok(mut states) = ah.state::<ManagedToggleState>().lock() {
                states.active_toggles.insert(binding_id, false);
            }
        });

        debug!(
            "TranscribeAction::stop completed in {:?}",
            stop_time.elapsed()
        );
    }
}

// Cancel Action
struct CancelAction;

impl ShortcutAction for CancelAction {
    fn start(&self, app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        utils::cancel_current_operation(app);
    }

    fn stop(&self, _app: &AppHandle, _binding_id: &str, _shortcut_str: &str) {
        // Nothing to do on stop for cancel
    }
}

// Test Action
struct TestAction;

impl ShortcutAction for TestAction {
    fn start(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Started - {} (App: {})", // Changed "Pressed" to "Started" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }

    fn stop(&self, app: &AppHandle, binding_id: &str, shortcut_str: &str) {
        log::info!(
            "Shortcut ID '{}': Stopped - {} (App: {})", // Changed "Released" to "Stopped" for consistency
            binding_id,
            shortcut_str,
            app.package_info().name
        );
    }
}

// Static Action Map
pub static ACTION_MAP: Lazy<HashMap<String, Arc<dyn ShortcutAction>>> = Lazy::new(|| {
    let mut map = HashMap::new();
    map.insert(
        "transcribe".to_string(),
        Arc::new(TranscribeAction {
            post_process: false,
        }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "transcribe_with_post_process".to_string(),
        Arc::new(TranscribeAction { post_process: true }) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "cancel".to_string(),
        Arc::new(CancelAction) as Arc<dyn ShortcutAction>,
    );
    map.insert(
        "test".to_string(),
        Arc::new(TestAction) as Arc<dyn ShortcutAction>,
    );
    map
});
