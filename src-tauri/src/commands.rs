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

/// Play a short sample in the current voice, so picking one in Settings gives an
/// instant preview. Call after `set_voice` so it uses the just-selected voice.
/// `name` is the voice's friendly name (e.g. "Puck"). Kokoro caches the rendered
/// clip so replays are instant (`Speaker::preview`); `preview_text` keeps the
/// phrasing identical to the pre-generated cache.
#[tauri::command]
pub fn preview_voice(state: State<AppState>, name: String) {
    let sample = crate::tts::preview_text(&name);
    state.speaker.preview(&sample);
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
    // Keep the tray's "Read selection (…)" hint in sync with the new chord.
    if action == "tts_toggle" {
        crate::refresh_read_label(&app, &shortcut);
    }
    crate::config::save(&snapshot).map_err(|e| e.to_string())
}

/// Reset every key binding — the alternate chords, the hold-to-talk trigger, and
/// the refine modifier — to their defaults. Re-registers the global chords live
/// (so they work without a restart) and persists. Returns the updated config so
/// the settings UI can re-render its controls.
#[tauri::command]
pub fn reset_hotkeys(
    app: AppHandle,
    state: State<AppState>,
) -> Result<crate::config::Config, String> {
    use crate::config::{
        DEFAULT_DICTATION_TRIGGER, DEFAULT_HOTKEY_DICTATE, DEFAULT_HOTKEY_TTS,
        DEFAULT_HOTKEY_TTS_SPEED, DEFAULT_REFINE_MODIFIER,
    };
    use crate::hotkeys::HotkeyAction;

    // Current chords, so each can be unregistered before rebinding to its default.
    let (old_dictate, old_tts, old_speed) = {
        let cfg = state.config.lock().map_err(|e| e.to_string())?;
        (
            cfg.hotkey_dictate.clone(),
            cfg.hotkey_tts.clone(),
            cfg.hotkey_tts_speed.clone(),
        )
    };

    // Re-register each chord at its default. `rebind` restores the old binding if
    // the new one fails, so a chord never ends up unbound.
    for (action, old, default) in [
        (HotkeyAction::Dictate, &old_dictate, DEFAULT_HOTKEY_DICTATE),
        (HotkeyAction::TtsToggle, &old_tts, DEFAULT_HOTKEY_TTS),
        (HotkeyAction::TtsSpeed, &old_speed, DEFAULT_HOTKEY_TTS_SPEED),
    ] {
        crate::hotkeys::rebind(&app, action, old, default).map_err(|e| e.to_string())?;
    }

    // Persist all keybinding fields at their defaults. The trigger + refine
    // modifier are read live by the Fn tap, so they need no re-registration.
    let snapshot = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.hotkey_dictate = DEFAULT_HOTKEY_DICTATE.to_string();
        cfg.hotkey_tts = DEFAULT_HOTKEY_TTS.to_string();
        cfg.hotkey_tts_speed = DEFAULT_HOTKEY_TTS_SPEED.to_string();
        cfg.dictation_trigger = DEFAULT_DICTATION_TRIGGER.to_string();
        cfg.refine_modifier = DEFAULT_REFINE_MODIFIER.to_string();
        cfg.clone()
    };
    crate::config::save(&snapshot).map_err(|e| e.to_string())?;
    // The read-aloud chord just reset to its default — resync the tray hint.
    crate::refresh_read_label(&app, &snapshot.hotkey_tts);
    log::info!("config: key bindings reset to defaults");
    Ok(snapshot)
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

/// Set the hold-to-talk trigger key (for keyboards without an Fn key, or to use a
/// dedicated modifier). Read live by the hardware tap, so it applies without a
/// restart. One of "Fn" | "RightCtrl" | "RightAlt" | "RightCmd" | "Ctrl" | "Alt"
/// | "Cmd".
#[tauri::command]
pub fn set_dictation_trigger(state: State<AppState>, trigger: String) -> Result<(), String> {
    const ACCEPTED: &[&str] = &[
        "Fn",
        "RightCtrl",
        "RightAlt",
        "RightCmd",
        "Ctrl",
        "Alt",
        "Cmd",
    ];
    if !ACCEPTED.contains(&trigger.as_str()) {
        return Err(format!("invalid trigger: {trigger}"));
    }
    let snapshot = {
        let mut cfg = state.config.lock().map_err(|e| e.to_string())?;
        cfg.dictation_trigger = trigger.clone();
        cfg.clone()
    };
    crate::config::save(&snapshot).map_err(|e| e.to_string())?;
    log::info!("config: dictation trigger → {trigger}");
    Ok(())
}

/// Kick off the Kokoro model + voice download from onboarding so its progress bar
/// fills alongside Whisper/Qwen. Idempotent + guarded (no-op if already on disk or
/// already downloading). Progress is emitted on `model-download` with id `kokoro`.
#[tauri::command]
pub fn download_neural_voice(app: AppHandle) {
    crate::spawn_download(&app, crate::ipc::download::KOKORO);
}

/// Retry (or start) a model download after a failure — powers the onboarding
/// "Retry" buttons and any in-app retry. `id` is "whisper" | "llm" | "kokoro".
/// Guarded, so a click while a download is running is a harmless no-op.
#[tauri::command]
pub fn retry_download(app: AppHandle, id: String) -> Result<(), String> {
    let id = match id.as_str() {
        "whisper" => crate::ipc::download::WHISPER,
        "llm" => crate::ipc::download::LLM,
        "kokoro" => crate::ipc::download::KOKORO,
        other => return Err(format!("unknown download id: {other}")),
    };
    crate::spawn_download(&app, id);
    Ok(())
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

/// Trigger the macOS microphone-permission prompt and return the resulting
/// authorization status. Uses `AVCaptureDevice.requestAccess`, which reliably
/// raises the TCC dialog and reports the user's actual decision (it blocks until
/// they answer) — run on a blocking pool thread so the async runtime isn't held.
#[tauri::command]
pub async fn request_microphone() -> i64 {
    tauri::async_runtime::spawn_blocking(crate::permissions::request_microphone_access)
        .await
        .unwrap_or_else(|_| crate::permissions::microphone_status())
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

/// Murmur's version (compile-time), for the Support tab's "About" card.
#[tauri::command]
pub fn app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Open a web URL in the user's default browser (Support tab's GitHub links).
/// Restricted to http(s) so it can't be coerced into `open`-ing a local path or
/// arbitrary URL scheme.
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("only http(s) URLs are allowed".into());
    }
    std::process::Command::new("open")
        .arg(&url)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
