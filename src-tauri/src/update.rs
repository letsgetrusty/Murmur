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
    /// Release notes (the `notes` field of `latest.json`), markdown. `None` when
    /// the release didn't ship any — the banner then shows just the version.
    pub notes: Option<String>,
    bytes: Vec<u8>,
}

impl StagedUpdate {
    /// The subset of a staged update the settings banner needs: the new version,
    /// the version it replaces, and the release notes. Serialized to the webview
    /// (camelCase) both as the `update-staged` event payload and the reply to the
    /// `pending_update_version` command.
    pub fn info(&self) -> UpdateInfo {
        UpdateInfo {
            version: self.version.clone(),
            current_version: env!("CARGO_PKG_VERSION").to_string(),
            notes: self.notes.clone(),
        }
    }
}

/// What the "update ready" banner renders: the incoming version, the one it
/// replaces (so the UI can link the `vOLD...vNEW` diff), and the release notes.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    pub notes: Option<String>,
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
    let notes = update.body.clone();
    log::info!("update: v{version} available — downloading in the background");
    match update.download(|_, _| {}, || {}).await {
        Ok(bytes) => {
            log::info!(
                "update: v{version} downloaded + verified ({} bytes) — staged",
                bytes.len()
            );
            Some(StagedUpdate {
                version,
                notes,
                bytes,
            })
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
