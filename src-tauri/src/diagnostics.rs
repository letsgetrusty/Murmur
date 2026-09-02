//! Diagnostics + one-click bug reports. Everything here is on-device: it reads
//! local state (version, macOS, permissions, config, the log file) and packages
//! it so a developer-user can file a high-signal GitHub issue without hand-
//! collecting any of it.
//!
//! The log discipline elsewhere is deliberate — the log records counts, states
//! and error strings, never the dictated/selected text (see `inject.rs`) — so a
//! log tail is safe to ship in a report. We still cap it to the current session
//! (the log is truncated each launch) and last N lines.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader};

use tauri::{AppHandle, Manager, Runtime};

use crate::AppState;

const REPO: &str = "https://github.com/letsgetrusty/Murmur";
/// How many trailing log lines to attach. The log is truncated each launch, so
/// this is "recent activity this session", which is what a repro needs.
const LOG_TAIL_LINES: usize = 120;

/// Classified cause of a paste/injection failure, so the overlay can say
/// something actionable instead of the truncated "synthesize Cmd+V".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PasteFailureCause {
    /// enigo refused because macOS Accessibility isn't granted to Murmur — by far
    /// the most common cause (enigo 0.6 checks `AXIsProcessTrusted` in `new()`,
    /// so a missing grant is a hard error, not a silent no-op).
    Accessibility,
    /// Secure Event Input is active (a password field, or Terminal's "Secure
    /// Keyboard Entry"): macOS drops synthesized keystrokes on purpose.
    SecureInput,
    /// Anything else — a CGEvent post failure, a clipboard error, etc.
    Other,
}

impl PasteFailureCause {
    /// Short line for the click-through status overlay (small, ~1 line).
    pub fn overlay_message(self) -> &'static str {
        match self {
            Self::Accessibility => "Paste failed — grant Accessibility to Murmur",
            Self::SecureInput => "Paste failed — Secure Input active (password field / Terminal)",
            Self::Other => "Paste failed — report it from Settings ▸ Support",
        }
    }
}

/// Best-effort classification from the full error chain plus the live permission
/// / secure-input state. Order matters: a missing Accessibility grant makes
/// enigo fail first, regardless of anything else.
pub fn classify_paste_failure(err_chain: &str) -> PasteFailureCause {
    if !crate::permissions::accessibility_granted() {
        return PasteFailureCause::Accessibility;
    }
    if secure_input_active() {
        return PasteFailureCause::SecureInput;
    }
    let lc = err_chain.to_lowercase();
    if lc.contains("accessibility") || lc.contains("permission") {
        return PasteFailureCause::Accessibility;
    }
    PasteFailureCause::Other
}

/// macOS "Secure Event Input": while any app has it enabled (password fields,
/// Terminal's Secure Keyboard Entry, some password managers), synthetic
/// keystrokes are silently dropped — which breaks clipboard-paste injection.
pub fn secure_input_active() -> bool {
    // `IsSecureEventInputEnabled` lives in the Carbon (HIToolbox) framework and
    // returns a Carbon `Boolean` (unsigned char, 0/1).
    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        fn IsSecureEventInputEnabled() -> u8;
    }
    // SAFETY: a parameterless Carbon query with no side effects; callable from
    // any thread.
    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// A packaged bug report, ready to drop into a GitHub issue.
pub struct BugReport {
    /// One-liner for the in-app confirmation ("Diagnostics copied — paste …").
    pub summary: String,
    /// Markdown environment block for the prefilled issue URL body (kept small
    /// so it fits well under the ~8KB URL budget — the log rides the clipboard).
    pub issue_body: String,
    /// The full report (environment + log tail) copied to the clipboard.
    pub full: String,
}

