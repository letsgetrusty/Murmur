// Auto-update via `tauri-plugin-updater`. The app checks a manifest endpoint
// (configured in tauri.conf.json → plugins.updater.endpoints), and if a newer,
// validly-signed release exists, downloads + installs it and relaunches.
//
// The update artifact is verified against the minisign public key baked into
// tauri.conf.json — this is a SEPARATE trust root from Apple code signing: it
// proves the update came from us, before macOS ever evaluates the new bundle.
// See docs/releasing.md for the signing + publishing flow.

use tauri::{AppHandle, Runtime};
use tauri_plugin_updater::UpdaterExt;

/// A downloaded-and-verified update held in memory until the user restarts.
/// Pre-fetching means "Restart to update" applies instantly.
pub struct StagedUpdate {
    pub version: String,
    bytes: Vec<u8>,
}

/// Check the endpoint and, if a newer release exists, download + verify it in
/// the background. Returns the staged bytes, or `None` when up to date /
/// unreachable / the endpoint isn't configured (all non-fatal, logged).
pub async fn check_and_download<R: Runtime>(app: &AppHandle<R>) -> Option<StagedUpdate> {
    let updater = app.updater().ok()?;
    let update = match updater.check().await {
        Ok(Some(u)) => u,
        Ok(None) => return None,
        Err(e) => {
            log::info!("update: check skipped ({e})");
            return None;
        }
    };
    let version = update.version.clone();
    log::info!("update: v{version} available — downloading in the background");
    match update.download(|_, _| {}, || {}).await {
        Ok(bytes) => {
            log::info!(
                "update: v{version} downloaded + verified ({} bytes) — staged",
                bytes.len()
            );
            Some(StagedUpdate { version, bytes })
        }
        Err(e) => {
            log::warn!("update: background download failed: {e}");
            None
        }
    }
}

/// Install a previously-staged (already-verified) update, then relaunch. Errors
/// as a display string if the update is no longer offered or the install fails.
pub async fn install_staged<R: Runtime>(
    app: &AppHandle<R>,
    staged: StagedUpdate,
) -> Result<(), String> {
    let updater = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("update check failed: {e}"))?
        .ok_or_else(|| "update is no longer available".to_string())?;
    update
        .install(&staged.bytes)
        .map_err(|e| format!("install failed: {e}"))?;
    log::info!("update: installed v{} — relaunching", staged.version);
    crate::relaunch(app);
    Ok(())
}
