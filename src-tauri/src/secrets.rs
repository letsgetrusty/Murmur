// Phase 1+: API keys live in the macOS Keychain via `keyring`, never in config
// files or logs.

use anyhow::{Context, Result};

const SERVICE: &str = "murmur";

pub const GROQ_API_KEY: &str = "groq_api_key";
pub const ELEVENLABS_API_KEY: &str = "elevenlabs_api_key";
pub const OPENROUTER_API_KEY: &str = "openrouter_api_key";

pub fn get(name: &str) -> Result<String> {
    let entry = keyring::Entry::new(SERVICE, name).context("create keyring entry")?;
    entry.get_password().context("read keyring entry")
}

pub fn set(name: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, name).context("create keyring entry")?;
    entry.set_password(value).context("write keyring entry")
}
