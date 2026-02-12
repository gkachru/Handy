use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

/// Checks whether text is likely English by looking at the fraction of words
/// that are common English function/stop words. These words (e.g. "the", "is",
/// "not") are unique to English and appear frequently in any English speech.
pub fn is_likely_english(text: &str) -> bool {
    // Fast path: if text contains significant non-Latin characters, it's not English.
    // This catches Hindi (Devanagari), Chinese (CJK), Arabic, Cyrillic, etc.
    // even when some English words are mixed in.
    let non_ws_chars = text.chars().filter(|c| !c.is_whitespace()).count();
    if non_ws_chars > 0 {
        let non_latin_count = text
            .chars()
            .filter(|c| {
                !c.is_whitespace() && !c.is_ascii_alphanumeric() && !c.is_ascii_punctuation()
            })
            .count();
        if non_latin_count as f64 / non_ws_chars as f64 > 0.15 {
            return false;
        }
    }

    const ENGLISH_STOP_WORDS: &[&str] = &[
        "the", "is", "are", "was", "were", "am", "be", "been", "being",
        "have", "has", "had", "do", "does", "did",
        "this", "that", "these", "those",
        "not", "but", "and", "or", "if", "so",
        "they", "them", "their", "he", "she", "it", "we", "you", "i",
        "would", "could", "should", "will", "shall", "may", "might", "can",
        "which", "what", "where", "when", "who", "how",
        "there", "here", "very", "just", "also", "some", "any",
        "my", "your", "his", "her", "its", "our",
        "a", "an", "of", "in", "to", "for", "on", "with", "at", "by", "from",
        // Common spoken-English words unlikely in other Latin-script languages
        "hello", "now", "about", "out", "up", "because", "going", "thing",
        "know", "want", "think", "getting", "actually", "really",
    ];

    let words: Vec<&str> = text
        .split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphabetic()))
        .filter(|w| !w.is_empty())
        .collect();

    if words.len() < 3 {
        // Too few words for stop-word analysis. If the text passed the
        // non-Latin character check above, it's likely English (or at least
        // Latin-script) — skip translation to avoid unnecessary API calls.
        return true;
    }

    let english_count = words
        .iter()
        .filter(|w| {
            let lower = w.to_lowercase();
            if ENGLISH_STOP_WORDS.contains(&lower.as_str()) {
                return true;
            }
            // Handle contractions: "I'm" → "i"+"m", "she's" → "she"+"s", etc.
            lower
                .split('\'')
                .any(|part| ENGLISH_STOP_WORDS.contains(&part))
        })
        .count();

    let ratio = english_count as f64 / words.len() as f64;
    ratio > 0.25
}

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
        let mut last_seen_text = String::new();
        let mut consecutive_failures: u32 = 0;

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
            if current_text.is_empty() || current_text == last_seen_text {
                continue;
            }

            // Language detection strategy:
            // - If new text contains non-Latin letters (Hindi, Chinese, etc.),
            //   always translate — catches language switches immediately.
            // - Otherwise, check the full accumulated text with stop-word analysis
            //   so that short all-English deltas aren't misclassified.
            let new_text = if current_text.starts_with(&last_seen_text) {
                &current_text[last_seen_text.len()..]
            } else {
                &current_text
            };
            let has_non_latin = new_text
                .chars()
                .any(|c| c.is_alphabetic() && !c.is_ascii());
            let skip_english = if has_non_latin {
                false
            } else {
                is_likely_english(&current_text)
            };

            info!(
                "Streaming translation: skip_english={} text={:?}",
                skip_english, current_text
            );

            if skip_english {
                last_seen_text = current_text.clone();
                continue;
            }

            match translate_with_mistral_client(
                &client,
                &api_key,
                "ministral-8b-latest",
                &current_text,
            )
            .await
            {
                Some(translated) => {
                    last_seen_text = current_text.clone();
                    consecutive_failures = 0;
                    *final_translation.lock().unwrap() = translated.clone();
                    let _ = app.emit("streaming-translation-update", translated);
                }
                None => {
                    consecutive_failures += 1;
                    let backoff = std::cmp::min(
                        100 * (1u64 << (consecutive_failures - 1).min(6)),
                        5000,
                    );
                    warn!(
                        "Streaming translation failed ({} consecutive), backing off {}ms",
                        consecutive_failures, backoff
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
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
