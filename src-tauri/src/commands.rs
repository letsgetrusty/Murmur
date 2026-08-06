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
    log::info!("config: saved from settings");
    Ok(())
}

#[derive(Serialize)]
pub struct VoiceOption {
    id: String,
    name: String,
}

/// The choices the Settings dropdowns render (speeds, TTS voices, mics).
#[derive(Serialize)]
pub struct Options {
    speeds: Vec<f32>,
    voices: Vec<VoiceOption>,
    mics: Vec<String>,
}

#[tauri::command]
pub fn get_options(state: State<AppState>) -> Options {
    let provider = state
        .config
        .lock()
        .map(|c| c.tts_provider.clone())
        .unwrap_or_default();
    Options {
        speeds: crate::tts::SPEEDS.to_vec(),
        voices: crate::tts::voices_for(&provider)
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
            "tts_speed" => cfg.hotkey_tts_speed.clone(),
            _ => return Err(format!("unknown hotkey action: {action}")),
        }
    };

    crate::hotkeys::rebind(&app, hk, &old, &shortcut).map_err(|e| e.to_string())?;

    let snapshot = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        match action.as_str() {
            "dictate" => cfg.hotkey_dictate = shortcut.clone(),
            "tts_toggle" => cfg.hotkey_tts = shortcut.clone(),
            "tts_speed" => cfg.hotkey_tts_speed = shortcut.clone(),
            _ => {}
        }
        cfg.clone()
    };
    crate::config::save(&snapshot).map_err(|e| e.to_string())
}

/// Cumulative local usage totals (refinements, dictations, read-alouds).
#[tauri::command]
pub fn get_usage(state: State<AppState>) -> crate::usage::UsageStats {
    state.usage.lock().map(|u| u.clone()).unwrap_or_default()
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

/// Relaunch the app so a startup-time change (e.g. the STT/TTS engine, built
/// once at launch) takes effect — one click instead of re-running dev.sh.
///
/// We deliberately do NOT use `app.restart()`: on macOS Tauri relaunches by
/// spawning the binary as a *child* process, and macOS then attributes the TCC
/// responsible process to the parent chain — which wedges the Fn-key tap /
/// Accessibility grant (the exact failure dev.sh's `open` launch avoids). So we
/// relaunch through LaunchServices (`open`) instead, matching dev.sh, so Open Wispr
/// stays its own responsible process and the grant survives.
#[tauri::command]
pub fn relaunch_app(app: AppHandle) {
    if let Ok(exe) = std::env::current_exe() {
        // exe = <OpenWispr.app>/Contents/MacOS/<bin>; walk up to the .app bundle.
        if let Some(bundle) = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
        {
            let pid = std::process::id();
            // Detached helper: wait for us to fully exit, then `open` the bundle
            // (a fresh LaunchServices launch → correct responsible process).
            let cmd = format!(
                "while kill -0 {pid} 2>/dev/null; do sleep 0.1; done; open '{}'",
                bundle.display()
            );
            let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
            app.exit(0);
            return;
        }
    }
    // Not inside an .app bundle (unusual) — fall back to Tauri's restart.
    app.restart();
}
