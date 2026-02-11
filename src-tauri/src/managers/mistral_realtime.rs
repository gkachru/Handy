use base64::Engine as _;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use tauri::{AppHandle, Emitter};
use tokio::sync::Notify;

/// Manages a Mistral API real-time WebSocket transcription session.
///
/// Audio flows: AudioRecorder (16kHz f32 frames) → mpsc::Receiver → this module
/// → converts to PCM s16le → base64 → sends over WebSocket
/// ← receives transcription deltas → emits events to frontend
pub struct MistralRealtimeSession {
    stop_signal: Arc<AtomicBool>,
    done_notify: Arc<Notify>,
    final_text: Arc<Mutex<String>>,
    task_handle: Option<tauri::async_runtime::JoinHandle<()>>,
}

#[derive(Serialize)]
struct AudioAppendMessage {
    #[serde(rename = "type")]
    msg_type: String,
    audio: String,
}

#[derive(Serialize)]
struct AudioEndMessage {
    #[serde(rename = "type")]
    msg_type: String,
}

#[derive(Deserialize, Debug)]
struct ServerMessage {
    #[serde(rename = "type")]
    msg_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

fn encode_audio_chunk(samples: &[f32]) -> String {
    let pcm: Vec<u8> = samples
        .iter()
        .flat_map(|&s| {
            let i = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            i.to_le_bytes()
        })
        .collect();
    base64::engine::general_purpose::STANDARD.encode(&pcm)
}

fn emit_status(app: &AppHandle, status: &str) {
    let _ = app.emit("streaming-status", status.to_string());
}

fn emit_text(app: &AppHandle, text: &str) {
    let _ = app.emit("streaming-transcription-update", text.to_string());
}

impl MistralRealtimeSession {
    /// Start a new Mistral realtime streaming session.
    ///
    /// Spawns a tokio task that connects to the Mistral WebSocket API,
    /// reads audio from `audio_rx`, encodes and sends it, and emits
    /// transcription events to the frontend.
    pub fn start(
        app_handle: AppHandle,
        api_key: String,
        audio_rx: mpsc::Receiver<Vec<f32>>,
    ) -> Self {
        let stop_signal = Arc::new(AtomicBool::new(false));
        let done_notify = Arc::new(Notify::new());
        let final_text = Arc::new(Mutex::new(String::new()));

        let stop = stop_signal.clone();
        let done = done_notify.clone();
        let text = final_text.clone();

        let audio_rx = Arc::new(Mutex::new(audio_rx));

        let handle = tauri::async_runtime::spawn(async move {
            Self::run_session(app_handle, api_key, audio_rx, stop, done.clone(), text).await;
            done.notify_waiters();
        });

        Self {
            stop_signal,
            done_notify,
            final_text,
            task_handle: Some(handle),
        }
    }

    /// Get a shared reference to the accumulated transcription text.
    pub fn get_text_ref(&self) -> Arc<Mutex<String>> {
        self.final_text.clone()
    }

    /// Signal the session to stop and wait for the final transcription text.
    pub async fn stop(&mut self) -> String {
        self.stop_signal.store(true, Ordering::Relaxed);

        // Wait for session to complete with timeout
        let done = self.done_notify.clone();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), done.notified()).await;

        if let Some(handle) = self.task_handle.take() {
            let _ = handle.await;
        }

