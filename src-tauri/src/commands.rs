// Tauri IPC commands backing the main (settings/history) window. The overlay
// is event-driven; the settings window is request/response, so it goes through
// `invoke` handlers here.

use serde::Serialize;
use tauri::{AppHandle, State};

use crate::config::Config;
use crate::AppState;

/// Current config, for the Settings form to populate itself.
#[tauri::command]
pub fn get_config(state: State<AppState>) -> Config {
    state.config.lock().map(|c| c.clone()).unwrap_or_default()
}

/// Persist an edited config and apply it live. The shared `AppState.config` is
/// what the refiner reads on each refine, so refine model/prompt changes take
/// effect immediately; TTS/mic changes take effect on their next use.
#[tauri::command]
pub fn save_config(state: State<AppState>, config: Config) -> Result<(), String> {
    {
        let mut c = state.config.lock().map_err(|e| e.to_string())?;
        *c = config.clone();
    }
    crate::config::save(&config).map_err(|e| e.to_string())?;
    log::info!(
        "config: saved from settings (refine_model={})",
        config.refine_model
    );
    Ok(())
}

#[derive(Serialize)]
pub struct VoiceOption {
    id: String,
    name: String,
}

/// The choices the Settings dropdowns render (speeds, ElevenLabs voices, mics).
#[derive(Serialize)]
pub struct Options {
    speeds: Vec<f32>,
    voices: Vec<VoiceOption>,
    mics: Vec<String>,
}

#[tauri::command]
pub fn get_options(state: State<AppState>) -> Options {
    Options {
        speeds: crate::tts::SPEEDS.to_vec(),
        voices: crate::tts::ELEVENLABS_VOICES
            .iter()
            .map(|(id, name)| VoiceOption {
                id: (*id).to_string(),
                name: (*name).to_string(),
            })
            .collect(),
        mics: state.mic_names.clone(),
    }
}

// Speed/voice/mic go through the same `apply_*` helpers the tray uses, so the
// tray checkmarks and on-disk config stay in lockstep with the window.
#[tauri::command]
pub fn set_speed(app: AppHandle, speed: f32) {
    crate::apply_speed(&app, speed);
}

#[tauri::command]
pub fn set_voice(app: AppHandle, voice_id: String) {
    crate::apply_voice(&app, &voice_id);
}

#[tauri::command]
pub fn set_mic(app: AppHandle, name: Option<String>) {
    crate::apply_mic(&app, name);
}

/// Rebind a global chord live and persist it. `action` is one of
/// "dictate" | "tts_toggle" | "tts_speed"; `shortcut` is the plugin format.
#[tauri::command]
pub fn set_hotkey(
    app: AppHandle,
    state: State<AppState>,
    action: String,
    shortcut: String,
) -> Result<(), String> {
    let hk = crate::hotkeys::HotkeyAction::parse(&action)
        .ok_or_else(|| format!("unknown hotkey action: {action}"))?;

    let old = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        match action.as_str() {
            "dictate" => cfg.hotkey_dictate.clone(),
            "tts_toggle" => cfg.hotkey_tts.clone(),
            _ => cfg.hotkey_tts_speed.clone(),
        }
    };

    crate::hotkeys::rebind(&app, hk, &old, &shortcut).map_err(|e| e.to_string())?;

    let snapshot = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        match action.as_str() {
            "dictate" => cfg.hotkey_dictate = shortcut.clone(),
            "tts_toggle" => cfg.hotkey_tts = shortcut.clone(),
            _ => cfg.hotkey_tts_speed = shortcut.clone(),
        }
        cfg.clone()
    };
    crate::config::save(&snapshot).map_err(|e| e.to_string())
}

/// Cumulative local usage totals (refine tokens, STT seconds, TTS chars).
#[tauri::command]
pub fn get_usage(state: State<AppState>) -> crate::usage::UsageStats {
    state.usage.lock().map(|u| u.clone()).unwrap_or_default()
}

