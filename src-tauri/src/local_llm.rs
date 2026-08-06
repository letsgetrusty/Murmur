// Embedded local LLM (llama.cpp via `llama-cpp-2`) for the refine + macro passes,
// so both can run fully offline instead of calling OpenRouter. One `LocalLlm` is
// shared by `refine::LocalRefiner` and `macros::LocalMatcher`; the GGUF model is
// loaded lazily on first use and cached, and downloaded on first run like the
// Whisper/Kokoro models.
//
// Default model is Qwen3 1.7B (Q4_K_M) — small, fast, and strong at instruction
// following, which is all "clean up this text" and "pick the matching macro"
// need. Qwen3 is a hybrid reasoning model: refine enables its reasoning pass
// (`think = true`, or it just echoes the input), while macro classification
// skips it via a pre-filled empty <think></think> block for speed.

use std::io::Write;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

const REPO: &str = "https://huggingface.co/unsloth/Qwen3-1.7B-GGUF/resolve/main";
/// Upper bound on generated tokens (refine output ≈ the input length; macro
/// output is a single number).
const MAX_TOKENS: i32 = 640;
const N_CTX: u32 = 4096;

/// Local path for a GGUF model by name (e.g. "Qwen3-1.7B-Q4_K_M").
pub fn model_path(name: &str) -> Result<PathBuf> {
    Ok(crate::stt::models_dir()?.join(format!("{name}.gguf")))
}

/// True once the GGUF model file is on disk.
pub fn assets_present(name: &str) -> bool {
    model_path(name).map(|p| p.exists()).unwrap_or(false)
}

/// Download the GGUF model from Hugging Face (unsloth Qwen3 repo) if missing,
/// via a `.part` temp + rename so the final file is always complete.
pub async fn ensure_local_llm(name: &str) -> Result<PathBuf> {
    let path = model_path(name)?;
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(crate::stt::models_dir()?).ok();
    let url = format!("{REPO}/{name}.gguf");
    log::info!("llm: downloading local model '{name}' (~1 GB, one-time)…");
    let mut resp = reqwest::Client::new()
        .get(&url)
        .send()
        .await
        .context("GET llm model")?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "download llm model '{name}' failed: HTTP {}",
            resp.status()
        ));
    }
    let part = path.with_extension("part");
    let mut file = std::fs::File::create(&part).context("create llm .part")?;
    while let Some(chunk) = resp.chunk().await.context("read llm chunk")? {
        file.write_all(&chunk).context("write llm chunk")?;
    }
    file.flush().ok();
    drop(file);
    std::fs::rename(&part, &path).context("finalize llm model")?;
    log::info!("llm: local model '{name}' ready");
    Ok(path)
}

/// Loaded llama.cpp model. Held behind an `Arc` and shared read-only; per-call
/// `LlamaContext`s (which aren't `Send`) are created inside `chat`.
struct Loaded {
    backend: LlamaBackend,
    model: LlamaModel,
}

pub struct LocalLlm {
    model_path: PathBuf,
    loaded: Mutex<Option<Arc<Loaded>>>,
}

impl LocalLlm {
    pub fn new(model_path: PathBuf) -> Self {
        Self {
            model_path,
            loaded: Mutex::new(None),
        }
    }

    /// Load (once) and return the cached model. First call pays the model load
    /// (~1s) + Metal init; later calls reuse it.
    fn loaded(&self) -> Result<Arc<Loaded>> {
        let mut guard = self
            .loaded
            .lock()
            .map_err(|_| anyhow!("llm lock poisoned"))?;
        if let Some(l) = guard.as_ref() {
            return Ok(l.clone());
        }
        if !self.model_path.exists() {
            return Err(anyhow!(
                "local LLM model not found at {} — it downloads on startup; wait a moment and retry",
                self.model_path.display()
            ));
        }
        let backend = LlamaBackend::init().map_err(|e| anyhow!("llama backend init: {e}"))?;
        // Offload all layers to the GPU (Metal on Apple Silicon).
        let params = LlamaModelParams::default().with_n_gpu_layers(999);
        let model = LlamaModel::load_from_file(&backend, &self.model_path, &params)
            .map_err(|e| anyhow!("load llm model: {e}"))?;
        let loaded = Arc::new(Loaded { backend, model });
        *guard = Some(loaded.clone());
        Ok(loaded)
    }

