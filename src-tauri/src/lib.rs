// Module layout per docs/voice-tool-architecture.md §4. lib.rs is the action router:
// hotkey → recorder lifecycle → transcribe → inject.
mod audio;
mod commands;
mod config;
mod download;
mod fn_key;
mod focus;
mod history;
mod hotkeys;
mod inject;
mod ipc;
mod llm;
mod local_llm;
mod permissions;
mod selection;
mod sound;
mod stt;
mod tts;
mod update;
mod usage;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::tts::Speaker as _;

use serde::Serialize;
use tauri::{
    menu::{CheckMenuItem, IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu},
    AppHandle, Emitter, Manager, RunEvent, Runtime, WebviewUrl, WebviewWindowBuilder, Wry,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// What to do with a committed recording. Decided at release (not press) so the
/// refine modifier can be pressed before or after Fn, and so the same recorder
/// lifecycle serves plain and refined dictation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictationMode {
    /// Paste the transcript verbatim.
    Plain,
    /// Run the transcript through the LLM refiner, then paste (Fn+Ctrl).
    Refine,
    /// Onboarding "Try it": transcribe and report the text back to the
    /// onboarding window instead of pasting — no refine, overlay, or history.
    Test,
}

pub enum DictationCmd {
    Start,
    /// Commit the in-flight recording and process it per `mode`.
    Stop {
        mode: DictationMode,
    },
    /// User pressed Esc — drop the in-flight recorder and don't transcribe.
    Cancel,
}

pub struct AppState {
    pub tx: UnboundedSender<DictationCmd>,
    pub speaker: Arc<dyn tts::Speaker>,
    /// The STT backend, shared with the dictation worker. Held here too so the
    /// hotkey press can `warm()` it — loading the model while the user speaks so
    /// transcribe doesn't stall on the load when they release.
    pub transcriber: Arc<dyn stt::Transcriber>,
    /// Tray menu checkmarks for speed, in the same order as `tts::SPEEDS`.
    pub speed_items: Vec<CheckMenuItem<Wry>>,
    /// Tray menu checkmarks for voice, in the same order as
    /// `tts::voices_for(tts_provider)` (the active backend's voice list).
    pub voice_items: Vec<CheckMenuItem<Wry>>,
    /// Tray menu checkmarks for the microphone picker. First entry is the
    /// system default; rest mirror `mic_names`.
    pub mic_items: Vec<CheckMenuItem<Wry>>,
    /// cpal device names parallel to `mic_items[1..]`. Index 0 of mic_items
    /// is "Default" (no name).
    pub mic_names: Vec<String>,
    /// Current mic selection. `None` = system default.
    pub mic_name: Mutex<Option<String>>,
    /// Live config, shared with the refiner and the settings-window IPC
    /// commands so edits apply without a restart.
    pub config: Arc<Mutex<config::Config>>,
    /// Cumulative refinement token/cost totals, shared with the refiner.
    pub usage: Arc<Mutex<usage::UsageStats>>,
    /// Dictation history database (SQLite). `Connection` is `!Sync`, so it's
    /// behind a Mutex; access is infrequent (once per dictation / UI action).
    pub history: Arc<Mutex<rusqlite::Connection>>,
    /// A newer release, downloaded + verified in the background and held until
    /// the user restarts. `Some` once staged; drives the "Restart to update"
    /// tray item and settings banner.
    pub pending_update: Arc<Mutex<Option<update::StagedUpdate>>>,
    /// The tray item that flips from "Check for Updates…" to
    /// "Restart to update (vX)" when an update is staged.
    pub update_item: tauri::menu::MenuItem<Wry>,
    /// The "Read selection (⌘⇧R)" tray item. Held so its shortcut hint can be
    /// relabeled when the read-aloud chord is rebound in Settings — the tray
    /// menu is built once at startup, so this is the only live handle to it.
    pub read_item: tauri::menu::MenuItem<Wry>,
    /// Latest per-model download progress, mirrored from `emit_download_progress`
    /// so the overlay can show a live bar when the user tries to dictate before
    /// the speech model has finished downloading.
    pub downloads: Arc<Mutex<Downloads>>,
    /// True while a dictation press actually started a recording, so the matching
    /// release only transcribes when a recording is in flight — guards the
    /// model-download gate and the race where the model becomes ready mid-hold.
    pub recording_armed: AtomicBool,
    /// True while the onboarding "Try dictation" step is active. A real
    /// dictation-trigger press then records a throwaway clip and reports the
    /// transcript to the onboarding window (no overlay/paste) instead of
    /// dictating — see `hotkeys::on_press` and `handle_recording`'s `Test` branch.
    pub onboarding_test: AtomicBool,
    /// True while the onboarding "Try read-aloud" step is active. A read-aloud
    /// key press then speaks a fixed sample (no selection capture / overlay) and
    /// reports progress to the onboarding window — see `tts_toggle`.
    pub onboarding_read_test: AtomicBool,
    /// True while the "waiting for the speech model" overlay watcher is running,
    /// so a second press doesn't spawn a duplicate.
    pub model_wait_active: AtomicBool,
    /// Monotonic counter identifying the in-flight dictation. Bumped when the
    /// user presses Esc during the transcribe/refine phase; the async pipeline
    /// captures it at the start and, on each hop (after transcribe, before
    /// refine, before paste), bails if it no longer matches — so an Esc drops
    /// the result instead of pasting it.
    pub dictation_gen: AtomicU64,
    /// Registered start/stop dictation cue sounds (see `sound.rs`). `Copy`, so no
    /// locking; playback is gated on the `dictation_sound` config flag.
    pub cues: sound::Cues,
    /// (finish time, focused-window center) of the previous dictation. A follow-up
    /// gets a separating space only if it lands within
    /// `DICTATION_CONTINUATION_WINDOW` AND into the same window — transcripts are
    /// trimmed (`stt::strip_nonspeech`), so without this, re-engaging Fn mid-thought
    /// pastes "One.Two.". Keying on the window (not just time) avoids a stray
    /// leading space when the follow-up is a different app/field.
    pub last_dictation: Mutex<Option<(std::time::Instant, (f64, f64))>>,
}

/// How long after a dictation a follow-up still counts as continuing the same
/// thought. Paired with the same-window check below.
const DICTATION_CONTINUATION_WINDOW: Duration = Duration::from_secs(10);

/// Whether two focused-window centers are the "same window" — their centers
/// coincide within a couple points (the window doesn't move as text is inserted,
/// so an unchanged center means we're still typing into the same window).
fn same_window(a: (f64, f64), b: (f64, f64)) -> bool {
    (a.0 - b.0).abs() < 2.0 && (a.1 - b.1).abs() < 2.0
}

/// Latest download progress for one model, mirrored from the download tasks.
#[derive(Default, Clone, Copy)]
pub struct DlProgress {
    pub downloaded: u64,
    pub total: u64,
    pub failed: bool,
    /// A download task is currently running for this model. Guards against a retry
    /// spawning a second task that would truncate the same `.part` file.
    pub in_flight: bool,
}

/// Latest download progress per model, for the overlay's pre-download dictation
/// message. Updated by `emit_download_progress` / `emit_download_error`.
#[derive(Default)]
pub struct Downloads {
    pub whisper: DlProgress,
    pub llm: DlProgress,
    pub kokoro: DlProgress,
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
    /// A required model (speech-to-text) is still downloading, so dictation can't
    /// run yet. `downloaded`/`total` fill the overlay's progress bar; total is 0
    /// when the server sent no Content-Length.
    Preparing {
        downloaded: u64,
        total: u64,
    },
    Recording,
    Transcribing,
    /// Fn+Ctrl only: the transcript is being cleaned up by the LLM.
    Refining,
    /// Read-aloud in progress; `progress` is the fraction [0.0, 1.0] spoken,
    /// used to fill the overlay pill.
    Reading {
        progress: f32,
    },
    Done {
        chars: usize,
    },
    Error {
        message: String,
    },
}

/// Tee log output to both stderr and a file. When the app is launched as a
/// bundle via `open` (the dev workflow), stderr is discarded — the file is the
/// only way to see logs. stderr stays useful for `cargo run` from a terminal.
struct Tee(std::fs::File);
impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), buf);
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::Write::flush(&mut std::io::stderr());
        self.0.flush()
    }
}