#[derive(Serialize)]
pub struct OpenRouterSpend {
    total_usd: f64,
    month_usd: f64,
}

/// Real OpenRouter spend for the configured key (all-time + this month), from
/// the provider's /key endpoint. This is the authoritative refinement cost.
#[tauri::command]
pub async fn get_openrouter_spend() -> Result<OpenRouterSpend, String> {
    let key = crate::secrets::get(crate::secrets::OPENROUTER_API_KEY)
        .map_err(|_| "no OpenRouter key in Keychain".to_string())?;
    let resp = reqwest::Client::new()
        .get("https://openrouter.ai/api/v1/key")
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("openrouter {}", resp.status()));
    }
    let j: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(parse_openrouter_spend(&j))
}

/// All-time + this-month spend from OpenRouter's `/key` response. Missing fields
/// default to 0 so a shape change degrades to "$0", not an error.
fn parse_openrouter_spend(json: &serde_json::Value) -> OpenRouterSpend {
    let d = &json["data"];
    OpenRouterSpend {
        total_usd: d["usage"].as_f64().unwrap_or(0.0),
        month_usd: d["usage_monthly"].as_f64().unwrap_or(0.0),
    }
}

#[tauri::command]
pub fn reset_usage(state: State<AppState>) -> Result<(), String> {
    let snapshot = {
        let mut u = state.usage.lock().map_err(|e| e.to_string())?;
        *u = crate::usage::UsageStats::default();
        u.clone()
    };
    crate::usage::save(&snapshot).map_err(|e| e.to_string())
}

/// Set the modifier held with Fn for refined dictation. Read live by the Fn
/// tap from the shared config, so it applies without a restart.
#[tauri::command]
pub fn set_refine_modifier(state: State<AppState>, modifier: String) -> Result<(), String> {
    if !["Ctrl", "Shift", "Alt", "Cmd"].contains(&modifier.as_str()) {
        return Err(format!("invalid modifier: {modifier}"));
    }
    let snapshot = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.refine_modifier = modifier.clone();
        cfg.clone()
    };
    crate::config::save(&snapshot).map_err(|e| e.to_string())?;
    log::info!("config: refine modifier → Fn+{modifier}");
    Ok(())
}

// --- History -----------------------------------------------------------------

#[tauri::command]
pub fn list_history(
    state: State<AppState>,
    query: String,
    limit: i64,
    offset: i64,
) -> Result<Vec<crate::history::Entry>, String> {
    let conn = state.history.lock().map_err(|e| e.to_string())?;
    crate::history::list(&conn, &query, limit, offset).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn delete_history(state: State<AppState>, id: i64) -> Result<(), String> {
    let conn = state.history.lock().map_err(|e| e.to_string())?;
    crate::history::delete(&conn, id).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn clear_history(state: State<AppState>) -> Result<(), String> {
    let conn = state.history.lock().map_err(|e| e.to_string())?;
    crate::history::clear(&conn).map_err(|e| e.to_string())
}

/// Aggregate usage stats for the Insights tab (14-day activity window).
#[tauri::command]
pub fn history_stats(state: State<AppState>) -> Result<crate::history::Stats, String> {
    let conn = state.history.lock().map_err(|e| e.to_string())?;
    crate::history::stats(&conn, 14).map_err(|e| e.to_string())
}

/// Put arbitrary text on the clipboard (used by the History "Copy" button).
#[tauri::command]
pub fn copy_text(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut c| c.set_text(text))
        .map_err(|e| e.to_string())
}

// --- API keys (Keychain) -----------------------------------------------------

// (ui id, keychain item name, label, what it powers)
const MANAGED_KEYS: &[(&str, &str, &str, &str)] = &[
    (
        "groq",
        crate::secrets::GROQ_API_KEY,
        "Groq",
        "Dictation — speech-to-text",
    ),
    (
        "elevenlabs",
        crate::secrets::ELEVENLABS_API_KEY,
        "ElevenLabs",
        "Read-aloud — text-to-speech",
    ),
    (
        "openrouter",
        crate::secrets::OPENROUTER_API_KEY,
        "OpenRouter",
        "Refinement — Fn+Ctrl dictation",
    ),
];

fn keychain_name(id: &str) -> Option<&'static str> {
    MANAGED_KEYS
        .iter()
        .find(|(i, ..)| *i == id)
        .map(|(_, name, ..)| *name)
}

