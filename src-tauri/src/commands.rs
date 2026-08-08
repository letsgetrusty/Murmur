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
        // Re-enumerate live (not the startup-cached `state.mic_names`) so a mic
        // plugged in after launch shows up when the user opens Settings. Safe
        // here: this command runs off the main thread, well past the startup
        // window where early CoreAudio enumeration crashes the release build —
        // the recorder already enumerates on demand the same way (`audio.rs`).
        mics: crate::audio::list_input_devices(),
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

/// Onboarding opt-out for the neural read-aloud voice. Persists the choice of
/// TTS backend (Kokoro when enabled, native macOS otherwise). Persists the
/// choice only; the actual ~310 MB fetch is kicked off by `download_neural_voice`
/// when kept, so opting out never downloads it.
#[tauri::command]
pub fn set_neural_voice(state: State<AppState>, enabled: bool) -> Result<(), String> {
    let provider = if enabled { "kokoro" } else { "native" };
    let snapshot = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.tts_provider = provider.to_string();
        cfg.clone()
    };
    crate::config::save(&snapshot).map_err(|e| e.to_string())?;
    log::info!("config: neural voice {} → tts_provider={provider}", enabled);
    Ok(())
}

/// Kick off the Kokoro model + voice download from onboarding so its progress bar
/// fills alongside Whisper/Qwen. Idempotent — no-ops (and reports a full bar) if
/// the assets are already on disk. Progress is emitted on the `model-download`
/// event with id `kokoro`.
#[tauri::command]
pub fn download_neural_voice(app: AppHandle) {
    let dl_app = app.clone();
    tauri::async_runtime::spawn(async move {
        let emit = |downloaded, total| {
            crate::emit_download_progress(&dl_app, crate::ipc::download::KOKORO, downloaded, total)
        };
        if let Err(e) = crate::tts::ensure_kokoro_assets(emit).await {
            log::warn!("tts/kokoro: onboarding download failed: {e}");
            crate::emit_download_error(&dl_app, crate::ipc::download::KOKORO);
        }
    });
}

// --- Onboarding (first-run setup) --------------------------------------------

/// Live permission + model-download state for the onboarding window to poll.
#[derive(Serialize)]
pub struct OnboardingStatus {
    /// Accessibility grant — the one required permission.
    accessibility: bool,
    /// Microphone: mirrors AVAuthorizationStatus (0 notDetermined, 1 restricted,
    /// 2 denied, 3 authorized).
    microphone: i64,
    /// Whether the default Whisper + refine models are already on disk.
    whisper_ready: bool,
    llm_ready: bool,
    /// Whether the Kokoro neural-voice assets are already on disk.
    kokoro_ready: bool,
}

#[tauri::command]
pub fn onboarding_status(state: State<AppState>) -> OnboardingStatus {
    let (stt_model, llm_model) = state
        .config
        .lock()
        .map(|c| (c.stt_model.clone(), c.llm_model.clone()))
        .unwrap_or_default();
    let whisper_ready = crate::stt::model_path(&stt_model)
        .map(|p| p.exists())
        .unwrap_or(false);
    OnboardingStatus {
        accessibility: crate::permissions::accessibility_granted(),
        microphone: crate::permissions::microphone_status(),
        whisper_ready,
        llm_ready: crate::local_llm::assets_present(&llm_model),
        kokoro_ready: crate::tts::kokoro_assets_present(),
    }
}

#[tauri::command]
pub fn open_accessibility_settings() {
    crate::permissions::open_accessibility_settings();
}

#[tauri::command]
pub fn open_microphone_settings() {
    crate::permissions::open_microphone_settings();
}

/// Trigger the macOS microphone-permission prompt (via a brief capture) and
/// return the resulting authorization status. Runs the capture on a blocking
/// thread since the cpal stream is `!Send`.
#[tauri::command]
pub async fn request_microphone() -> i64 {
    let _ = tauri::async_runtime::spawn_blocking(|| {
        if let Err(e) = crate::audio::probe_microphone() {
            log::warn!("onboarding: mic probe failed: {e}");
        }
    })
    .await;
    crate::permissions::microphone_status()
}

/// Mark onboarding complete and persist it. The caller (onboarding JS) then
/// either relaunches — to activate the Fn tap once Accessibility is granted —
/// or closes the window, so we don't tear the webview down mid-call here.
#[tauri::command]
pub fn finish_onboarding(state: State<AppState>) -> Result<(), String> {
    let snapshot = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.onboarding_done = true;
        cfg.clone()
    };
    crate::config::save(&snapshot).map_err(|e| e.to_string())?;
    log::info!("onboarding: complete");
    Ok(())
}

// --- Auto-update -------------------------------------------------------------

/// The version of a staged (already-downloaded) update, if one is ready. Drives
/// the settings "Restart to update" banner when the window opens.
#[tauri::command]
pub fn pending_update_version(state: State<AppState>) -> Option<String> {
    state
        .pending_update
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|u| u.version.clone()))
}

/// Apply the staged (already-downloaded + verified) update and relaunch.
#[tauri::command]
pub async fn install_staged_update(app: AppHandle) -> Result<(), String> {
    crate::install_pending(&app).await
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
/// Delegates to `crate::relaunch` (LaunchServices `open`, not `app.restart()`).
#[tauri::command]
pub fn relaunch_app(app: AppHandle) {
    crate::relaunch(&app);
}