/// `~/Library/Logs/murmur.log`, truncated each launch so it stays readable.
fn open_log_file() -> Option<std::fs::File> {
    let mut path = std::path::PathBuf::from(std::env::var_os("HOME")?);
    path.push("Library/Logs/murmur.log");
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .ok()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    if let Some(file) = open_log_file() {
        builder.target(env_logger::Target::Pipe(Box::new(Tee(file))));
    }
    builder.init();

    // Enumerate cpal input devices for the tray mic picker BEFORE Tauri/NSApp
    // takes over the main thread. Calling into CoreAudio HAL from inside the
    // NSApplicationDidFinishLaunching notification handler segfaults the release
    // build (HALDeviceList::GetData on a not-yet-ready audio subsystem), so we
    // query from the bare process at startup.
    //
    // But only once the microphone permission already exists: on macOS, touching
    // CoreAudio input enumeration raises the TCC mic prompt out of context ("as
    // soon as the app opens"), preempting the in-context grant onboarding's
    // Enable button (AVCaptureDevice.requestAccess) is meant to drive — and a
    // grant obtained that way leaves AVCaptureDevice's authorization status
    // stale, so onboarding would still read "not enabled". When not yet
    // authorized we start with an empty picker; it populates on the next launch
    // after the grant.
    let mic_names = if permissions::microphone_status() == 3 {
        audio::list_input_devices()
    } else {
        Vec::new()
    };
    let cfg = config::load();

    tauri::Builder::default()
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::save_config,
            commands::get_options,
            commands::set_speed,
            commands::set_voice,
            commands::preview_voice,
            commands::set_mic,
            commands::set_hotkey,
            commands::reset_hotkeys,
            commands::set_refine_modifier,
            commands::set_dictation_trigger,
            commands::download_neural_voice,
            commands::retry_download,
            commands::get_usage,
            commands::list_history,
            commands::delete_history,
            commands::clear_history,
            commands::history_stats,
            commands::copy_text,
            commands::relaunch_app,
            commands::app_version,
            commands::open_url,
            commands::onboarding_status,
            commands::open_accessibility_settings,
            commands::open_microphone_settings,
            commands::request_microphone,
            commands::finish_onboarding,
            commands::close_onboarding,
            commands::set_overlay_position,
            commands::set_onboarding_test,
            commands::suspend_shortcuts,
            commands::resume_shortcuts,
            commands::pending_update_version,
            commands::install_staged_update,
        ])
        .setup(move |app| {
            // Install the dev-term pronunciation lexicon before any Kokoro synth
            // (preview pre-gen / warm below), since kokoro-en reads the override
            // env var lazily on first phoneme lookup.
            tts::install_g2p_lexicon();
            // Pin the CoreML compute path (before the session is built) so synth
            // speed stays consistent instead of swinging when the ANE drops us.
            tts::pin_coreml_compute_units();

            // Shared live config: the refiner reads it on each refine and the
            // settings window edits it via IPC, so changes apply without a restart.
            let config_state = Arc::new(Mutex::new(cfg.clone()));

            // Speech-to-text: local on-device Whisper (whisper-rs).
            // Speech-to-text: local on-device Whisper (whisper-rs). The model is
            // fetched in the background after AppState is managed (see the
            // spawn_download calls below), so the first dictation isn't blocked on
            // a ~0.5 GB download; WhisperStt loads it lazily on first transcribe.
            let transcriber: Arc<dyn stt::Transcriber> = {
                log::info!("stt: local Whisper backend (model '{}')", cfg.stt_model);
                Arc::new(stt::WhisperStt::new(cfg.stt_model.clone()))
            };

            // Cumulative refinement usage, persisted across restarts.
            let usage_state = Arc::new(Mutex::new(usage::load()));

            // Dictation history database.
            let history_state = Arc::new(Mutex::new(history::open()));

            // LLM for the Fn+Ctrl refine pass: the embedded llama.cpp engine
            // (Qwen3), loaded on first use.
            // LLM for the Fn+Ctrl refine pass (Qwen3 via embedded llama.cpp),
            // loaded on first use. The ~1 GB model is fetched in the background
            // after AppState is managed (spawn_download below).
            let chat: Arc<dyn llm::LlmChat> = {
                if local_llm::assets_present(&cfg.llm_model) {
                    log::info!("llm: local backend (model '{}')", cfg.llm_model);
                } else {
                    log::info!("llm: local backend; model missing — will download");
                }
                let engine = Arc::new(local_llm::LocalLlm::new(
                    local_llm::model_path(&cfg.llm_model).unwrap_or_default(),
                ));
                Arc::new(llm::LocalChat::new(engine))
            };

            // The cpal Stream is !Send on macOS; keep it owned by a dedicated
            // worker thread so we never carry it across the global-shortcut
            // callback boundary.
            let tx = spawn_dictation_worker(app.handle().clone(), transcriber.clone(), chat);

            // Text-to-speech (both on-device): "kokoro" local neural, or the
            // native macOS AVSpeechSynthesizer (default).
            let speaker: Arc<dyn tts::Speaker> = match cfg.tts_provider.as_str() {
                "kokoro" => {
                    if tts::kokoro_assets_present() {
                        log::info!("tts: Kokoro local neural backend");
                    } else {
                        log::info!("tts: Kokoro backend; assets missing — downloading…");
                    }
                    // The ~310 MB model + voices are fetched in the background
                    // after AppState is managed (spawn_download below), gated on
                    // `onboarding_done` so opting out on first run never triggers
                    // the download and the two paths can't race on the same files.
                    let s = tts::KokoroSpeaker::new(
                        tts::kokoro_model_path().unwrap_or_default(),
                        tts::kokoro_voices_dir().unwrap_or_default(),
                    );
                    s.set_speed(cfg.tts_speed);
                    // Honor the saved voice if it's a valid Kokoro voice; a stale
                    // id from another provider is ignored (keeps the default).
                    s.set_voice(&cfg.tts_voice_id);
                    // Render + cache each voice's preview in the background so
                    // switching voices in Settings is instant (no-op if cached).
                    s.pregenerate_previews();
                    Arc::new(s)
                }
                _ => {
                    log::info!("tts: native macOS AVSpeechSynthesizer backend");
                    Arc::new(tts::MacSpeaker::new())
                }
            };

            // Build the tray menu with CheckMenuItems initialised from the
            // saved config so the user sees their previous selection as soon
            // as they open the menu. `mic_names` was enumerated above before
            // Tauri took the main thread.
            let (menu, speed_items, voice_items, mic_items, update_item, read_item) =
                build_tray_menu(app.handle(), &cfg, &mic_names)?;
            app.manage(AppState {
                tx,
                speaker,
                transcriber,
                speed_items,
                voice_items,
                mic_items,
                mic_names,
                mic_name: Mutex::new(cfg.mic_name.clone()),
                config: config_state.clone(),
                usage: usage_state.clone(),
                history: history_state.clone(),
                pending_update: Arc::new(Mutex::new(None)),
                update_item,
                read_item,
                downloads: Arc::new(Mutex::new(Downloads::default())),
                recording_armed: AtomicBool::new(false),
                onboarding_test: AtomicBool::new(false),
                onboarding_read_test: AtomicBool::new(false),
                model_wait_active: AtomicBool::new(false),
                dictation_gen: AtomicU64::new(0),
                cues: sound::Cues::load(),
                last_dictation: Mutex::new(None),
            });

            // Clean up orphaned `.part` temps from interrupted downloads of models
            // that are no longer selected. Keep the ones for the currently
            // configured models so their downloads resume instead of restarting.
            {
                use std::collections::HashSet;
                let mut keep: HashSet<std::path::PathBuf> = HashSet::new();
                let part_of = |p: std::path::PathBuf| p.with_extension("part");
                if let Ok(p) = stt::model_path(&cfg.stt_model) {
                    keep.insert(part_of(p));
                }
                if let Ok(p) = local_llm::model_path(&cfg.llm_model) {
                    keep.insert(part_of(p));
                }
                if cfg.tts_provider == "kokoro" {
                    if let Ok(p) = tts::kokoro_model_path() {
                        keep.insert(part_of(p));
                    }
                    if let Ok(dir) = tts::kokoro_voices_dir() {
                        for (id, _) in tts::KOKORO_VOICES {
                            keep.insert(dir.join(format!("{id}.part")));
                        }
                    }
                }
                let dirs: Vec<std::path::PathBuf> = [stt::models_dir(), tts::kokoro_voices_dir()]
                    .into_iter()
                    .flatten()
                    .collect();
                download::sweep_stale_parts(&dirs, &keep);
            }

            // Prefetch the models in the background so the first dictation/refine/
            // read-aloud isn't blocked on a download. These route through
            // spawn_download (guarded + idempotent), which is also the retry path,
            // and re-attempt any model still missing from a prior failed launch.
            // Kokoro is gated on onboarding so opting out never fetches it.
            spawn_download(app.handle(), ipc::download::WHISPER);
            spawn_download(app.handle(), ipc::download::LLM);
            if cfg.onboarding_done && cfg.tts_provider == "kokoro" {
                spawn_download(app.handle(), ipc::download::KOKORO);
            }

            // Deliberately DON'T warm Whisper at startup. Warming loads its
            // weights + compiles the Metal graph and pins that resident before
            // the user has done anything — it's what doubled idle memory
            // (~700 MB → ~1.5 GB) and rubs against AGENTS.md "keep idle memory
            // low." `hotkeys::on_press` already warms it on the dictation
            // keypress, overlapping the load with the seconds the user spends
            // speaking, so the first real dictation is still warm by release —
            // we just no longer pay for it while idle.
            //
            // Kokoro is the exception: read-aloud speaks immediately on the
            // keypress with no hold to hide a cold load behind, so warm it up
            // front — but only when the user has actually opted into it (~310 MB).
            if cfg.onboarding_done && cfg.tts_provider == "kokoro" && tts::kokoro_assets_present() {
                app.state::<AppState>().speaker.warm();
            }

            // The tray icon itself is auto-created from the `trayIcon` block
            // in `tauri.conf.json`. Attaching the menu to that single
            // instance avoids the duplicate-slot bug we hit when building a
            // second TrayIcon in setup.
            let tray = app
                .tray_by_id("main")
                .ok_or_else(|| anyhow::anyhow!("tray 'main' not found — check tauri.conf.json"))?;
            tray.set_menu(Some(menu))?;
            tray.on_menu_event(handle_tray_event);

            // Pre-position the overlay at the configured anchor of the primary
            // monitor; keep hidden until the hotkey fires.
            if let Some(win) = app.get_webview_window("overlay") {
                let _ = win.set_always_on_top(true);
                let _ = win.set_skip_taskbar(true);
                // Show on every Space/desktop, so the pill follows the user
                // instead of staying on the Space where it was created.
                let _ = win.set_visible_on_all_workspaces(true);
                position_overlay(&win, &cfg.overlay_position);
            }

            // The main (settings/history) window is created lazily on first
            // open (tray item / dock reopen) and destroyed on close, so an
            // idle Murmur carries no WebKit content process for a window the
            // user may never open. See `show_main_window`.

            // Hotkey registration MUST happen on the main thread on macOS —
            // CLAUDE.md hard rule #1. `setup` runs on the main thread. Chords
            // come from config so the settings window can rebind them.
            hotkeys::register(app.handle(), &cfg)?;

            // Install the Fn-key tap onto the main thread's CFRunLoop (same
            // run loop NSApp drives). Failure here only means "no Fn yet" —
            // the chord still works.
            fn_key::install(app.handle().clone())?;

            // First run: walk the user through Accessibility, microphone, and
            // the model downloads. Shown until they finish onboarding.
            if !cfg.onboarding_done {
                show_onboarding_window(app.handle());
            }

            // Background auto-update: check on launch and hourly, and when a
            // newer signed release exists, download + verify it silently and
            // stage it. Staging flips the tray item to "Restart to update (vX)"
            // and shows the settings banner — the user applies it when they
            // choose (no surprise relaunches). See `mark_update_staged`.
            {
                let app = app.handle().clone();
                tauri::async_runtime::spawn(async move {
                    const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);
                    loop {
                        let staged = app
                            .state::<AppState>()
                            .pending_update
                            .lock()
                            .map(|g| g.is_some())
                            .unwrap_or(true);
                        if !staged {
                            if let Some(update) = update::check_and_download(&app).await {
                                let info = update.info();
                                if let Ok(mut g) = app.state::<AppState>().pending_update.lock() {
                                    *g = Some(update);
                                }
                                mark_update_staged(&app, info);
                            }
                        }
                        tokio::time::sleep(CHECK_INTERVAL).await;
                    }
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // Keep the app alive when the overlay hides. Tauri's default is to
        // exit when the last visible window goes away — but our overlay is
        // a transient pill that's hidden most of the time, with the tray icon
        // as the persistent UI. Only exit when the tray Quit menu item calls
        // `app.exit(N)` (which surfaces here as `code = Some(N)`).
        .run(|app, event| match event {
            RunEvent::ExitRequested { code, api, .. } => {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
            // macOS: clicking the dock icon (with no visible windows) fires
            // Reopen — bring the settings window up.
            RunEvent::Reopen { .. } => show_main_window(app),
            _ => {}
        });
}

/// Show the main settings/history window, creating it if needed. Used by the
/// dock-reopen handler and the tray "Settings…" item.
///
/// The window is not declared in `tauri.conf.json`; it's built here on first
/// open and destroyed when the user closes it (Tauri's default), so an idle
/// Murmur keeps no WebKit content process for a window that's rarely opened.
/// It self-populates from the backend on load (`settings.js` `init`), so a
/// fresh instance needs no restored state.
fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }
    match WebviewWindowBuilder::new(app, "main", WebviewUrl::App("settings.html".into()))
        .title("Murmur")
        .inner_size(900.0, 640.0)
        .min_inner_size(640.0, 460.0)
        // Cap the width so the content column (max 820px, see settings.css .tab)
        // always fills the pane — past this the window would just grow empty
        // margins. 1060 = 180 sidebar + 64 padding + ~816 content. Height is left
        // effectively unbounded.
        .max_inner_size(1060.0, 10000.0)
        .center()
        // Paint the window + webview our dark UI background (tokens.css --bg
        // #141210) from creation, so it doesn't flash white before the CSS loads.
        .background_color(tauri::window::Color(20, 18, 16, 255))
        .build()
    {
        Ok(win) => {
            let _ = win.set_focus();
        }
        Err(e) => log::warn!("failed to create settings window: {e}"),
    }
}

/// Show the first-run onboarding window, creating it if needed. Auto-shown at
/// startup while `config.onboarding_done` is false, and re-openable from the
/// tray. Built here (not in `tauri.conf.json`) like the settings window.
pub fn show_onboarding_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("onboarding") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return;
    }
    match WebviewWindowBuilder::new(app, "onboarding", WebviewUrl::App("onboarding.html".into()))
        .title("Welcome to Murmur")
        .inner_size(640.0, 620.0)
        .resizable(false)
        .center()
        // Dark background from creation so there's no white flash before CSS.
        .background_color(tauri::window::Color(20, 18, 16, 255))
        .build()
    {
        Ok(win) => {
            let _ = win.set_focus();
        }
        Err(e) => log::warn!("failed to create onboarding window: {e}"),
    }
}