/// Mask a secret for display: first 4 + … + last 4, or bullets if short.
fn mask(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        "•".repeat(chars.len().max(4))
    } else {
        let first: String = chars[..4].iter().collect();
        let last: String = chars[chars.len() - 4..].iter().collect();
        format!("{first}…{last}")
    }
}

#[derive(Serialize)]
pub struct KeyInfo {
    id: String,
    label: String,
    purpose: String,
    present: bool,
    masked: String,
}

/// Presence + masked preview for each managed key. Never returns full values.
#[tauri::command]
pub fn list_keys() -> Vec<KeyInfo> {
    MANAGED_KEYS
        .iter()
        .map(|(id, name, label, purpose)| {
            let val = crate::secrets::get(name).ok();
            KeyInfo {
                id: (*id).to_string(),
                label: (*label).to_string(),
                purpose: (*purpose).to_string(),
                present: val.is_some(),
                masked: val.as_deref().map(mask).unwrap_or_default(),
            }
        })
        .collect()
}

/// Full value for a single key, for the reveal toggle (local IPC only).
#[tauri::command]
pub fn reveal_key(id: String) -> Result<String, String> {
    let name = keychain_name(&id).ok_or_else(|| format!("unknown key: {id}"))?;
    crate::secrets::get(name).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn save_key(id: String, value: String) -> Result<(), String> {
    let name = keychain_name(&id).ok_or_else(|| format!("unknown key: {id}"))?;
    let v = value.trim();
    if v.is_empty() {
        return Err("key is empty".into());
    }
    crate::secrets::set(name, v).map_err(|e| e.to_string())?;
    log::info!("secrets: '{id}' key updated"); // never log the value
    Ok(())
}

#[tauri::command]
pub fn delete_key(id: String) -> Result<(), String> {
    let name = keychain_name(&id).ok_or_else(|| format!("unknown key: {id}"))?;
    crate::secrets::delete(name).map_err(|e| e.to_string())?;
    log::info!("secrets: '{id}' key removed");
    Ok(())
}

/// Open an external URL in the default browser (macOS `open`). Used by the
/// "Get a key" links in Settings. Guarded to http(s) so a stray value can't
/// hand `open` an arbitrary scheme or local path.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!("refusing to open non-http(s) url: {url}"));
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_long_key() {
        assert_eq!(mask("sk-abcdefghijklmnop"), "sk-a…mnop");
    }

    #[test]
    fn mask_short_key_is_bulleted() {
        assert_eq!(mask("abc"), "••••"); // <4 chars still shows 4 bullets
        assert_eq!(mask("abcdefgh"), "••••••••"); // 8 chars, all bullets
    }

    #[test]
    fn open_url_rejects_non_http() {
        assert!(open_url("file:///etc/passwd".into()).is_err());
        assert!(open_url("javascript:alert(1)".into()).is_err());
        assert!(open_url("ftp://example.com".into()).is_err());
    }

    #[test]
    fn parse_openrouter_spend_reads_data() {
        let j = serde_json::json!({"data": {"usage": 12.5, "usage_monthly": 3.25}});
        let s = parse_openrouter_spend(&j);
        assert!((s.total_usd - 12.5).abs() < 1e-9);
        assert!((s.month_usd - 3.25).abs() < 1e-9);
    }

    #[test]
    fn parse_openrouter_spend_defaults_to_zero() {
        let s = parse_openrouter_spend(&serde_json::json!({}));
        assert_eq!(s.total_usd, 0.0);
        assert_eq!(s.month_usd, 0.0);
    }
}
