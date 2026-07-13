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
    state
        .config
        .lock()
        .map(|c| c.clone())
        .unwrap_or_default()
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