extern "C" {
    /// POSIX immediate-termination wrapper: never returns, runs no `atexit`
    /// handlers or C/C++ static destructors, touches no user-space state.
    fn _exit(code: std::ffi::c_int) -> !;
    /// Register a C `atexit` handler (runs during `exit()` in LIFO order).
    fn atexit(cb: extern "C" fn()) -> std::ffi::c_int;
}

/// Terminate immediately via `_exit`, skipping static destructors. We do this
/// instead of a clean exit because whisper's ggml Metal backend registers a
/// global `ggml_metal_device` whose destructor (`ggml_metal_device_free` →
/// `ggml_metal_rsets_free`) hits `ggml_abort()` during `__cxa_finalize` at exit,
/// crashing with SIGABRT. Nothing we own needs orderly teardown (config saves on
/// change, logs flush per write), so skipping finalization is safe.
fn hard_exit(code: std::ffi::c_int) -> ! {
    // SAFETY: see the `_exit` extern docs.
    unsafe { _exit(code) }
}

/// `atexit` guard: catches clean-exit paths our explicit `hard_exit` sites don't
/// — Cmd+Q, the AppleEvent quit (`osascript`/`dev.sh`), and system logout, which
/// reach AppKit's `exit()` directly. `atexit` runs handlers LIFO, so as long as
/// this is registered *after* ggml's Metal backend inits (see `install_exit_guard`),
/// it fires before ggml's aborting destructor and `_exit`s clean.
extern "C" fn exit_guard() {
    // SAFETY: see the `_exit` extern docs.
    unsafe { _exit(0) }
}

/// Register [`exit_guard`]. Idempotent. Must be called *after* whisper's ggml
/// Metal backend is created (see `stt::open_context`) so it lands later than
/// ggml's destructor in `atexit`'s LIFO order and therefore runs first. An
/// earlier (e.g. startup) registration loses that race intermittently.
pub(crate) fn install_exit_guard() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: `atexit` takes a plain `extern "C" fn()`; registration is safe.
        unsafe { atexit(exit_guard) };
    });
}

/// Relaunch Murmur as its own responsible process. We deliberately do NOT
/// use `app.restart()`: on macOS Tauri relaunches by spawning the binary as a
/// *child* process, and macOS then attributes the TCC responsible process to the
/// parent chain — which wedges the Fn-key tap / Accessibility grant (the exact
/// failure dev.sh's `open` launch avoids). So we relaunch through LaunchServices
/// (`open`), matching dev.sh, so the grant survives. Used by the settings
/// "Relaunch" button and after an auto-update installs.
pub fn relaunch<R: Runtime>(app: &AppHandle<R>) {
    if let Ok(exe) = std::env::current_exe() {
        // exe = <Murmur.app>/Contents/MacOS/<bin>; walk up to the .app bundle.
        if let Some(bundle) = exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
        {
            let pid = std::process::id();
            // Detached helper: wait for us to fully exit, then `open` the bundle
            // (a fresh LaunchServices launch → correct responsible process).
            let cmd = format!(
                "while kill -0 {pid} 2>/dev/null; do sleep 0.1; done; open '{}'",
                bundle.display()
            );
            let _ = std::process::Command::new("sh").arg("-c").arg(cmd).spawn();
            // `_exit` (not `app.exit`) so ggml's Metal static destructor doesn't
            // abort during finalization — see `hard_exit`. The helper above is
            // already waiting for this pid to die, then reopens the bundle.
            hard_exit(0);
        }
    }
    // Not inside an .app bundle (unusual) — fall back to Tauri's restart.
    app.restart();
}

/// Take the staged update out of `AppState` and install it, then relaunch.
/// Used by both the tray "Restart to update" item and the settings banner.
pub async fn install_pending<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let staged = app
        .state::<AppState>()
        .pending_update
        .lock()
        .ok()
        .and_then(|mut g| g.take());
    match staged {
        Some(u) => update::install_staged(app, u).await,
        None => Err("no staged update".to_string()),
    }
}

