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

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager, Runtime,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

pub enum DictationCmd {
    Start,
    Stop,
}

pub struct AppState {
    pub tx: UnboundedSender<DictationCmd>,
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
            let transcriber: Arc<dyn stt::Transcriber> = match secrets::get(secrets::GROQ_API_KEY) {
                Ok(key) => Arc::new(stt::GroqWhisper::new(key)),
                Err(e) => {
                    log::warn!(
                        "no Groq API key in keyring ({e}). Run `cargo run -- set-key` (or `murmur set-key`) before dictating."
                    );
                    Arc::new(NoopTranscriber)
                }
            };

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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
                        Err(e) => log::warn!("dictation: failed to start recording: {e}"),
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
                        Err(e) => log::warn!("dictation: stop failed: {e}"),
                    }
                }
            }
        }
        log::info!("dictation worker exiting");
    });
    tx
}

fn handle_recording<R: Runtime>(
    _app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
    recording: audio::Recording,
) {
    if recording.duration_ms < 200 {
        log::info!(
            "dictation: discarded short clip ({} ms)",
            recording.duration_ms
        );
        return;
    }
    // CLAUDE.md hard rule #7: silent audio almost always means missing mic
    // permission. Surface it explicitly.
    if recording.mean_abs < 1e-4 {
        log::warn!(
            "dictation: clip is silent (mean|amp|={:.6}). Grant Microphone access in System Settings → Privacy & Security → Microphone, then try again.",
            recording.mean_abs
        );
        return;
    }

    tauri::async_runtime::spawn(async move {
        match transcriber.transcribe(&recording.wav).await {
            Ok(text) if text.is_empty() => {
                log::info!("dictation: empty transcript");
            }
            Ok(text) => {
                log::info!("dictation: transcript ({} chars): {}", text.len(), text);
                if let Err(e) = inject::paste_text(&text) {
                    log::warn!("dictation: paste failed: {e}");
                }
            }
            Err(e) => log::warn!("dictation: transcribe failed: {e}"),
        }
    });
}

struct NoopTranscriber;
impl stt::Transcriber for NoopTranscriber {
    fn transcribe<'a>(&'a self, _wav: &'a [u8]) -> stt::TranscribeFuture<'a> {
        Box::pin(async {
            Err(anyhow::anyhow!(
                "Groq API key not configured. Run `cargo run -- set-key`."
            ))
        })
    }
}
