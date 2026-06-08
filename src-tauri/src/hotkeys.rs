use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

// v1 trigger is a chord, NOT the Fn key. See CLAUDE.md hard rule #4.
const TOGGLE_OVERLAY: &str = "CmdOrCtrl+Shift+Space";

pub fn register<R: Runtime>(app: &AppHandle<R>) -> anyhow::Result<()> {
    let shortcut: Shortcut = TOGGLE_OVERLAY
        .parse()
        .expect("hard-coded shortcut string parses");

    app.global_shortcut().on_shortcut(
        shortcut,
        move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
            // Fire on key-down only; v1 is a press-to-toggle, not press-to-hold.
            if event.state() == ShortcutState::Pressed {
                log::info!("hotkey fired: {TOGGLE_OVERLAY}");
                toggle_overlay(app);
            }
        },
    )?;

    Ok(())
}

pub fn toggle_overlay<R: Runtime>(app: &AppHandle<R>) {
    let Some(win) = app.get_webview_window("overlay") else {
        log::warn!("overlay window not found");
        return;
    };

    match win.is_visible() {
        Ok(true) => {
            if let Err(e) = win.hide() {
                log::warn!("overlay hide failed: {e}");
            }
        }
        Ok(false) => {
            let _ = win.set_always_on_top(true);
            if let Err(e) = win.show() {
                log::warn!("overlay show failed: {e}");
            }
        }
        Err(e) => log::warn!("overlay visibility check failed: {e}"),
    }
}