/// Relabel the "Read selection (…)" tray item so its shortcut hint matches the
/// current read-aloud chord. Called after the chord is rebound in Settings — the
/// tray menu is built once at startup, so without this it would stay stale. Menu
/// mutation must happen on the main thread on macOS, so it's deferred there.
pub(crate) fn refresh_read_label<R: Runtime>(app: &AppHandle<R>, hotkey_tts: &str) {
    let label = format!("Read selection ({})", format_accelerator(hotkey_tts));
    let app_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        let _ = app_main.state::<AppState>().read_item.set_text(&label);
    });
}

/// Surface a staged update: relabel the tray item to "Restart to update (vX)"
/// and notify the settings window (which shows an install banner). Menu mutation
/// must happen on the main thread on macOS, so the relabel is deferred there.
fn mark_update_staged<R: Runtime>(app: &AppHandle<R>, info: update::UpdateInfo) {
    let version = info.version.clone();
    let label = format!("Restart to update (v{version})");
    let app_main = app.clone();
    let _ = app.run_on_main_thread(move || {
        let _ = app_main.state::<AppState>().update_item.set_text(&label);
    });
    let _ = app.emit(ipc::event::UPDATE_STAGED, info);
    log::info!("update: staged v{version} — 'Restart to update' offered");
}

/// Progress payload for the onboarding download bars. `total` is 0 when the
/// server didn't send a Content-Length. `failed` marks a download that errored.
#[derive(Clone, Serialize)]
struct DownloadProgress {
    id: &'static str,
    downloaded: u64,
    total: u64,
    failed: bool,
}

/// The tracker slot for a model id, or `None` for an unknown id.
fn dl_slot<'a>(d: &'a mut Downloads, id: &str) -> Option<&'a mut DlProgress> {
    if id == ipc::download::WHISPER {
        Some(&mut d.whisper)
    } else if id == ipc::download::LLM {
        Some(&mut d.llm)
    } else if id == ipc::download::KOKORO {
        Some(&mut d.kokoro)
    } else {
        None
    }
}

/// Update the progress fields of the shared tracker for `id`, leaving `in_flight`
/// (owned by spawn_download) untouched. No-ops if AppState isn't managed yet.
fn track_download<R: Runtime>(
    app: &AppHandle<R>,
    id: &'static str,
    downloaded: u64,
    total: u64,
    failed: bool,
) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Ok(mut d) = state.downloads.lock() {
            if let Some(slot) = dl_slot(&mut d, id) {
                slot.downloaded = downloaded;
                slot.total = total;
                slot.failed = failed;
            }
        }
    }
}

pub(crate) fn emit_download_progress<R: Runtime>(
    app: &AppHandle<R>,
    id: &'static str,
    downloaded: u64,
    total: u64,
) {
    track_download(app, id, downloaded, total, false);
    let _ = app.emit(
        ipc::event::MODEL_DOWNLOAD,
        DownloadProgress {
            id,
            downloaded,
            total,
            failed: false,
        },
    );
}

pub(crate) fn emit_download_error<R: Runtime>(app: &AppHandle<R>, id: &'static str) {
    track_download(app, id, 0, 0, true);
    let _ = app.emit(
        ipc::event::MODEL_DOWNLOAD,
        DownloadProgress {
            id,
            downloaded: 0,
            total: 0,
            failed: true,
        },
    );
}

/// Start (or restart) a model download in the background, emitting progress on the
/// `model-download` event. Idempotent and self-guarding: a no-op if that model is
/// already downloading, so it doubles as the retry entry point (onboarding retry
/// button, the dictation/refine gates, and startup prefetch all route through it).
/// AppState must be managed before this is called.
pub(crate) fn spawn_download<R: Runtime>(app: &AppHandle<R>, id: &'static str) {
    // Claim the slot: bail if a task is already running, else mark in-flight and
    // clear any prior progress/failure so the UI resets cleanly.
    {
        let state = app.state::<AppState>();
        let Ok(mut d) = state.downloads.lock() else {
            return;
        };
        let Some(slot) = dl_slot(&mut d, id) else {
            return;
        };
        if slot.in_flight {
            return;
        }
        *slot = DlProgress {
            in_flight: true,
            ..Default::default()
        };
    }
    // Model names come from live config so a retry after a settings change fetches
    // the right file.
    let (stt_model, llm_model) = app
        .state::<AppState>()
        .config
        .lock()
        .map(|c| (c.stt_model.clone(), c.llm_model.clone()))
        .unwrap_or_default();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let emit = |downloaded, total| emit_download_progress(&app, id, downloaded, total);
        // Was the asset already on disk before this task? `ensure_*` returns Ok
        // instantly for a present model, so warming unconditionally in the Ok
        // branch below would fire on EVERY launch — loading the model + compiling
        // its graph and pinning it resident, which is what doubled idle memory.
        // Warm only when we actually downloaded (first run / model switch).
        let was_present = match id {
            ipc::download::WHISPER => stt_model_ready(&app),
            ipc::download::KOKORO => tts::kokoro_assets_present(),
            _ => true,
        };
        let res = if id == ipc::download::WHISPER {
            stt::ensure_local_model(&stt_model, emit).await.map(|_| ())
        } else if id == ipc::download::LLM {
            local_llm::ensure_local_llm(&llm_model, emit)
                .await
                .map(|_| ())
        } else if id == ipc::download::KOKORO {
            tts::ensure_kokoro_assets(emit).await.map(|_| ())
        } else {
            Ok(())
        };
        match res {
            Err(e) => {
                log::warn!("download: {id} failed: {e}");
                emit_download_error(&app, id);
            }
            Ok(()) => {
                // Freshly downloaded → warm it now (load + graph compile) so the
                // first dictation / read-aloud after a first-run or model switch
                // isn't the one paying the cold-start cost. Skip when the model was
                // already present: at steady state STT warms lazily on the
                // dictation keypress (hotkeys::on_press, hidden under the hold) and
                // Kokoro up front in setup when selected — no idle load here.
                if !was_present {
                    if let Some(state) = app.try_state::<AppState>() {
                        if id == ipc::download::WHISPER {
                            state.transcriber.warm();
                        } else if id == ipc::download::KOKORO {
                            state.speaker.warm();
                        }
                    }
                }
            }
        }
        // Release the slot so a later retry can run.
        if let Some(state) = app.try_state::<AppState>() {
            if let Ok(mut d) = state.downloads.lock() {
                if let Some(slot) = dl_slot(&mut d, id) {
                    slot.in_flight = false;
                }
            }
        }
    });
}

/// Percent complete for one model's download, or `None` when the size is unknown.
fn dl_pct(p: DlProgress) -> Option<u32> {
    (p.total > 0).then(|| ((p.downloaded as f64 / p.total as f64) * 100.0) as u32)
}

/// Whether the configured speech-to-text model is downloaded and ready.
/// Dictation can't run until this is true; it gates the hold-to-talk trigger and
/// drives the overlay's "downloading model" message.
pub(crate) fn stt_model_ready<R: Runtime>(app: &AppHandle<R>) -> bool {
    let model = app
        .state::<AppState>()
        .config
        .lock()
        .map(|c| c.stt_model.clone())
        .unwrap_or_default();
    stt::model_path(&model).map(|p| p.exists()).unwrap_or(false)
}

/// The user tried to dictate before the speech model finished downloading: show
/// its progress in the overlay and, once, start a watcher that keeps the bar
/// updated until the model is ready (then clears it) or the download fails.
pub(crate) fn begin_model_wait<R: Runtime>(app: &AppHandle<R>) {
    let whisper = |app: &AppHandle<R>| {
        app.state::<AppState>()
            .downloads
            .lock()
            .ok()
            .map(|d| d.whisper)
            .unwrap_or_default()
    };
    // Trying to dictate is a natural retry trigger: (re)start the download if it
    // isn't already running (no-op if a prior attempt is still in flight).
    spawn_download(app, ipc::download::WHISPER);
    show_overlay(app);
    let p = whisper(app);
    emit_state(
        app,
        OverlayState::Preparing {
            downloaded: p.downloaded,
            total: p.total,
        },
    );
    // Only one watcher at a time.
    if app
        .state::<AppState>()
        .model_wait_active
        .swap(true, Ordering::AcqRel)
    {
        return;
    }
    let app = app.clone();
    std::thread::spawn(move || {
        loop {
            if stt_model_ready(&app) {
                emit_state(&app, OverlayState::Idle);
                break;
            }
            let p = whisper(&app);
            if p.failed {
                emit_state(
                    &app,
                    OverlayState::Error {
                        message: "speech model download failed — will retry on relaunch".into(),
                    },
                );
                idle_after(app.clone(), Duration::from_millis(3500));
                break;
            }
            emit_state(
                &app,
                OverlayState::Preparing {
                    downloaded: p.downloaded,
                    total: p.total,
                },
            );
            std::thread::sleep(Duration::from_millis(400));
        }
        app.state::<AppState>()
            .model_wait_active
            .store(false, Ordering::Release);
    });
}

pub fn emit_state<R: Runtime>(app: &AppHandle<R>, state: OverlayState) {
    if let Some(win) = app.get_webview_window("overlay") {
        if let Err(e) = win.emit(ipc::event::STATE, state) {
            log::debug!("emit overlay state failed: {e}");
        }
    }
}

/// Cycle TTS playback speed. Currently 1.0 ↔ 2.0; tracked in the Speaker.
/// Routes through `apply_speed` so the tray checkmarks and the on-disk
/// config stay in lockstep with the hotkey.
pub fn tts_speed_cycle<R: Runtime>(app: &AppHandle<R>) {
    let next = app.state::<AppState>().speaker.cycle_speed();
    apply_speed(app, next);
}

