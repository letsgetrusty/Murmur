// Auto-update via `tauri-plugin-updater`. The app checks a manifest endpoint
// (configured in tauri.conf.json → plugins.updater.endpoints), and if a newer,
// validly-signed release exists, downloads + installs it and relaunches.
//
// The update artifact is verified against the minisign public key baked into
// tauri.conf.json — this is a SEPARATE trust root from Apple code signing: it
// proves the update came from us, before macOS ever evaluates the new bundle.
// See docs/releasing.md for the signing + publishing flow.

use serde::Serialize;
use tauri::{AppHandle, Runtime};
use tauri_plugin_updater::UpdaterExt;

/// A newer release the user can install, shaped for the settings-window banner.
#[derive(Serialize, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub current_version: String,
    /// Release notes from the manifest, if any.
    pub notes: Option<String>,
}

/// Check the configured endpoint for a newer signed release. Returns `None` when
/// already up to date, the endpoint isn't reachable, or the updater isn't
/// configured (e.g. the placeholder endpoint) — all non-fatal and logged, so a
/// failed check never disrupts the app.
pub async fn check<R: Runtime>(app: &AppHandle<R>) -> Option<UpdateInfo> {
    let updater = match app.updater() {
        Ok(u) => u,
        Err(e) => {
            log::warn!("update: updater unavailable: {e}");
            return None;
        }
    };
    match updater.check().await {
        Ok(Some(update)) => {
            log::info!(
                "update: v{} available (current v{})",
                update.version,
                update.current_version
            );
            Some(UpdateInfo {
                version: update.version.clone(),
                current_version: update.current_version.clone(),
                notes: update.body.clone(),
            })
        }
        Ok(None) => {
            log::info!("update: up to date");
            None
        }
        Err(e) => {
            // Endpoint not set up yet (placeholder) or offline — expected, info-level.
            log::info!("update: check skipped ({e})");
            None
        }
    }
}

/// Download + install the available update (verifying its signature), then
/// relaunch. Errors (as a display string) if no update is available or the
/// download/install fails; the caller surfaces it to the user.
pub async fn install<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let updater = app
        .updater()
        .map_err(|e| format!("updater unavailable: {e}"))?;
    let update = updater
        .check()
        .await
        .map_err(|e| format!("update check failed: {e}"))?
        .ok_or_else(|| "no update available".to_string())?;

    log::info!("update: installing v{}…", update.version);
    let mut downloaded: usize = 0;
    update
        .download_and_install(
            |chunk, total| {
                downloaded += chunk;
                if let Some(total) = total {
                    log::debug!("update: {downloaded}/{total} bytes");
                }
            },
            || log::info!("update: download complete — installing"),
        )
        .await
        .map_err(|e| format!("update install failed: {e}"))?;

    log::info!("update: installed — relaunching");
    crate::relaunch(app);
    Ok(())
}
