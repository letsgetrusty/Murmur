// Post-dictation text refinement via an LLM. Triggered by Fn+Ctrl: the raw
// transcript is run through a cleanup prompt and the refined text is injected
// instead of the raw transcript. Turns rambling dictation into polished text
// in one step.
//
// `Refiner` is the seam (mirrors `stt::Transcriber`) so the provider can be
// swapped via config without touching the call site. OpenRouter is an
// OpenAI-compatible gateway, so this is a plain chat-completions POST — the
// same shape as the Groq STT call. The model + system prompt come from config.
//
// The API key is read lazily from the keyring on first use and cached for the
// session, so the user sees at most one macOS authorization prompt per launch.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use serde_json::json;

use crate::config::Config;
use crate::secrets;
use crate::usage::UsageStats;

pub type RefineFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

pub trait Refiner: Send + Sync {
    fn refine<'a>(&'a self, text: &'a str) -> RefineFuture<'a>;
}

pub struct OpenRouterRefiner {
    client: reqwest::Client,
    /// Shared with `AppState` so edits from the Settings window take effect on
    /// the next refine without a restart.
    config: Arc<Mutex<Config>>,
    /// Cumulative token/cost totals, shared with `AppState`.
    usage: Arc<Mutex<UsageStats>>,
    api_key: Mutex<Option<String>>,
}

impl OpenRouterRefiner {
    pub fn new(config: Arc<Mutex<Config>>, usage: Arc<Mutex<UsageStats>>) -> Self {
        Self {
            client: reqwest::Client::new(),
            config,
            usage,
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
        let k = secrets::get(secrets::OPENROUTER_API_KEY).map_err(|_| {
            anyhow!("no OpenRouter API key in Keychain. Set one with:\n  murmur set-key openrouter")
        })?;
        *cached = Some(k.clone());
        Ok(k)
    }
}

impl Refiner for OpenRouterRefiner {
    fn refine<'a>(&'a self, text: &'a str) -> RefineFuture<'a> {
        Box::pin(async move {
            let api_key = self.api_key()?;
            // Snapshot model + prompt from shared config (no await while locked).
            let (model, system_prompt) = {
                let cfg = self
                    .config
                    .lock()
                    .map_err(|_| anyhow!("config lock poisoned"))?;
                (cfg.refine_model.clone(), cfg.refine_prompt.clone())
            };
            let body = json!({
                "model": model,
                "messages": [
                    { "role": "system", "content": system_prompt },
                    { "role": "user", "content": text },
                ],
                // Ask OpenRouter to return token counts + actual USD cost.
                "usage": { "include": true },
            });

            let resp = self
                .client
                .post("https://openrouter.ai/api/v1/chat/completions")
                .bearer_auth(api_key)
                // Optional attribution headers (used only for OpenRouter's
                // public rankings); harmless to send.
                .header("HTTP-Referer", "https://github.com/local/murmur")
                .header("X-Title", "murmur")
                .json(&body)
                .send()
                .await
                .context("POST openrouter chat completion")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(anyhow!("openrouter refine failed ({status}): {body}"));
            }

            let json: serde_json::Value = resp.json().await.context("decode openrouter response")?;
            let out = json["choices"][0]["message"]["content"]
                .as_str()
                .ok_or_else(|| anyhow!("openrouter response missing choices[0].message.content"))?
                .trim()
                .to_string();
            if out.is_empty() {
                return Err(anyhow!("openrouter returned empty content"));
            }

            // Record token usage + cost (best-effort — never fail a refine over it).
            if let Some(u) = json.get("usage") {
                let field = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
                let cost = u.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
                if let Ok(mut stats) = self.usage.lock() {
                    stats.record(
                        field("prompt_tokens"),
                        field("completion_tokens"),
                        field("total_tokens"),
                        cost,
                    );
                    let snapshot = stats.clone();
                    drop(stats);
                    if let Err(e) = crate::usage::save(&snapshot) {
                        log::warn!("usage: save failed: {e}");
                    }
                }
            }

            Ok(out)
        })
    }
}