/// Read the current selection (or clipboard) aloud. If a read is already playing,
/// cancel it and start the new one. Always called on the main thread — both the
/// global shortcut callback and the tray dispatch here, and both
/// `selection::capture_selection` and `tts::Speaker::speak` require it.
pub fn tts_toggle<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<AppState>();
    // Re-triggering while a read is in progress cancels it and reads the current
    // selection instead of toggling off — stopping is still available via Esc or
    // the tray "Stop reading". stop() releases the old player immediately; the new
    // read below sets up its own overlay state + idle-watcher.
    if state.speaker.is_speaking() {
        log::info!("tts: cancel in-progress read, starting a new one");
        state.speaker.stop();
    }
    // Onboarding "Try read-aloud": speak a fixed sample instead of reading a
    // selection — no clipboard capture, no overlay. Reports progress to the
    // onboarding window so it can show "Playing…" then "done".
    if state.onboarding_read_test.load(Ordering::Acquire) {
        let provider = state
            .config
            .lock()
            .map(|c| c.tts_provider.clone())
            .unwrap_or_default();
        if provider == "kokoro" && !tts::kokoro_assets_present() {
            spawn_download(app, ipc::download::KOKORO);
            emit_read_test(app, "unavailable");
            return;
        }
        // Read the user's highlighted text — the real selection → TTS path —
        // reporting to the onboarding window instead of the overlay. Nudge them
        // to highlight the sample first if nothing's selected.
        match selection::capture_selection().ok().flatten() {
            Some(t) if !t.trim().is_empty() => {
                emit_read_test(app, "speaking");
                state.speaker.speak(&t);
                spawn_read_test_watcher(app.clone());
            }
            _ => emit_read_test(app, "select-first"),
        }
        return;
    }
    // Neural read-aloud requested but its voice model isn't downloaded yet: surface
    // it on the overlay (and keep the download going) rather than doing nothing.
    let provider = state
        .config
        .lock()
        .map(|c| c.tts_provider.clone())
        .unwrap_or_default();
    if provider == "kokoro" && !tts::kokoro_assets_present() {
        let pct = dl_pct(
            app.state::<AppState>()
                .downloads
                .lock()
                .ok()
                .map(|d| d.kokoro)
                .unwrap_or_default(),
        );
        spawn_download(app, ipc::download::KOKORO);
        show_overlay(app);
        emit_state(
            app,
            OverlayState::Error {
                message: format!(
                    "read-aloud voice still downloading{}",
                    pct.map(|p| format!(" ({p}%)")).unwrap_or_default()
                ),
            },
        );
        idle_after(app.clone(), Duration::from_millis(3000));
        return;
    }
    // Prefer the live selection; fall back to the clipboard when there's none,
    // so text you can't mouse-select (a mouse-capturing terminal TUI, etc.) can
    // still be read after copying it with the app's own command.
    let sel_t0 = std::time::Instant::now();
    let text = match selection::capture_selection() {
        Ok(Some(text)) => Some(text),
        Ok(None) => selection::clipboard_text(),
        Err(e) => {
            log::warn!("tts: selection capture failed: {e}");
            show_overlay(app);
            emit_state(
                app,
                OverlayState::Error {
                    message: format!("selection failed: {e}"),
                },
            );
            idle_after(app.clone(), Duration::from_millis(2200));
            return;
        }
    };
    log::info!(
        "tts: selection capture {:.0}ms",
        sel_t0.elapsed().as_secs_f32() * 1000.0
    );
    match text {
        Some(text) => {
            let chars = text.chars().count();
            log::info!("tts: speak ({} chars)", chars);
            record_usage(app, |u| u.record_tts(chars as u64));
            show_overlay(app);
            emit_state(app, OverlayState::Reading { progress: 0.0 });
            state.speaker.speak(&text);
            // The watcher registers the Esc-to-stop shortcut (from its own
            // thread, to avoid a global-shortcut re-lock deadlock) and releases
            // it when the read ends.
            spawn_tts_idle_watcher(app.clone());
        }
        None => {
            log::info!("tts: nothing to read (no selection or clipboard text)");
            show_overlay(app);
            emit_state(
                app,
                OverlayState::Error {
                    message: "nothing to read".into(),
                },
            );
            idle_after(app.clone(), Duration::from_millis(1600));
        }
    }
}

pub fn show_overlay<R: Runtime>(app: &AppHandle<R>) {
    if let Some(win) = app.get_webview_window("overlay") {
        // Re-place the pill on the screen the user is currently on before
        // showing — otherwise it stays pinned to wherever it started.
        let anchor = app
            .state::<AppState>()
            .config
            .lock()
            .map(|c| c.overlay_position.clone())
            .unwrap_or_else(|_| config::DEFAULT_OVERLAY_POSITION.to_string());
        position_overlay(&win, &anchor);
        let _ = win.set_always_on_top(true);
        let _ = win.set_ignore_cursor_events(true);
        let _ = win.show();
    }
}

/// Briefly flash the overlay at the configured anchor, so changing the position
/// in Settings shows *where* the pill will land. Reuses the "✓ done" pill.
pub(crate) fn preview_overlay_position<R: Runtime>(app: &AppHandle<R>) {
    show_overlay(app);
    emit_state(app, OverlayState::Done { chars: 0 });
    idle_after(app.clone(), Duration::from_millis(1400));
}

/// Find the monitor whose bounds contain `pt`, a global top-left point in
/// *logical* screen points. Each monitor's physical bounds are converted to
/// points by dividing by its own scale factor, so this stays correct on
/// mixed-DPI multi-monitor setups.
fn monitor_containing_point<R: Runtime>(
    win: &tauri::WebviewWindow<R>,
    pt: (f64, f64),
) -> Option<tauri::Monitor> {
    let monitors = win.available_monitors().ok()?;
    monitors.into_iter().find(|m| {
        let s = m.scale_factor();
        let pos = m.position();
        let size = m.size();
        let lx = pos.x as f64 / s;
        let ly = pos.y as f64 / s;
        let lw = size.width as f64 / s;
        let lh = size.height as f64 / s;
        pt.0 >= lx && pt.0 < lx + lw && pt.1 >= ly && pt.1 < ly + lh
    })
}

fn position_overlay<R: Runtime>(win: &tauri::WebviewWindow<R>, anchor: &str) {
    // Target the screen the user is actually working on: the one holding the
    // active window they're dictating into. Ask Accessibility for the focused
    // window's center (global top-left points) and match it to a monitor. If
    // there's no focused window, fall back to the monitor under the mouse
    // cursor, then the window's current monitor, then primary. Wrap in
    // early-returns so a single missing monitor query doesn't tank it.
    let monitor = focus::focused_window_center()
        .and_then(|pt| monitor_containing_point(win, pt))
        .or_else(|| {
            win.cursor_position()
                .ok()
                .and_then(|p| win.monitor_from_point(p.x, p.y).ok().flatten())
        })
        .or_else(|| win.current_monitor().ok().flatten())
        .or_else(|| win.primary_monitor().ok().flatten());
    let monitor = match monitor {
        Some(m) => m,
        None => {
            log::warn!("overlay: no monitor found; leaving position as-is");
            return;
        }
    };
    // Do ALL geometry in logical points. Physical pixels are per-monitor-scale
    // and don't compose into one space across mixed-DPI monitors, so a physical
    // position computed for a non-primary monitor is misinterpreted by
    // `set_position` (it dumped the pill onto the wrong screen). Logical points
    // ARE one consistent global space — the same one the AX focus point and
    // monitor-matching use — so convert monitor bounds to points, place in points,
    // and set a LogicalPosition.
    let s = monitor.scale_factor();
    let mp = monitor.position();
    let ms = monitor.size();
    let (ml, mt) = (mp.x as f64 / s, mp.y as f64 / s);
    let (mw, mh) = (ms.width as f64 / s, ms.height as f64 / s);

    // The overlay window's logical size (matches tauri.conf.json). The window
    // carries generous transparent margin around the centered pill so its
    // drop-shadow has room without hanging off-screen.
    const OVERLAY_W: f64 = 380.0;
    const OVERLAY_H: f64 = 160.0;
    // `anchor` is "<vertical>-<horizontal>" (e.g. "bottom-center"); unknown values
    // fall back to bottom-center. Keep the window FULLY inside the monitor — one
    // that hangs off the edge gets relocated by macOS (badly, on multi-monitor).
    let (vert, horiz) = anchor.split_once('-').unwrap_or(("bottom", "center"));
    let edge = 12.0; // gap from the screen edge on the docked side(s)
    let edge_top = 36.0; // clears the menu bar / notch on top anchors
    let x = match horiz {
        "left" => ml + edge,
        "right" => ml + mw - OVERLAY_W - edge,
        _ => ml + (mw - OVERLAY_W) / 2.0,
    };
    let y = match vert {
        "top" => mt + edge_top,
        _ => mt + mh - OVERLAY_H - edge,
    };
    let x = x.clamp(ml, ml + (mw - OVERLAY_W).max(0.0));
    let y = y.clamp(mt, mt + (mh - OVERLAY_H).max(0.0));
    if let Err(e) = win.set_position(tauri::LogicalPosition::new(x, y)) {
        log::warn!("overlay: set_position failed: {e}");
    }
}

