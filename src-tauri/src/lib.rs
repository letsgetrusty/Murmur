// Module layout per voice-tool-architecture.md §4. lib.rs is the action router:
// hotkey → recorder lifecycle → transcribe → inject. selection / tts / kb land
// in subsequent phases and remain stubs.
mod audio;
mod config;
mod hotkeys;
mod inject;
mod kb;
mod secrets;
mod selection;
mod stt;
mod tts;

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Emitter, Manager, RunEvent, Runtime,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub enum DictationCmd {
    Start,
    Stop,
}

pub struct AppState {
    pub tx: UnboundedSender<DictationCmd>,
}

/// State the overlay renders, emitted by the action router over the
/// `state` Tauri event. Kept narrow so the frontend doesn't need to know
/// about Rust types.
///
/// `Idle` means "render nothing"; the underlying webview window stays
/// `visible` to NSApp so AppKit doesn't terminate the process when there
/// are no on-screen windows. Hiding/showing is done in CSS, not native.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverlayState {
    Idle,
    Recording,
    Transcribing,
    Done { chars: usize },
    Error { message: String },
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // First-run setup: `murmur set-key` stores the Groq API key in Keychain.
    // CLAUDE.md hard rule #6: secrets never live in config files or source.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("set-key") {
        return run_set_key();
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .setup(|app| {
            // GroqWhisper reads the API key from Keychain on each transcribe,
            // so the user can run `set-key` without restarting the app.
            let transcriber: Arc<dyn stt::Transcriber> = Arc::new(stt::GroqWhisper::new());

            // The cpal Stream is !Send on macOS; keep it owned by a dedicated
            // worker thread so we never carry it across the global-shortcut
            // callback boundary.
            let tx = spawn_dictation_worker(app.handle().clone(), transcriber);
            app.manage(AppState { tx });

            // Tray
            let quit = MenuItem::with_id(app, "quit", "Quit murmur", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&quit])?;
            TrayIconBuilder::with_id("main")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| {
                    if event.id.as_ref() == "quit" {
                        app.exit(0);
                    }
                })
                .build(app)?;

            // Pre-position the overlay; keep hidden until the hotkey fires.
            if let Some(win) = app.get_webview_window("overlay") {
                let _ = win.set_always_on_top(true);
                let _ = win.set_skip_taskbar(true);
            }

            // Hotkey registration MUST happen on the main thread on macOS —
            // CLAUDE.md hard rule #1. `setup` runs on the main thread.
            hotkeys::register(app.handle())?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // Keep the app alive when the overlay hides. Tauri's default is to
        // exit when the last visible window goes away — but our overlay is
        // a transient pill that's hidden most of the time, with the tray icon
        // as the persistent UI. Only exit when the tray Quit menu item calls
        // `app.exit(N)` (which surfaces here as `code = Some(N)`).
        .run(|_app, event| {
            if let RunEvent::ExitRequested { code, api, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}

pub fn emit_state<R: Runtime>(app: &AppHandle<R>, state: OverlayState) {
    if let Some(win) = app.get_webview_window("overlay") {
        if let Err(e) = win.emit("state", state) {
            log::debug!("emit overlay state failed: {e}");
        }
    }
}

/// Show the overlay window. After the first call, the window stays
/// `visible` for the rest of the session — we never call `hide()`,
/// because AppKit terminates the process when its last visible window
/// goes away (LSUIElement does not exempt us from that). Subsequent
/// "hides" are done by emitting `OverlayState::Idle`, which the
/// frontend renders as nothing.
pub fn show_overlay<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("overlay") {
        let _ = win.set_always_on_top(true);
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.show();
    }
}

/// Tell the overlay frontend to render nothing after a dwell. Uses a
/// std::thread so we don't block any tokio worker.
fn idle_after<R: Runtime>(app: AppHandle<R>, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        emit_state(&app, OverlayState::Idle);
    });
}

fn run_set_key() {
    use std::io::{self, Write};
    eprint!("Enter Groq API key: ");
    let _ = io::stderr().flush();
    let mut key = String::new();
    if let Err(e) = io::stdin().read_line(&mut key) {
        eprintln!("read failed: {e}");
        return;
    }
    let key = key.trim();
    if key.is_empty() {
        eprintln!("empty key, aborting");
        return;
    }
    match secrets::set(secrets::GROQ_API_KEY, key) {
        Ok(()) => eprintln!("saved to Keychain."),
        Err(e) => eprintln!("save failed: {e}"),
    }
}

