// Standalone STT latency benchmark. Drives whisper.cpp (via whisper-rs) with the
// EXACT params the app uses in stt.rs — flash-attn, greedy best_of=1, no_context,
// temp 0, same thread cap — on a fixed clip, with no TTS/refine running. This
// isolates the engine's floor latency so we can compare models and build profiles
// (debug vs --release) honestly.
//
// Usage: cargo run --example bench_stt --release -- <model> <wav> [iters]
//        e.g. cargo run --example bench_stt --release -- small.en /path/clip.wav 5

use std::time::Instant;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

fn model_path(name: &str) -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    std::path::PathBuf::from(home)
        .join("Library/Application Support/murmur/models")
        .join(format!("ggml-{name}.bin"))
}

// Minimal 16-bit PCM mono WAV → f32 (clip is s16le/16k/mono from afconvert).
fn read_wav_mono_f32(path: &str) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("read wav");
    let pos = bytes
        .windows(4)
        .position(|w| w == b"data")
        .expect("no data chunk");
    let data = &bytes[pos + 8..];
    data.chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
        .collect()
}

fn threads() -> std::os::raw::c_int {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8) as std::os::raw::c_int)
        .unwrap_or(4)
}

fn run(ctx: &WhisperContext, samples: &[f32]) -> f32 {
    let mut state = ctx.create_state().expect("state");
    let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    p.set_language(Some("en"));
    p.set_translate(false);
    p.set_no_context(true);
    p.set_temperature(0.0);
    p.set_temperature_inc(0.0);
    p.set_n_threads(threads());
    p.set_print_special(false);
    p.set_print_progress(false);
    p.set_print_realtime(false);
    p.set_print_timestamps(false);
    let t = Instant::now();
    state.full(p, samples).expect("infer");
    t.elapsed().as_secs_f32() * 1000.0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let model = args.get(1).map(String::as_str).unwrap_or("small.en");
    let wav = args
        .get(2)
        .map(String::as_str)
        .expect("usage: bench_stt <model> <wav> [iters]");
    let iters: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);

    let samples = read_wav_mono_f32(wav);
    let audio_secs = samples.len() as f32 / 16_000.0;
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!(
        "\n== bench_stt [{profile}] model={model} audio={audio_secs:.1}s iters={iters} threads={} ==",
        threads()
    );

    let mut cparams = WhisperContextParameters::default();
    cparams.flash_attn(true);
    let t_load = Instant::now();
    let ctx =
        WhisperContext::new_with_params(model_path(model).to_str().expect("path utf8"), cparams)
            .expect("load model");
    println!(
        "model load: {:.0}ms",
        t_load.elapsed().as_secs_f32() * 1000.0
    );

    let cold = run(&ctx, &samples);
    println!(
        "cold infer (graph compile): {:.0}ms ({:.1}x realtime)",
        cold,
        audio_secs / (cold / 1000.0)
    );

    let mut best = f32::MAX;
    let mut sum = 0.0;
    for i in 0..iters {
        let ms = run(&ctx, &samples);
        best = best.min(ms);
        sum += ms;
        println!(
            "  warm iter {i}: {:.0}ms ({:.1}x realtime)",
            ms,
            audio_secs / (ms / 1000.0)
        );
    }
    let avg = sum / iters as f32;
    let avg_rt = audio_secs / (avg / 1000.0);
    println!(
        "WARM avg: {:.0}ms ({avg_rt:.1}x realtime) | best: {:.0}ms ({:.1}x realtime)",
        avg,
        best,
        audio_secs / (best / 1000.0)
    );

    // Release gate: fail if warm throughput fell below the floor (set by
    // scripts/bench.sh). Catches a model-throttle or slow-decode regression —
    // e.g. medium.en dropping to ~2.7x avg where small.en holds ~26x.
    if let Ok(min) = std::env::var("MURMUR_GATE_MIN_REALTIME") {
        let min: f32 = min.parse().unwrap_or(0.0);
        if avg_rt < min {
            eprintln!("GATE FAIL: STT {avg_rt:.1}x realtime < required {min:.1}x");
            std::process::exit(1);
        }
        println!("GATE OK: STT {avg_rt:.1}x realtime ≥ {min:.1}x");
    }
}
