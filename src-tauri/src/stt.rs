// Phase 1: cloud STT via Groq's OpenAI-compatible Whisper endpoint.
//
// `Transcriber` is the seam — Phase 2 swaps in `whisper-rs` behind the same
// trait. Using a hand-rolled Pin<Box<Future>> instead of pulling in
// `async-trait`, since async-trait isn't in the stack list and the friction is
// small.
//
// The API key is read lazily from the keyring on first use and cached in
// memory for the rest of the session, so the user sees at most one macOS
// authorization prompt per app launch (typically zero if the keychain item
// has its ACL relaxed via `security ... -A`).

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};

use crate::secrets;

pub type TranscribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a>;
}

pub struct GroqWhisper {
    client: reqwest::Client,
    model: String,
    /// Cached API key. We avoid hitting the keychain on every transcribe so
    /// the user only sees the macOS authorization prompt once per app launch.
    /// If the user rotates the key, restart murmur to pick it up.
    api_key: Mutex<Option<String>>,
}

impl GroqWhisper {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            // whisper-large-v3-turbo: fastest of Groq's options, plenty
            // accurate for short dictation.
            model: "whisper-large-v3-turbo".into(),
            api_key: Mutex::new(None),
        }
    }

    fn api_key(&self) -> Result<String> {
        let mut cached = self
            .api_key
            .lock()
            .map_err(|_| anyhow!("api key cache poisoned"))?;
        if let Some(k) = cached.as_ref() {
            return Ok(k.clone());
        }
        let k = secrets::get(secrets::GROQ_API_KEY).map_err(|_| {
            anyhow!(
                "no Groq API key in Keychain. Set one with:\n  security add-generic-password -A -s murmur -a groq_api_key -w\nthen paste the key when prompted."
            )
        })?;
        *cached = Some(k.clone());
        Ok(k)
    }
}

impl Default for GroqWhisper {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcriber for GroqWhisper {
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
        Box::pin(async move {
            let api_key = self.api_key()?;

            let part = reqwest::multipart::Part::bytes(wav.to_vec())
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .context("set audio part mime")?;
            let form = reqwest::multipart::Form::new()
                .text("model", self.model.clone())
                .text("response_format", "json")
                // Pin to English. Whisper auto-detect frequently hallucinates
                // languages like Chinese/Korean from short English clips with
                // mild noise. Make this configurable in a later phase.
                .text("language", "en")
                .part("file", part);

            let resp = self
                .client
                .post("https://api.groq.com/openai/v1/audio/transcriptions")
                .bearer_auth(api_key)
                .multipart(form)
                .send()
                .await
                .context("POST groq transcription")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("groq transcription failed ({status}): {body}"));
            }

            let json: serde_json::Value = resp.json().await.context("decode groq response")?;
            Ok(json["text"].as_str().unwrap_or("").trim().to_string())
        })
    }
}