/// Tell the overlay frontend to render nothing after a dwell. Uses a
/// std::thread so we don't block any tokio worker.
/// Poll the speaker after a `speak()` and emit `Idle` when playback ends —
/// either because the audio finished naturally or because the user pressed
/// the read-aloud shortcut again to stop. Without this the overlay would stay stuck on
/// "Transcribing…" forever.
fn spawn_tts_idle_watcher<R: Runtime>(app: AppHandle<R>) {
    std::thread::spawn(move || {
        let started_by = std::time::Instant::now() + Duration::from_secs(15);
        // Phase 1: wait for `is_speaking` to flip true (the AVPlayer has
        // started and is reading). Bail out if it never does — most likely
        // an API failure that already logged its own warning.
        loop {
            if app.state::<AppState>().speaker.is_speaking() {
                break;
            }
            if std::time::Instant::now() > started_by {
                hotkeys::unregister_tts_escape(&app);
                emit_state(&app, OverlayState::Idle);
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        // Read-aloud is playing — let Esc stop it. Registered from this
        // background thread (not the shortcut callback) to avoid a deadlock;
        // see hotkeys::register_tts_escape.
        hotkeys::register_tts_escape(&app);
        // Phase 2: wait for it to flip false (end-of-media or stop()), emitting
        // read-aloud progress so the overlay pill fills as it speaks.
        loop {
            std::thread::sleep(Duration::from_millis(120));
            let state = app.state::<AppState>();
            if !state.speaker.is_speaking() {
                hotkeys::unregister_tts_escape(&app);
                emit_state(&app, OverlayState::Idle);
                return;
            }
            if let Some(progress) = state.speaker.progress() {
                emit_state(&app, OverlayState::Reading { progress });
            }
        }
    });
}

fn idle_after<R: Runtime>(app: AppHandle<R>, delay: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        emit_state(&app, OverlayState::Idle);
    });
}

/// Build the tray menu. Returns the assembled menu plus parallel vectors of
/// the speed, voice, and microphone `CheckMenuItem`s so the click handler can
/// toggle their checked state when the user picks one.
/// The tray menu plus its checkable items, in `(menu, speed, voice, mic)` order,
/// so callers can flip the checkmarks when the user changes a setting.
type TrayMenu = (
    Menu<Wry>,
    Vec<CheckMenuItem<Wry>>,
    Vec<CheckMenuItem<Wry>>,
    Vec<CheckMenuItem<Wry>>,
    MenuItem<Wry>,
    MenuItem<Wry>,
);

/// Render a `tauri-plugin-global-shortcut` accelerator (e.g.
/// "CmdOrCtrl+Shift+R") as a compact macOS symbol hint ("⌘⇧R") for menu
/// labels. Unknown tokens pass through uppercased so an odd chord still shows
/// *something* truthful rather than a hardcoded guess.
pub(crate) fn format_accelerator(accel: &str) -> String {
    accel
        .split('+')
        .map(|tok| match tok.trim().to_ascii_lowercase().as_str() {
            "cmdorctrl" | "cmd" | "command" | "super" | "meta" => "⌘".to_string(),
            "ctrl" | "control" => "⌃".to_string(),
            "alt" | "option" => "⌥".to_string(),
            "shift" => "⇧".to_string(),
            other => other.to_ascii_uppercase(),
        })
        .collect()
}

fn build_tray_menu(
    app: &AppHandle<Wry>,
    cfg: &config::Config,
    mic_names: &[String],
) -> tauri::Result<TrayMenu> {
    let open_main = MenuItem::with_id(app, "open_main", "Settings…", true, None::<&str>)?;
    let open_setup = MenuItem::with_id(app, "open_setup", "Setup…", true, None::<&str>)?;
    let check_update = MenuItem::with_id(
        app,
        "check_update",
        "Check for Updates…",
        true,
        None::<&str>,
    )?;
    let read = MenuItem::with_id(
        app,
        "tts_read",
        format!("Read selection ({})", format_accelerator(&cfg.hotkey_tts)),
        true,
        None::<&str>,
    )?;
    let stop_read = MenuItem::with_id(app, "tts_stop", "Stop reading", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Murmur", true, None::<&str>)?;

    // Speed submenu — CheckMenuItems with the current selection pre-checked.
    let speed_items: Vec<CheckMenuItem<Wry>> = tts::SPEEDS
        .iter()
        .map(|&s| {
            let id = format!("speed_{s}");
            let label = format!("{s:.1}×");
            let checked = (s - cfg.tts_speed).abs() < 1e-3;
            CheckMenuItem::with_id(app, id, label, true, checked, None::<&str>)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let speed_refs: Vec<&dyn IsMenuItem<Wry>> = speed_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<Wry>)
        .collect();
    let speed_menu = Submenu::with_id_and_items(app, "speed", "Speed", true, &speed_refs)?;

    // Voice submenu — the list matches the active TTS provider so the picker
    // reflects the backend that will actually speak.
    let voice_items: Vec<CheckMenuItem<Wry>> = tts::voices_for(&cfg.tts_provider)
        .iter()
        .map(|(id, name)| {
            let item_id = format!("voice_{id}");
            let checked = *id == cfg.tts_voice_id;
            CheckMenuItem::with_id(app, item_id, *name, true, checked, None::<&str>)
        })
        .collect::<tauri::Result<Vec<_>>>()?;
    let voice_refs: Vec<&dyn IsMenuItem<Wry>> = voice_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<Wry>)
        .collect();
    let voice_menu = Submenu::with_id_and_items(app, "voice", "Voice", true, &voice_refs)?;

    // Microphone submenu — first entry is the system default, rest are
    // whatever cpal enumerated.
    let mut mic_items: Vec<CheckMenuItem<Wry>> = Vec::with_capacity(mic_names.len() + 1);
    mic_items.push(CheckMenuItem::with_id(
        app,
        "mic_default",
        "System default",
        true,
        cfg.mic_name.is_none(),
        None::<&str>,
    )?);
    for (i, name) in mic_names.iter().enumerate() {
        let id = format!("mic_{i}");
        let checked = cfg.mic_name.as_deref() == Some(name.as_str());
        mic_items.push(CheckMenuItem::with_id(
            app,
            id,
            name.as_str(),
            true,
            checked,
            None::<&str>,
        )?);
    }
    let mic_refs: Vec<&dyn IsMenuItem<Wry>> = mic_items
        .iter()
        .map(|i| i as &dyn IsMenuItem<Wry>)
        .collect();
    let mic_menu = Submenu::with_id_and_items(app, "mic", "Microphone", true, &mic_refs)?;

    let sep1 = PredefinedMenuItem::separator(app)?;
    let sep2 = PredefinedMenuItem::separator(app)?;

    let sep0 = PredefinedMenuItem::separator(app)?;
    let menu = Menu::with_items(
        app,
        &[
            &open_main as &dyn IsMenuItem<Wry>,
            &open_setup,
            &check_update,
            &sep0,
            &read,
            &stop_read,
            &sep1,
            &speed_menu,
            &voice_menu,
            &mic_menu,
            &sep2,
            &quit,
        ],
    )?;
    Ok((
        menu,
        speed_items,
        voice_items,
        mic_items,
        check_update,
        read,
    ))
}

fn handle_tray_event<R: Runtime>(app: &AppHandle<R>, event: tauri::menu::MenuEvent) {
    let id = event.id.as_ref();
    match id {
        "open_main" => show_main_window(app),
        "open_setup" => show_onboarding_window(app),
        "check_update" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                // When an update is staged this item reads "Restart to update
                // (vX)" — apply it now.
                let staged = app
                    .state::<AppState>()
                    .pending_update
                    .lock()
                    .map(|g| g.is_some())
                    .unwrap_or(false);
                if staged {
                    if let Err(e) = install_pending(&app).await {
                        log::warn!("update: restart-to-update failed: {e}");
                    }
                    return;
                }
                // Otherwise, a plain "Check for Updates…": check + download, open
                // Settings, and surface the result — an "up to date" modal, or
                // the install banner when an update was staged.
                show_main_window(&app);
                match update::check_and_download(&app).await {
                    Some(u) => {
                        let info = u.info();
                        if let Ok(mut g) = app.state::<AppState>().pending_update.lock() {
                            *g = Some(u);
                        }
                        mark_update_staged(&app, info);
                    }
                    None => {
                        let _ = app.emit(ipc::event::UPDATE_NONE, ());
                    }
                }
            });
        }
        "quit" => hard_exit(0),
        "tts_read" => tts_toggle(app),
        "tts_stop" => {
            app.state::<AppState>().speaker.stop();
        }
        _ => {
            if let Some(s) = id.strip_prefix("speed_") {
                if let Ok(speed) = s.parse::<f32>() {
                    apply_speed(app, speed);
                }
            } else if let Some(v) = id.strip_prefix("voice_") {
                apply_voice(app, v);
            } else if id == "mic_default" {
                apply_mic(app, None);
            } else if let Some(idx_str) = id.strip_prefix("mic_") {
                if let Ok(idx) = idx_str.parse::<usize>() {
                    let name = app.state::<AppState>().mic_names.get(idx).cloned();
                    apply_mic(app, name);
                }
            }
        }
    }
}

