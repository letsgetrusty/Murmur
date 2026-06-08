use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

use crate::{emit_state, show_overlay, AppState, DictationCmd, OverlayState};

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
    log::info!("hotkey: press");
    show_overlay(app);
    emit_state(app, OverlayState::Recording);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Start) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

fn on_release<R: Runtime>(app: &AppHandle<R>) {
    log::info!("hotkey: release");
    // Keep the overlay visible — the router will flip it to Done/Error and
    // schedule the hide once the transcribe + inject pipeline returns.
    emit_state(app, OverlayState::Transcribing);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Stop) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}
