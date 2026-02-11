use log::{debug, error, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// Periodically translates accumulated transcription text using Mistral's chat completion API.
///
/// Runs a background loop that checks for new text every ~3 seconds,
/// translates it with `ministral-14b-latest`, and emits translation events to the frontend.
pub struct StreamingTranslator {
    stop_signal: Arc<AtomicBool>,
    task_handle: Option<tauri::async_runtime::JoinHandle<()>>,
    final_translation: Arc<Mutex<String>>,
}

impl StreamingTranslator {
    /// Start the periodic translation loop.
    ///
    /// `source_text` should be a shared reference to the accumulated transcription text
    /// (e.g. `MistralRealtimeSession::final_text`).
    pub fn start(app_handle: AppHandle, api_key: String, source_text: Arc<Mutex<String>>) -> Self {
        let stop_signal = Arc::new(AtomicBool::new(false));
        let final_translation = Arc::new(Mutex::new(String::new()));

        let stop = stop_signal.clone();
        let translation = final_translation.clone();

        let handle = tauri::async_runtime::spawn(async move {
            Self::translation_loop(app_handle, api_key, source_text, stop, translation).await;
        });

        Self {
            stop_signal,
            task_handle: Some(handle),
            final_translation,
        }
    }

    /// Signal the translator to stop.
    pub fn stop(&mut self) {
        self.stop_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = self.task_handle.take() {
            handle.abort();
        }
    }

    /// Get the current translation text.
    pub fn get_translation(&self) -> String {
        self.final_translation.lock().unwrap().clone()
    }

    async fn translation_loop(
        app: AppHandle,
        api_key: String,
        source_text: Arc<Mutex<String>>,
        stop_signal: Arc<AtomicBool>,
        final_translation: Arc<Mutex<String>>,
    ) {
        let client = reqwest::Client::new();
        let mut last_translated_text = String::new();

        loop {
            if stop_signal.load(Ordering::Relaxed) {
                break;
            }

            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            if stop_signal.load(Ordering::Relaxed) {
                break;
            }

            let current_text = source_text.lock().unwrap().clone();

            // Skip if text is empty or unchanged
            if current_text.is_empty() || current_text == last_translated_text {
                continue;
            }

            last_translated_text = current_text.clone();

            match translate_with_mistral_client(
                &client,
                &api_key,
                "ministral-8b-latest",
                &current_text,
            )
            .await
            {
                Some(translated) => {
                    *final_translation.lock().unwrap() = translated.clone();
                    let _ = app.emit("streaming-translation-update", translated);
                }
                None => {
                    warn!("Streaming translation: periodic translation failed, continuing");
                }
            }
        }
    }
}

/// Translate text using Mistral's chat completions API.
///
/// Returns `None` on any error (network, parsing, empty response).
pub async fn translate_with_mistral(api_key: &str, model: &str, text: &str) -> Option<String> {
    let client = reqwest::Client::new();
    translate_with_mistral_client(&client, api_key, model, text).await
}

async fn translate_with_mistral_client(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    text: &str,
) -> Option<String> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": format!(
                    "Translate the following text to English. Output ONLY the translation, nothing else:\n\n{}",
                    text
                )
            }
        ]
    });

    let response = match client
        .post("https://api.mistral.ai/v1/chat/completions")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            error!("Streaming translation: request failed: {}", e);
            return None;
        }
    };

    if !response.status().is_success() {
        error!(
            "Streaming translation: API returned status {}",
            response.status()
        );
        return None;
    }

    let json: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(e) => {
            error!("Streaming translation: failed to parse response: {}", e);
            return None;
        }
    };

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_string();

    if content.is_empty() {
        debug!("Streaming translation: empty response from API");
        return None;
    }

    Some(content)
}
