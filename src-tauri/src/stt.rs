// Phase 1: cloud STT via Groq's OpenAI-compatible Whisper endpoint.
//
// `Transcriber` is the seam — Phase 2 swaps in `whisper-rs` behind the same
// trait. Using a hand-rolled Pin<Box<Future>> instead of pulling in
// `async-trait`, since async-trait isn't in the stack list and the friction is
// small.

use std::future::Future;
use std::pin::Pin;

use anyhow::{anyhow, Context, Result};

pub type TranscribeFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait Transcriber: Send + Sync {
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a>;
}

pub struct GroqWhisper {
    client: reqwest::Client,
    api_key: String,
    model: String,
}

impl GroqWhisper {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            // Whisper-large-v3-turbo: fastest of Groq's options, plenty accurate
            // for short dictation.
            model: "whisper-large-v3-turbo".into(),
        }
    }
}

impl Transcriber for GroqWhisper {
    fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
        Box::pin(async move {
            let part = reqwest::multipart::Part::bytes(wav.to_vec())
                .file_name("audio.wav")
                .mime_str("audio/wav")
                .context("set audio part mime")?;
            let form = reqwest::multipart::Form::new()
                .text("model", self.model.clone())
                .text("response_format", "json")
                .part("file", part);

            let resp = self
                .client
                .post("https://api.groq.com/openai/v1/audio/transcriptions")
                .bearer_auth(&self.api_key)
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