    /// Blocking chat completion: `system` + `user` → assistant text. Greedy
    /// decoding for deterministic output. Call under `spawn_blocking` — it's
    /// CPU/GPU-bound and synchronous.
    ///
    /// `think` enables Qwen3's reasoning pass: editing tasks (refine) need it or
    /// the model just echoes; classification (macros) is fine — and much faster —
    /// without it.
    pub fn chat(&self, system: &str, user: &str, think: bool) -> Result<String> {
        let l = self.loaded()?;
        let model = &l.model;

        // Build Qwen3's ChatML prompt directly — llama.cpp's apply_chat_template
        // truncates multi-line message content. When not thinking, pre-fill an
        // empty `<think></think>` block (Qwen3's own convention) so it answers
        // straight away instead of reasoning.
        let assistant = if think { "" } else { "<think>\n\n</think>\n\n" };
        let prompt = format!(
            "<|im_start|>system\n{system}<|im_end|>\n\
             <|im_start|>user\n{user}<|im_end|>\n\
             <|im_start|>assistant\n{assistant}"
        );
        // Special tokens (<|im_start|> …) are parsed by str_to_token; the ChatML
        // markers stand in for BOS, so we don't add one.
        let tokens = model
            .str_to_token(&prompt, AddBos::Never)
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        if tokens.len() as u32 >= N_CTX {
            return Err(anyhow!("prompt too long for context"));
        }

        let ctx_params = LlamaContextParams::default().with_n_ctx(NonZeroU32::new(N_CTX));
        let mut ctx = model
            .new_context(&l.backend, ctx_params)
            .map_err(|e| anyhow!("new context: {e}"))?;

        // Decode the prompt (logits on the last token).
        let mut batch = LlamaBatch::new(N_CTX as usize, 1);
        let last = tokens.len() as i32 - 1;
        for (i, tok) in tokens.iter().enumerate() {
            batch
                .add(*tok, i as i32, &[0], i as i32 == last)
                .map_err(|e| anyhow!("batch add: {e}"))?;
        }
        ctx.decode(&mut batch)
            .map_err(|e| anyhow!("decode prompt: {e}"))?;

        // Greedy generation loop. Accumulate raw bytes and decode once at the
        // end so multi-byte UTF-8 split across tokens is handled correctly.
        let mut sampler = LlamaSampler::greedy();
        let mut out = Vec::<u8>::new();
        let mut n_cur = tokens.len() as i32;
        let limit = tokens.len() as i32 + MAX_TOKENS;
        loop {
            let token = sampler.sample(&ctx, batch.n_tokens() - 1);
            if model.is_eog_token(token) || n_cur >= limit {
                break;
            }
            if let Ok(bytes) = model.token_to_piece_bytes(token, 32, false, None) {
                out.extend_from_slice(&bytes);
            }
            sampler.accept(token);
            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .map_err(|e| anyhow!("batch add: {e}"))?;
            n_cur += 1;
            ctx.decode(&mut batch).map_err(|e| anyhow!("decode: {e}"))?;
        }
        let text = String::from_utf8_lossy(&out);
        Ok(strip_think(&text).trim().to_string())
    }
}

/// Drop everything up to and including a `</think>` block, in case Qwen3 emits
/// one despite `/no_think`.
fn strip_think(s: &str) -> &str {
    match s.rfind("</think>") {
        Some(pos) => &s[pos + "</think>".len()..],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_think_keeps_only_the_answer() {
        assert_eq!(
            strip_think("<think>\nreasoning\n</think>\nHello."),
            "\nHello."
        );
        assert_eq!(strip_think("no think tags here"), "no think tags here");
    }

    /// End-to-end local generation; needs the GGUF model on disk. Ignored by
    /// default. Run: cargo test --no-default-features -- --ignored llm_chat --nocapture
    #[test]
    #[ignore = "needs the Qwen3 GGUF in <app-support>/murmur/models"]
    fn llm_chat() {
        let path = model_path("Qwen3-1.7B-Q4_K_M").unwrap();
        let llm = LocalLlm::new(path);
        // Refine (editing) needs thinking on, or the model echoes the input.
        let refined = llm
            .chat(
                "You clean up dictated speech. Fix grammar and remove filler words and repetition. Output only the cleaned text.",
                "um so like i i went to the the store yesterday and uh bought some milk you know",
                true,
            )
            .unwrap();
        eprintln!("REFINE OUT: {refined:?}");
        assert!(!refined.is_empty() && !refined.contains("um so"));
        // Macro classification (pick a number) works fast without thinking.
        let pick = llm
            .chat(
                "Reply with ONLY the number of the best-matching command, or 0 if none.",
                "Spoken: \"schedule a call\"\nCommands:\n1. Schedule call\n2. Send address\nReply with the number.",
                false,
            )
            .unwrap();
        eprintln!("MACRO PICK: {pick:?}");
        assert!(pick.contains('1'));
    }
}
