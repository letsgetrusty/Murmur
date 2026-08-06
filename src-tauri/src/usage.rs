// Cumulative token/cost tracking for the OpenRouter refinement calls. Persisted
// to a small JSON file so the total survives restarts. STT (audio-seconds) and
// TTS (characters) bill differently and aren't tracked here.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    // Refinement (OpenRouter). Tokens are exact; `cost_usd` is what OpenRouter
    // reported per call. The OpenRouter /key API is authoritative for total $.
    pub refine_count: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: f64,
    // Dictation (Groq) — no provider usage API, so tracked locally since Open Wispr
    // started counting. Cost is estimated in the UI from the audio duration.
    #[serde(default)]
    pub stt_count: u64,
    #[serde(default)]
    pub stt_seconds: f64,
    // Read-aloud (ElevenLabs) — key can't read usage, so tracked locally.
    #[serde(default)]
    pub tts_count: u64,
    #[serde(default)]
    pub tts_chars: u64,
}

impl UsageStats {
    pub fn record(&mut self, prompt: u64, completion: u64, total: u64, cost: f64) {
        self.refine_count += 1;
        self.prompt_tokens += prompt;
        self.completion_tokens += completion;
        self.total_tokens += total;
        self.cost_usd += cost;
    }

    pub fn record_stt(&mut self, seconds: f64) {
        self.stt_count += 1;
        self.stt_seconds += seconds;
    }

    pub fn record_tts(&mut self, chars: u64) {
        self.tts_count += 1;
        self.tts_chars += chars;
    }
}

fn path() -> Option<PathBuf> {
    let mut p = PathBuf::from(std::env::var_os("HOME")?);
    p.push("Library/Application Support/openwispr/usage.json");
    Some(p)
}

pub fn load() -> UsageStats {
    match path().and_then(|p| std::fs::read(p).ok()) {
        Some(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        None => UsageStats::default(),
    }
}

pub fn save(stats: &UsageStats) -> Result<()> {
    let p = path().context("HOME unset")?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let json = serde_json::to_vec_pretty(stats).context("serialize usage")?;
    std::fs::write(p, json).context("write usage")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_accumulates() {
        let mut u = UsageStats::default();
        u.record(10, 5, 15, 0.02);
        u.record(2, 3, 5, 0.01);
        assert_eq!(u.refine_count, 2);
        assert_eq!(u.prompt_tokens, 12);
        assert_eq!(u.completion_tokens, 8);
        assert_eq!(u.total_tokens, 20);
        assert!((u.cost_usd - 0.03).abs() < 1e-9);

        u.record_stt(1.5);
        u.record_stt(2.5);
        assert_eq!(u.stt_count, 2);
        assert!((u.stt_seconds - 4.0).abs() < 1e-9);

        u.record_tts(100);
        u.record_tts(50);
        assert_eq!(u.tts_count, 2);
        assert_eq!(u.tts_chars, 150);
    }
}
