use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

use crate::{AppState, DictationCmd};

// Phase 1 trigger: a chord, NOT the Fn key. See CLAUDE.md hard rule #4.
// Hold to dictate, release to commit.
const DICTATE: &str = "CmdOrCtrl+Shift+Space";

pub fn register<R: Runtime>(app: &AppHandle<R>) -> anyhow::Result<()> {
    let shortcut: Shortcut = DICTATE
        .parse()
        .expect("hard-coded shortcut string parses");

    app.global_shortcut().on_shortcut(
        shortcut,
        move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| match event.state() {
            ShortcutState::Pressed => on_press(app),
            ShortcutState::Released => on_release(app),
        },
    )?;
    Ok(())
}

fn on_press<R: Runtime>(app: &AppHandle<R>) {
    log::debug!("hotkey: press");
    if let Some(win) = app.get_webview_window("overlay") {
        let _ = win.set_always_on_top(true);
        let _ = win.show();
    }
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Start) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

fn on_release<R: Runtime>(app: &AppHandle<R>) {
    log::debug!("hotkey: release");
    if let Some(win) = app.get_webview_window("overlay") {
        let _ = win.hide();
    }
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Stop) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}