/// Gather everything into a `BugReport`. Never fails — missing pieces render as
/// "unknown" so a report is always producible.
pub fn gather<R: Runtime>(app: &AppHandle<R>) -> BugReport {
    let version = env!("CARGO_PKG_VERSION");
    let os = os_version();
    let arch = std::env::consts::ARCH;

    let accessibility = crate::permissions::accessibility_granted();
    let mic = mic_status_label(crate::permissions::microphone_status());
    let secure = secure_input_active();

    // Config summary + model presence — no transcripts, no prompt text.
    let (
        stt_model,
        llm_model,
        tts_provider,
        tts_voice,
        dictation_trigger,
        hotkey_dictate,
        hotkey_tts,
        refine_modifier,
    ) = app
        .state::<AppState>()
        .config
        .lock()
        .map(|c| {
            (
                c.stt_model.clone(),
                c.llm_model.clone(),
                c.tts_provider.clone(),
                c.tts_voice_id.clone(),
                c.dictation_trigger.clone(),
                c.hotkey_dictate.clone(),
                c.hotkey_tts.clone(),
                c.refine_modifier.clone(),
            )
        })
        // On lock poison, fall back to explicit "unknown" rather than empty
        // strings, so the report never shows a blank "STT model: " line.
        .unwrap_or_else(|_| {
            let u = || "unknown".to_string();
            (u(), u(), u(), u(), u(), u(), u(), u())
        });

    let whisper_ready = crate::stt::model_path(&stt_model)
        .map(|p| p.exists())
        .unwrap_or(false);
    let llm_ready = crate::local_llm::assets_present(&llm_model);
    let kokoro_ready = crate::tts::kokoro_assets_present();

    let mut env = String::new();
    let _ = writeln!(env, "### Environment");
    let _ = writeln!(env, "- Murmur: v{version}");
    let _ = writeln!(env, "- macOS: {os} ({arch})");
    let _ = writeln!(env, "- Accessibility granted: {}", yes_no(accessibility));
    let _ = writeln!(env, "- Microphone: {mic}");
    let _ = writeln!(env, "- Secure Input active: {}", yes_no(secure));
    let _ = writeln!(
        env,
        "- STT model: {stt_model} (present: {})",
        yes_no(whisper_ready)
    );
    let _ = writeln!(env, "- TTS provider: {tts_provider} (voice: {tts_voice})");
    let _ = writeln!(env, "- Kokoro assets present: {}", yes_no(kokoro_ready));
    let _ = writeln!(
        env,
        "- Refine LLM: {llm_model} (present: {})",
        yes_no(llm_ready)
    );
    let _ = writeln!(
        env,
        "- Hotkeys: dictate={hotkey_dictate}, read={hotkey_tts}, refine-mod={refine_modifier}, trigger={dictation_trigger}"
    );

    // The prefilled issue body: a template the user fills in, plus the env block.
    let mut issue_body = String::new();
    let _ = writeln!(
        issue_body,
        "### What happened\n\n<!-- describe the bug -->\n"
    );
    let _ = writeln!(issue_body, "### Steps to reproduce\n\n1. \n2. \n");
    let _ = writeln!(issue_body, "{env}");
    let _ = writeln!(
        issue_body,
        "<!-- The full diagnostics + recent log were copied to your clipboard — paste them below. -->"
    );

    // The clipboard payload: env block + recent log tail.
    let mut full = String::new();
    let _ = writeln!(full, "{env}");
    let _ = writeln!(
        full,
        "### Recent log (`~/Library/Logs/murmur.log`, last {LOG_TAIL_LINES} lines)\n"
    );
    let _ = writeln!(full, "```");
    let _ = writeln!(full, "{}", log_tail(LOG_TAIL_LINES));
    let _ = writeln!(full, "```");

    let summary = format!(
        "Murmur v{version} · macOS {os} · Accessibility {}",
        if accessibility { "granted" } else { "DENIED" }
    );

    BugReport {
        summary,
        issue_body,
        full,
    }
}

/// Build the prefilled "new issue" URL (env block only — the log goes via the
/// clipboard because it would blow the URL length budget).
pub fn issue_url(report: &BugReport) -> String {
    format!(
        "{REPO}/issues/new?title={}&body={}",
        percent_encode("[bug] paste/dictation failure"),
        percent_encode(&report.issue_body),
    )
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn mic_status_label(status: i64) -> &'static str {
    match status {
        0 => "not determined",
        1 => "restricted",
        2 => "denied",
        3 => "granted",
        _ => "unknown",
    }
}

fn os_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn log_tail(max_lines: usize) -> String {
    let Some(home) = std::env::var_os("HOME") else {
        return "(HOME unset — cannot locate log)".to_string();
    };
    let mut path = std::path::PathBuf::from(home);
    path.push("Library/Logs/murmur.log");
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => return format!("(could not read {}: {e})", path.display()),
    };
    // Stream the file, keeping only the last `max_lines` in a ring buffer, so a
    // long-running session's log can't blow up memory when filing a report.
    let mut ring: VecDeque<String> = VecDeque::with_capacity(max_lines + 1);
    for line in BufReader::new(file).lines() {
        match line {
            Ok(l) => {
                if ring.len() == max_lines {
                    ring.pop_front();
                }
                ring.push_back(l);
            }
            Err(e) => return format!("(error reading {}: {e})", path.display()),
        }
    }
    Vec::from(ring).join("\n")
}

/// Percent-encode for a URL query component (RFC 3986 unreserved set kept).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}