fn spawn_dictation_worker<R: Runtime>(
    app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
) -> UnboundedSender<DictationCmd> {
    let (tx, mut rx) = unbounded_channel::<DictationCmd>();
    std::thread::spawn(move || {
        let mut rec: Option<audio::Recorder> = None;
        while let Some(cmd) = rx.blocking_recv() {
            match cmd {
                DictationCmd::Start => {
                    if rec.is_some() {
                        log::debug!("dictation: already recording, ignoring Start");
                        continue;
                    }
                    match audio::Recorder::start() {
                        Ok(r) => {
                            log::info!("dictation: recording started");
                            rec = Some(r);
                        }
                        Err(e) => {
                            log::warn!("dictation: failed to start recording: {e}");
                            emit_state(
                                &app,
                                OverlayState::Error {
                                    message: format!("mic error: {e}"),
                                },
                            );
                            idle_after(app.clone(), Duration::from_millis(2200));
                        }
                    }
                }
                DictationCmd::Stop => {
                    let Some(r) = rec.take() else {
                        log::debug!("dictation: Stop without active recording");
                        continue;
                    };
                    match r.stop() {
                        Ok(recording) => {
                            handle_recording(app.clone(), transcriber.clone(), recording);
                        }
                        Err(e) => {
                            log::warn!("dictation: stop failed: {e}");
                            emit_state(
                                &app,
                                OverlayState::Error {
                                    message: format!("audio stop failed: {e}"),
                                },
                            );
                            idle_after(app.clone(), Duration::from_millis(2000));
                        }
                    }
                }
            }
        }
        log::info!("dictation worker exiting");
    });
    tx
}

fn handle_recording<R: Runtime>(
    app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
    recording: audio::Recording,
) {
    if recording.duration_ms < 200 {
        log::info!(
            "dictation: discarded short clip ({} ms)",
            recording.duration_ms
        );
        emit_state(&app, OverlayState::Done { chars: 0 });
        idle_after(app, Duration::from_millis(250));
        return;
    }
    // CLAUDE.md hard rule #7: silent audio almost always means missing mic
    // permission. Surface it explicitly.
    if recording.mean_abs < 1e-4 {
        log::warn!(
            "dictation: clip is silent (mean|amp|={:.6}). Grant Microphone access in System Settings → Privacy & Security → Microphone, then try again.",
            recording.mean_abs
        );
        emit_state(
            &app,
            OverlayState::Error {
                message: "no mic input — grant Microphone access".into(),
            },
        );
        idle_after(app, Duration::from_millis(2500));
        return;
    }

    tauri::async_runtime::spawn(async move {
        let (state, dwell_ms) = match transcriber.transcribe(&recording.wav).await {
            Ok(text) if text.is_empty() => {
                log::info!("dictation: empty transcript");
                (OverlayState::Done { chars: 0 }, 400)
            }
            Ok(text) => {
                let chars = text.chars().count();
                log::info!("dictation: transcript ({} chars): {}", chars, text);
                // `inject::paste_text` calls enigo, which posts CGEvents via
                // Quartz. CGEventPost requires a CFRunLoop on the calling
                // thread — tokio workers don't have one, and calling from a
                // bare worker exits the process silently. Run on the main
                // thread (NSApp's run loop) instead.
                let result = paste_on_main_thread(&app, text).await;
                match result {
                    Ok(()) => (OverlayState::Done { chars }, 600),
                    Err(e) => {
                        log::warn!("dictation: paste failed: {e}");
                        (
                            OverlayState::Error {
                                message: format!("paste failed: {e}"),
                            },
                            2500,
                        )
                    }
                }
            }
            Err(e) => {
                log::warn!("dictation: transcribe failed: {e}");
                (
                    OverlayState::Error {
                        message: e.to_string(),
                    },
                    3000,
                )
            }
        };
        emit_state(&app, state);
        idle_after(app, Duration::from_millis(dwell_ms));
    });
}

async fn paste_on_main_thread<R: Runtime>(app: &AppHandle<R>, text: String) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(inject::paste_text(&text));
    })
    .map_err(|e| anyhow::anyhow!("dispatch to main thread failed: {e}"))?;
    rx.await
        .map_err(|e| anyhow::anyhow!("paste task cancelled: {e}"))?
}
