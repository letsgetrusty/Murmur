use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutEvent, ShortcutState};

use crate::{emit_state, show_overlay, tts_speed_cycle, tts_toggle, AppState, DictationCmd, OverlayState};

// Secondary dictation trigger. v1 default is the chord; Fn-hold lives in
// `fn_key` (CLAUDE.md hard rule #4 was relaxed by explicit user request).
const DICTATE: &str = "CmdOrCtrl+Shift+Space";
// Phase 3 TTS trigger: tap once to start reading the selection, tap again
// to stop. Posts AVSpeechSynthesizer on the main thread.
const TTS_TOGGLE: &str = "Alt+A";
// Cycle playback speed (1.0x ↔ 2.0x). Only meaningful for backends that
// support it (ElevenLabs).
const TTS_SPEED: &str = "Alt+Shift+S";

pub fn register<R: Runtime>(app: &AppHandle<R>) -> anyhow::Result<()> {
    let dictate: Shortcut = DICTATE
        .parse()
        .expect("hard-coded shortcut string parses");
    let tts: Shortcut = TTS_TOGGLE
        .parse()
        .expect("hard-coded shortcut string parses");
    let tts_speed: Shortcut = TTS_SPEED
        .parse()
        .expect("hard-coded shortcut string parses");

    let gs = app.global_shortcut();
    gs.on_shortcut(
        dictate,
        move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| match event.state() {
            ShortcutState::Pressed => on_press(app),
            ShortcutState::Released => on_release(app),
        },
    )?;
    gs.on_shortcut(
        tts,
        move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
            // Toggle on key-down only; releases mean nothing for TTS.
            if event.state() == ShortcutState::Pressed {
                tts_toggle(app);
            }
        },
    )?;
    gs.on_shortcut(
        tts_speed,
        move |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
            if event.state() == ShortcutState::Pressed {
                tts_speed_cycle(app);
            }
        },
    )?;
    Ok(())
}

/// Begin a dictation. Safe to call from any thread that can resolve
/// `AppHandle`; called by both the chord callback (this module) and the
/// Fn-key event tap (`fn_key`).
pub fn on_press<R: Runtime>(app: &AppHandle<R>) {
    log::info!("hotkey: press");
    show_overlay(app);
    emit_state(app, OverlayState::Recording);
    register_escape(app);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Start) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

/// Commit a dictation. Keep the overlay visible — the router flips it to
/// Done/Error and schedules the idle render once transcribe + inject return.
pub fn on_release<R: Runtime>(app: &AppHandle<R>) {
    log::info!("hotkey: release");
    unregister_escape(app);
    emit_state(app, OverlayState::Transcribing);
    if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Stop) {
        log::warn!("hotkey: dictation worker unreachable: {e}");
    }
}

const ESC: &str = "Escape";

/// Briefly hijack the Escape key while a dictation is in flight. The
/// shortcut is unregistered the moment the user releases the hotkey or
/// hits Esc, so we never block Esc system-wide outside of recording.
fn register_escape<R: Runtime>(app: &AppHandle<R>) {
    let Ok(esc) = ESC.parse::<Shortcut>() else { return };
    let res = app
        .global_shortcut()
        .on_shortcut(esc, |app: &AppHandle<R>, _sc: &Shortcut, event: ShortcutEvent| {
            if event.state() == ShortcutState::Pressed {
                log::info!("hotkey: cancel (Esc)");
                unregister_escape(app);
                if let Err(e) = app.state::<AppState>().tx.send(DictationCmd::Cancel) {
                    log::warn!("hotkey: cancel send failed: {e}");
                }
            }
        });
    if let Err(e) = res {
        log::debug!("hotkey: esc register failed: {e}");
    }
}

fn unregister_escape<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(esc) = ESC.parse::<Shortcut>() {
        let _ = app.global_shortcut().unregister(esc);
    }
}
