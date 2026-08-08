//! String identifiers shared with the webviews. Mirrored on the frontend in
//! `frontend/constants.js` (EVENTS / DOWNLOAD) — keep the two in sync so a
//! rename can't silently break the Rust↔JS wiring.

/// Tauri event names emitted to the webviews (constants.js `EVENTS`).
pub mod event {
    /// Overlay: recording → transcribing → done/error state transitions.
    pub const STATE: &str = "state";
    /// Overlay: mic input level for the waveform.
    pub const AUDIO_LEVEL: &str = "audio:level";
    /// Settings: usage counters changed.
    pub const USAGE: &str = "usage";
    /// Settings: a dictation was recorded or history cleared.
    pub const HISTORY: &str = "history";
    /// Onboarding: model download progress.
    pub const MODEL_DOWNLOAD: &str = "model-download";
    /// Settings: an update finished downloading and is staged.
    pub const UPDATE_STAGED: &str = "update-staged";
    /// Settings: an update check found nothing newer.
    pub const UPDATE_NONE: &str = "update-none";
}

/// Model-download ids carried in the MODEL_DOWNLOAD payload's `id` field
/// (constants.js `DOWNLOAD`).
pub mod download {
    pub const WHISPER: &str = "whisper";
    pub const LLM: &str = "llm";
    pub const KOKORO: &str = "kokoro";
}

// Contract tests: the whole point of this module is that the Rust and JS sides
// agree on these strings. These assert that agreement, so a rename on one side
// that isn't mirrored on the other fails the build instead of breaking silently
// at runtime.
#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    const CONSTANTS_JS: &str = include_str!("../../frontend/constants.js");
    const LIB_RS: &str = include_str!("lib.rs");

    /// The substring of `src` strictly between the first `start` marker and the
    /// next `end` after it.
    fn slice_between<'a>(src: &'a str, start: &str, end: &str) -> &'a str {
        let s = src
            .find(start)
            .unwrap_or_else(|| panic!("marker not found: {start}"))
            + start.len();
        let rel_end = src[s..]
            .find(end)
            .unwrap_or_else(|| panic!("end marker '{end}' not found after '{start}'"));
        &src[s..s + rel_end]
    }

    /// Every double-quoted string literal in `src`.
    fn quoted(src: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = src;
        while let Some(open) = rest.find('"') {
            let after = &rest[open + 1..];
            let close = after.find('"').expect("unterminated string literal");
            out.insert(after[..close].to_string());
            rest = &after[close + 1..];
        }
        out
    }

    /// The values of a `export const NAME = { KEY: "value", … };` object.
    fn js_object(name: &str) -> BTreeSet<String> {
        quoted(slice_between(
            CONSTANTS_JS,
            &format!("export const {name} = {{"),
            "};",
        ))
    }

    /// The command names registered in lib.rs's `generate_handler!` block.
    fn registered_commands() -> BTreeSet<String> {
        slice_between(LIB_RS, "generate_handler![", "]")
            .split("commands::")
            .skip(1)
            .map(|s| {
                s.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>()
            })
            .collect()
    }

    fn set(strs: &[&str]) -> BTreeSet<String> {
        strs.iter().copied().map(String::from).collect()
    }

    #[test]
    fn js_commands_match_registered_tauri_commands() {
        assert_eq!(js_object("CMD"), registered_commands());
    }

    #[test]
    fn js_events_match_rust_event_consts() {
        let rust = set(&[
            super::event::STATE,
            super::event::AUDIO_LEVEL,
            super::event::USAGE,
            super::event::HISTORY,
            super::event::MODEL_DOWNLOAD,
            super::event::UPDATE_STAGED,
            super::event::UPDATE_NONE,
        ]);
        assert_eq!(js_object("EVENTS"), rust);
    }

    #[test]
    fn js_download_ids_match_rust_download_consts() {
        let rust = set(&[
            super::download::WHISPER,
            super::download::LLM,
            super::download::KOKORO,
        ]);
        assert_eq!(js_object("DOWNLOAD"), rust);
    }
}
