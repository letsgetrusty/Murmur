// Cumulative local usage counters (dictations, refinements, read-alouds),
// persisted to a small JSON file so totals survive restarts. Drives the Insights
// stats. All on-device — no provider billing.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    /// Refined dictations (Fn+Ctrl).
    #[serde(default)]
    pub refine_count: u64,
    /// Dictations transcribed + total audio seconds.
    #[serde(default)]
    pub stt_count: u64,
    #[serde(default)]
    pub stt_seconds: f64,
    /// Read-alouds + total characters spoken.
    #[serde(default)]
    pub tts_count: u64,
    #[serde(default)]
    pub tts_chars: u64,
}

impl UsageStats {
    pub fn record_refine(&mut self) {
        self.refine_count += 1;
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
    p.push("Library/Application Support/murmur/usage.json");
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
        u.record_refine();
        u.record_refine();
        assert_eq!(u.refine_count, 2);

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