        let text = self.final_text.lock().unwrap();
        text.clone()
    }

    async fn run_session(
        app: AppHandle,
        api_key: String,
        audio_rx: Arc<Mutex<mpsc::Receiver<Vec<f32>>>>,
        stop_signal: Arc<AtomicBool>,
        _done_notify: Arc<Notify>,
        final_text: Arc<Mutex<String>>,
    ) {
        // Buffer all audio for reconnection
        let audio_buffer: Arc<Mutex<Vec<Vec<f32>>>> = Arc::new(Mutex::new(Vec::new()));
        let mut retry_count = 0;
        let max_retries = 10;

        loop {
            if stop_signal.load(Ordering::Relaxed) {
                break;
            }

            emit_status(
                &app,
                if retry_count == 0 {
                    "connecting"
                } else {
                    "reconnecting"
                },
            );

            match Self::connect_and_stream(
                &app,
                &api_key,
                &audio_rx,
                &stop_signal,
                &final_text,
                &audio_buffer,
            )
            .await
            {
                Ok(()) => {
                    // Clean exit (stop was signaled or audio_end was sent)
                    break;
                }
                Err(e) => {
                    retry_count += 1;
                    if retry_count > max_retries {
                        error!(
                            "Mistral realtime: max retries ({}) exceeded, giving up: {}",
                            max_retries, e
                        );
                        emit_status(&app, "error");
                        break;
                    }
                    warn!(
                        "Mistral realtime: connection error (retry {}/{}): {}",
                        retry_count, max_retries, e
                    );
                    // Exponential backoff: 100ms, 200ms, 400ms, ... capped at 5s
                    let delay = std::cmp::min(100 * (1 << (retry_count - 1)), 5000);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
            }
        }
    }

    async fn connect_and_stream(
        app: &AppHandle,
        api_key: &str,
        audio_rx: &Arc<Mutex<mpsc::Receiver<Vec<f32>>>>,
        stop_signal: &Arc<AtomicBool>,
        final_text: &Arc<Mutex<String>>,
        audio_buffer: &Arc<Mutex<Vec<Vec<f32>>>>,
    ) -> Result<(), String> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite;

        let url = "wss://api.mistral.ai/v1/audio/transcriptions/realtime?model=voxtral-mini-transcribe-realtime-2602";

        let request = tungstenite::http::Request::builder()
            .uri(url)
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Host", "api.mistral.ai")
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header(
                "Sec-WebSocket-Key",
                tungstenite::handshake::client::generate_key(),
            )
            .body(())
            .map_err(|e| format!("Failed to build request: {}", e))?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| format!("WebSocket connection failed: {}", e))?;

        let (mut ws_sink, mut ws_source) = ws_stream.split();

        // Wait for session.created
        let mut session_ready = false;
        while let Some(msg) = ws_source.next().await {
            match msg {
                Ok(tungstenite::Message::Text(text)) => {
                    if let Ok(parsed) = serde_json::from_str::<ServerMessage>(&text) {
                        if parsed.msg_type == "session.created" {
                            session_ready = true;
                            debug!("Mistral realtime: session created");
                            break;
                        }
                    }
                }
                Err(e) => return Err(format!("Error waiting for session.created: {}", e)),
                _ => {}
            }
        }

        if !session_ready {
            return Err("Connection closed before session.created".to_string());
        }

        emit_status(app, "connected");

        // Send 2s of silence warmup
        let silence_samples = vec![0.0f32; 480]; // 30ms at 16kHz
        let silence_chunks = 67; // ~2s worth of 30ms chunks
        for _ in 0..silence_chunks {
            let encoded = encode_audio_chunk(&silence_samples);
            let msg = AudioAppendMessage {
                msg_type: "input_audio.append".to_string(),
                audio: encoded,
            };
            let json = serde_json::to_string(&msg).unwrap();
            ws_sink
                .send(tungstenite::Message::Text(json.into()))
                .await
                .map_err(|e| format!("Failed to send warmup: {}", e))?;
        }

        // Replay buffered audio from previous connection attempts
        let buffered_chunks = {
            let buffer = audio_buffer.lock().unwrap();
            if buffer.is_empty() {
                Vec::new()
            } else {
                debug!(
                    "Mistral realtime: replaying {} buffered chunks",
                    buffer.len()
                );
                // Reset accumulated text since server will re-transcribe everything
                *final_text.lock().unwrap() = String::new();
                buffer.clone()
            }
        };
        for chunk in &buffered_chunks {
            let encoded = encode_audio_chunk(chunk);
            let msg = AudioAppendMessage {
                msg_type: "input_audio.append".to_string(),
                audio: encoded,
            };
            let json = serde_json::to_string(&msg).unwrap();
            if let Err(e) = ws_sink.send(tungstenite::Message::Text(json.into())).await {
                return Err(format!("Failed to replay buffered audio: {}", e));
            }
        }

        // Main streaming loop: read from audio_rx and send to WebSocket,
        // while also reading responses from the WebSocket
        let accumulated_text = final_text.clone();
        let stop = stop_signal.clone();
        let app_for_recv = app.clone();
        let buffer_for_send = audio_buffer.clone();

        // Spawn receiver task
        let recv_accumulated = accumulated_text.clone();
        let recv_stop = stop.clone();
        let recv_handle = tokio::spawn(async move {
            while let Some(msg) = ws_source.next().await {
                if recv_stop.load(Ordering::Relaxed) {
                    break;
                }
                match msg {
                    Ok(tungstenite::Message::Text(text)) => {
                        if let Ok(parsed) = serde_json::from_str::<ServerMessage>(&text) {
                            match parsed.msg_type.as_str() {
                                "transcription.text.delta" => {
                                    if let Some(delta) = &parsed.text {
                                        let mut acc = recv_accumulated.lock().unwrap();
                                        acc.push_str(delta);
                                        let current = acc.clone();
                                        drop(acc);
                                        emit_text(&app_for_recv, &current);
                                    }
                                }
                                "transcription.done" => {
                                    debug!("Mistral realtime: transcription.done received");
                                    break;
                                }
                                "error" => {
                                    error!(
                                        "Mistral realtime error: {:?}",
                                        parsed.error.unwrap_or_default()
                                    );
                                    break;
                                }
                                _ => {
                                    debug!("Mistral realtime: received {}", parsed.msg_type);
                                }
                            }
                        }
                    }
                    Ok(tungstenite::Message::Close(_)) => {
                        debug!("Mistral realtime: WebSocket closed by server");
                        break;
                    }
                    Err(e) => {
                        warn!("Mistral realtime: WebSocket receive error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        // Sender loop: read from audio_rx, send to WebSocket
        loop {
            if stop.load(Ordering::Relaxed) {
                // Send input_audio.end
                let end_msg = AudioEndMessage {
                    msg_type: "input_audio.end".to_string(),
                };
                let json = serde_json::to_string(&end_msg).unwrap();
                let _ = ws_sink.send(tungstenite::Message::Text(json.into())).await;

                // Wait for receiver to get transcription.done
                let _ = tokio::time::timeout(std::time::Duration::from_secs(3), recv_handle).await;
                return Ok(());
            }

            // Non-blocking receive from audio channel
            // Scope the MutexGuard so it's dropped before any .await
            let recv_result = audio_rx.lock().unwrap().try_recv();
            match recv_result {
                Ok(samples) => {
                    // Buffer for potential reconnection
                    buffer_for_send.lock().unwrap().push(samples.clone());

                    let encoded = encode_audio_chunk(&samples);
                    let msg = AudioAppendMessage {
                        msg_type: "input_audio.append".to_string(),
                        audio: encoded,
                    };
                    let json = serde_json::to_string(&msg).unwrap();
                    if let Err(e) = ws_sink.send(tungstenite::Message::Text(json.into())).await {
                        // Check if receiver task ended (indicates server-side disconnect)
                        if recv_handle.is_finished() {
                            return Err(format!("WebSocket disconnected: {}", e));
                        }
                        return Err(format!("Failed to send audio: {}", e));
                    }
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // No audio available, brief sleep to avoid busy-waiting
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Audio source disconnected (recording stopped)
                    info!("Mistral realtime: audio channel disconnected");
                    let end_msg = AudioEndMessage {
                        msg_type: "input_audio.end".to_string(),
                    };
                    let json = serde_json::to_string(&end_msg).unwrap();
                    let _ = ws_sink.send(tungstenite::Message::Text(json.into())).await;
                    let _ =
                        tokio::time::timeout(std::time::Duration::from_secs(3), recv_handle).await;
                    return Ok(());
                }
            }
        }
    }
}
