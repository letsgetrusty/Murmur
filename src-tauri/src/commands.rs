// Tauri IPC commands backing the main (settings/history) window. The overlay
// is event-driven; the settings window is request/response, so it goes through
// `invoke` handlers here.

use tauri::State;

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