/// Apply a speed selection: update the speaker, flip tray checkmarks so the
/// chosen one is the only check, and persist to disk.
pub(crate) fn apply_speed<R: Runtime>(app: &AppHandle<R>, speed: f32) {
    let state = app.state::<AppState>();
    state.speaker.set_speed(speed);
    for (item, &s) in state.speed_items.iter().zip(tts::SPEEDS.iter()) {
        let _ = item.set_checked((s - speed).abs() < 1e-3);
    }
    log::info!("tts: speed → {speed}x");
    persist_config(app);
}

/// Apply a voice selection: update the speaker, flip tray checkmarks, persist.
pub(crate) fn apply_voice<R: Runtime>(app: &AppHandle<R>, voice_id: &str) {
    let state = app.state::<AppState>();
    state.speaker.set_voice(voice_id);
    let provider = state
        .config
        .lock()
        .map(|c| c.tts_provider.clone())
        .unwrap_or_default();
    for (item, (id, _)) in state
        .voice_items
        .iter()
        .zip(tts::voices_for(&provider).iter())
    {
        let _ = item.set_checked(*id == voice_id);
    }
    log::info!("tts: voice → {voice_id}");
    persist_config(app);
}

/// Apply a microphone selection: store the name, flip tray checkmarks (the
/// "System default" item is index 0), persist. The new device takes effect
/// on the next `DictationCmd::Start`.
pub(crate) fn apply_mic<R: Runtime>(app: &AppHandle<R>, name: Option<String>) {
    let state = app.state::<AppState>();
    *state.mic_name.lock().expect("mic name mutex") = name.clone();
    if let Some(item) = state.mic_items.first() {
        let _ = item.set_checked(name.is_none());
    }
    for (i, item) in state.mic_items.iter().skip(1).enumerate() {
        let matches = state
            .mic_names
            .get(i)
            .map(|n| Some(n.as_str()) == name.as_deref())
            .unwrap_or(false);
        let _ = item.set_checked(matches);
    }
    log::info!("audio: mic → {}", name.as_deref().unwrap_or("(default)"));
    persist_config(app);
}

fn persist_config<R: Runtime>(app: &AppHandle<R>) {
    let state = app.state::<AppState>();
    // Update the shared live config in place (preserving refine/history fields
    // the tray doesn't own), then write that snapshot to disk.
    let snapshot = {
        let Ok(mut cfg) = state.config.lock() else {
            log::warn!("config: lock poisoned; skipping save");
            return;
        };
        cfg.tts_speed = state.speaker.current_speed();
        cfg.tts_voice_id = state.speaker.current_voice().unwrap_or_default();
        cfg.mic_name = state.mic_name.lock().ok().and_then(|g| g.clone());
        cfg.clone()
    };
    if let Err(e) = config::save(&snapshot) {
        log::warn!("config: save failed: {e}");
    }
    // Nudge the settings window to re-sync speed/voice/mic, which the tray and
    // hotkeys can change while it's open.
    let _ = app.emit(ipc::event::CONFIG_CHANGED, ());
}

/// Play the dictation start/stop cue, unless the `dictation_sound` config flag
/// is off. Cheap and fire-and-forget (see `sound.rs`).
fn play_dictation_cue<R: Runtime>(app: &AppHandle<R>, start: bool) {
    let state = app.state::<AppState>();
    let enabled = state
        .config
        .lock()
        .map(|c| c.dictation_sound)
        .unwrap_or(true);
    if !enabled {
        return;
    }
    if start {
        state.cues.play_start();
    } else {
        state.cues.play_stop();
    }
}

fn spawn_dictation_worker<R: Runtime>(
    app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
    chat: Arc<dyn llm::LlmChat>,
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
                    let mic = app
                        .state::<AppState>()
                        .mic_name
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    // Forward audio levels to the overlay frontend.
                    let app_for_level = app.clone();
                    let on_level: audio::LevelFn = Box::new(move |level: f32| {
                        let _ = app_for_level.emit(ipc::event::AUDIO_LEVEL, level);
                    });
                    match audio::Recorder::start(mic.as_deref(), Some(on_level)) {
                        Ok(r) => {
                            log::info!("dictation: recording started");
                            play_dictation_cue(&app, true);
                            rec = Some(r);
                        }
                        Err(e) => {
                            log::warn!("dictation: failed to start recording: {e}");
                            // Recording never started; free the Esc hijack
                            // on_press registered.
                            hotkeys::unregister_escape(&app);
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
                DictationCmd::Cancel => {
                    if rec.take().is_some() {
                        log::info!("dictation: cancelled");
                    }
                    // Free the Esc hijack — a Cancel is the end of the dictation,
                    // whether it fired during recording or mid-transcribe.
                    hotkeys::unregister_escape(&app);
                    emit_state(&app, OverlayState::Idle);
                    continue;
                }
                DictationCmd::Stop { mode } => {
                    let Some(r) = rec.take() else {
                        log::debug!("dictation: Stop without active recording");
                        // Esc was registered on press but no recording is in
                        // flight (e.g. mic start failed) — release it.
                        hotkeys::unregister_escape(&app);
                        continue;
                    };
                    play_dictation_cue(&app, false);
                    let stop_t0 = std::time::Instant::now();
                    match r.stop() {
                        Ok(recording) => {
                            log::info!(
                                "stt: recorder stopped in {:.0}ms ({}ms audio captured)",
                                stop_t0.elapsed().as_secs_f32() * 1000.0,
                                recording.duration_ms,
                            );
                            handle_recording(
                                app.clone(),
                                transcriber.clone(),
                                chat.clone(),
                                recording,
                                mode,
                            );
                        }
                        Err(e) => {
                            log::warn!("dictation: stop failed: {e}");
                            hotkeys::unregister_escape(&app);
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

/// Record local usage (STT audio-seconds / TTS characters), persist, and push
/// the updated totals to the settings window.
fn record_usage<R: Runtime>(app: &AppHandle<R>, update: impl FnOnce(&mut usage::UsageStats)) {
    if let Ok(mut u) = app.state::<AppState>().usage.lock() {
        update(&mut u);
        let snapshot = u.clone();
        drop(u);
        if let Err(e) = usage::save(&snapshot) {
            log::warn!("usage: save failed: {e}");
        }
        let _ = app.emit(ipc::event::USAGE, snapshot);
    }
}

/// Save a committed dictation to the history database, honoring the enable
/// toggle and retention cap. `refined` is set only for Fn-refined dictations.
fn record_history<R: Runtime>(app: &AppHandle<R>, raw: &str, refined: Option<&str>) {
    // Clone the Arcs out of the managed state so the locks below don't borrow
    // the temporary `State` guard.
    let (config, history) = {
        let state = app.state::<AppState>();
        (state.config.clone(), state.history.clone())
    };
    // History is always recorded now (capped by history_limit).
    let limit = config.lock().map(|c| c.history_limit).unwrap_or(1000);
    let conn = match history.lock() {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Err(e) = history::insert(&conn, raw, refined) {
        log::warn!("history: insert failed: {e}");
    }
    let _ = history::prune(&conn, limit);
    drop(conn);
    let _ = app.emit(ipc::event::HISTORY, ()); // nudge the History tab to refresh if open
}

/// A phase of the onboarding "Try it" test, pushed to the onboarding window.
/// `phase` is "recording" (trigger held), "transcribing", or "done"; on "done"
/// `text` is the transcript and `heard_audio` is false when the clip was
/// effectively silent (the mic isn't feeding audio — the same 1e-4 floor the
/// live path uses), which the frontend turns into a "check your mic" diagnosis.
#[derive(Clone, Serialize)]
struct TestDictationEvent {
    phase: &'static str,
    text: String,
    heard_audio: bool,
}

/// Push a "Try it" phase to the onboarding window. Called from `hotkeys::on_press`
/// (recording) and `handle_recording` (transcribing/done).
pub(crate) fn emit_test_state<R: Runtime>(
    app: &AppHandle<R>,
    phase: &'static str,
    text: String,
    heard_audio: bool,
) {
    let _ = app.emit(
        ipc::event::TEST_DICTATION_RESULT,
        TestDictationEvent {
            phase,
            text,
            heard_audio,
        },
    );
}

/// A phase of the onboarding "Try read-aloud" test: "speaking", "done", or
/// "unavailable" (the neural voice is still downloading).
#[derive(Clone, Serialize)]
struct ReadTestEvent {
    phase: &'static str,
}

/// Push a "Try read-aloud" phase to the onboarding window.
pub(crate) fn emit_read_test<R: Runtime>(app: &AppHandle<R>, phase: &'static str) {
    let _ = app.emit(ipc::event::TEST_READ_RESULT, ReadTestEvent { phase });
}

/// Wait for the onboarding read-aloud sample to finish, then tell the window.
/// Mirrors `spawn_tts_idle_watcher` but reports to onboarding rather than driving
/// the overlay + Esc.
fn spawn_read_test_watcher<R: Runtime>(app: AppHandle<R>) {
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        // Wait for playback to actually start (bail if it never does).
        loop {
            if app.state::<AppState>().speaker.is_speaking() {
                break;
            }
            if std::time::Instant::now() > deadline {
                emit_read_test(&app, "done");
                return;
            }
            std::thread::sleep(Duration::from_millis(80));
        }
        // Then wait for it to end.
        loop {
            std::thread::sleep(Duration::from_millis(120));
            if !app.state::<AppState>().speaker.is_speaking() {
                emit_read_test(&app, "done");
                return;
            }
        }
    });
}

fn handle_recording<R: Runtime>(
    app: AppHandle<R>,
    transcriber: Arc<dyn stt::Transcriber>,
    chat: Arc<dyn llm::LlmChat>,
    recording: audio::Recording,
    mode: DictationMode,
) {
    // Onboarding "Try it": no overlay, paste, refine, or history — just
    // transcribe (unless the clip was silent or too short) and report the text
    // back to the onboarding window, which turns it into a success or a
    // diagnosis. This validates the real mic → capture → STT path end to end.
    if mode == DictationMode::Test {
        let heard = recording.mean_abs >= 1e-4;
        tauri::async_runtime::spawn(async move {
            emit_test_state(&app, "transcribing", String::new(), heard);
            let text = if heard && recording.duration_ms >= 200 {
                transcriber
                    .transcribe(&recording.wav)
                    .await
                    .unwrap_or_default()
            } else {
                String::new()
            };
            emit_test_state(&app, "done", text.trim().to_string(), heard);
        });
        return;
    }
    // These early exits end the dictation, so free the Esc hijack on_press
    // registered (it stays live through transcribe/refine otherwise).
    if recording.duration_ms < 200 {
        log::info!(
            "dictation: discarded short clip ({} ms)",
            recording.duration_ms
        );
        hotkeys::unregister_escape(&app);
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
        hotkeys::unregister_escape(&app);
        emit_state(
            &app,
            OverlayState::Error {
                message: "no mic input — grant Microphone access".into(),
            },
        );
        idle_after(app, Duration::from_millis(2500));
        return;
    }

    // Esc stays hijacked (from on_press) through transcribe + refine so the user
    // can still cancel after releasing the trigger. `gen` is this pipeline's
    // identity; an Esc bumps `dictation_gen`, and each hop below bails — and
    // frees Esc — when it no longer matches.
    let gen = app
        .state::<AppState>()
        .dictation_gen
        .load(Ordering::Acquire);
    // End-to-end: from here (recording handed off) through transcribe + refine +
    // paste, so the felt latency can be compared against the pure inference time
    // the transcriber logs. A big gap between the two points at contention or
    // queueing around inference rather than slow inference itself.
    let pipeline_t0 = std::time::Instant::now();
    tauri::async_runtime::spawn(async move {
        let secs = recording.duration_ms as f64 / 1000.0;
        let transcribe_result = transcriber.transcribe(&recording.wav).await;
        // Esc pressed while Whisper was running: abandon the transcript (the
        // native call already finished — we just drop its output), don't paste.
        if dictation_cancelled(&app, gen) {
            log::info!("dictation: cancelled during transcription");
            hotkeys::unregister_escape(&app);
            emit_state(&app, OverlayState::Idle);
            return;
        }
        // Count the audio toward usage stats once it transcribes cleanly.
        if transcribe_result.is_ok() {
            record_usage(&app, |u| u.record_stt(secs));
        }
        let (state, dwell_ms) = match transcribe_result {
            Ok(text) if text.is_empty() => {
                log::info!("dictation: empty transcript");
                (OverlayState::Done { chars: 0 }, 400)
            }
            Ok(text) => {
                log::info!(
                    "dictation: transcript ({} chars): {}",
                    text.chars().count(),
                    text
                );
                run_dictation(
                    &app,
                    chat.as_ref(),
                    mode == DictationMode::Refine,
                    text,
                    gen,
                )
                .await
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
        hotkeys::unregister_escape(&app);
        log::info!(
            "stt: dictation end-to-end {:.0}ms (transcribe→refine→paste)",
            pipeline_t0.elapsed().as_secs_f32() * 1000.0,
        );
        emit_state(&app, state);
        idle_after(app, Duration::from_millis(dwell_ms));
    });
}

/// True once the user has pressed Esc to cancel the dictation identified by
/// `gen` (its `dictation_gen` snapshot at spawn). Checked at each pipeline hop so
/// a cancel abandons the result instead of pasting it.
fn dictation_cancelled<R: Runtime>(app: &AppHandle<R>, gen: u64) -> bool {
    app.state::<AppState>()
        .dictation_gen
        .load(Ordering::Acquire)
        != gen
}

/// Plain / refined dictation: optionally clean the transcript with the LLM, log
/// it to history, and paste. Returns the overlay state + dwell for the caller
/// to emit. On refine failure we fall back to the raw transcript so a dropped
/// API call never loses the user's dictation.
async fn run_dictation<R: Runtime>(
    app: &AppHandle<R>,
    chat: &dyn llm::LlmChat,
    refine: bool,
    text: String,
    gen: u64,
) -> (OverlayState, u64) {
    let raw = text.clone(); // keep the original transcript for history
    let mut was_refined = false;
    // The refine prompt + model come from live config so edits apply without restart.
    let (prompt, llm_model) = match app.state::<AppState>().config.lock() {
        Ok(c) => (c.refine_prompt.clone(), c.llm_model.clone()),
        Err(_) => (String::new(), String::new()),
    };
    // Refinement requested but its model hasn't downloaded yet: surface it on the
    // overlay (and keep the download going) instead of silently pasting the
    // unrefined text. The words are saved to History so they aren't lost.
    if refine && !local_llm::assets_present(&llm_model) {
        let pct = dl_pct(
            app.state::<AppState>()
                .downloads
                .lock()
                .ok()
                .map(|d| d.llm)
                .unwrap_or_default(),
        );
        spawn_download(app, ipc::download::LLM);
        record_history(app, &raw, None);
        return (
            OverlayState::Error {
                message: format!(
                    "refine model still downloading{} — saved to History",
                    pct.map(|p| format!(" ({p}%)")).unwrap_or_default()
                ),
            },
            3500,
        );
    }
    // Esc during the "Transcribing…" hand-off, before we start refining.
    if dictation_cancelled(app, gen) {
        return (OverlayState::Idle, 0);
    }
    let final_text = if refine {
        emit_state(app, OverlayState::Refining);
        match llm::transform(chat, &prompt, &text).await {
            Ok(refined) => {
                log::info!(
                    "dictation: refined ({} chars): {}",
                    refined.chars().count(),
                    refined
                );
                // Count the refined dictation and refresh the settings window's
                // usage view if it's open.
                record_usage(app, |u| u.record_refine());
                was_refined = true;
                refined
            }
            Err(e) => {
                log::warn!("dictation: refine failed, pasting raw transcript: {e}");
                text
            }
        }
    } else {
        text
    };
    // Last chance to bail before we commit: Esc during refine (or right at the
    // end) drops the result — nothing pasted, nothing saved to History.
    if dictation_cancelled(app, gen) {
        return (OverlayState::Idle, 0);
    }
    record_history(
        app,
        &raw,
        if was_refined { Some(&final_text) } else { None },
    );
    let chars = final_text.chars().count();
    // Continuation spacing: prepend a space when this dictation lands soon after
    // the previous one AND into the same window (re-engaging Fn to continue a
    // thought). History keeps the clean text; only the pasted copy is spaced.
    let cur_win = focus::focused_window_center();
    let continues = {
        let last = app
            .state::<AppState>()
            .last_dictation
            .lock()
            .ok()
            .and_then(|g| *g);
        matches!((last, cur_win), (Some((t, w)), Some(cw))
            if t.elapsed() <= DICTATION_CONTINUATION_WINDOW && same_window(w, cw))
    };
    let to_paste =
        if continues && !final_text.is_empty() && !final_text.starts_with(char::is_whitespace) {
            format!(" {final_text}")
        } else {
            final_text
        };
    // `inject::paste_text` calls enigo, which posts CGEvents via Quartz.
    // CGEventPost requires a CFRunLoop on the calling thread — tokio workers
    // don't have one, and calling from a bare worker exits the process
    // silently. Run on the main thread (NSApp's run loop) instead.
    let paste_result = paste_on_main_thread(app, to_paste).await;
    // Record this paste for the next continuation check — only when we know the
    // window, so an unknown window can't chain a stray space onto the next one.
    if let Ok(mut g) = app.state::<AppState>().last_dictation.lock() {
        *g = cur_win.map(|cw| (std::time::Instant::now(), cw));
    }
    match paste_result {
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

async fn paste_on_main_thread<R: Runtime>(app: &AppHandle<R>, text: String) -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(inject::paste_text(&text));
    })
    .map_err(|e| anyhow::anyhow!("dispatch to main thread failed: {e}"))?;
    rx.await
        .map_err(|e| anyhow::anyhow!("paste task cancelled: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::format_accelerator;

    #[test]
    fn accelerator_renders_mac_symbols() {
        // The read-aloud default — the chord the tray was mislabeling as ⌥A.
        assert_eq!(format_accelerator("CmdOrCtrl+Shift+R"), "⌘⇧R");
        assert_eq!(format_accelerator("Cmd+Ctrl+S"), "⌘⌃S");
        // An Option chord still renders truthfully (⌥) rather than a guess.
        assert_eq!(format_accelerator("Alt+A"), "⌥A");
        assert_eq!(format_accelerator("Option+A"), "⌥A");
    }
}
