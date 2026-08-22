// Standalone Kokoro TTS latency benchmark. Drives the same `kokoro-en` backend
// the app uses, sweeping the CoreML compute path (KOKORO_COREML_COMPUTE_UNITS)
// so we can see ANE vs GPU vs CPU synth speed, whether it throttles under
// sustained load, and what a tiny "fast first chunk" would actually cost.
//
// Usage: cargo run --example bench_tts --release -- <compute_units> [iters]
//   compute_units ∈ { cpu_and_gpu (app default), all (ANE), cpu_only }

use kokoro_en::KokoroTts;
use std::time::Instant;

const SR: f32 = 24_000.0;

fn models_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    std::path::PathBuf::from(home).join("Library/Application Support/murmur/models")
}

/// Returns `(realtime_factor, max_edge_ms)` for gating in main.
async fn synth_once(tts: &KokoroTts, text: &str, voice: &str, label: &str) -> (f32, f32) {
    let t = Instant::now();
    let (samples, _) = tts.synth(text, voice).await.expect("synth");
    let ms = t.elapsed().as_secs_f32() * 1000.0;
    let audio = samples.len() as f32 / SR;
    let realtime = audio / (ms / 1000.0);
    // Edge silence: samples below ~-50 dBFS at head/tail. If Kokoro pads each
    // chunk, trimming it makes small-chunk seams gapless.
    const THRESH: f32 = 0.003;
    let lead = samples.iter().take_while(|s| s.abs() < THRESH).count();
    let tail = samples
        .iter()
        .rev()
        .take_while(|s| s.abs() < THRESH)
        .count();
    let lead_ms = lead as f32 / SR * 1000.0;
    let tail_ms = tail as f32 / SR * 1000.0;
    println!(
        "  {label}: {:.0}ms → {:.2}s audio ({realtime:.1}x realtime) [edge silence: lead {lead_ms:.0}ms, tail {tail_ms:.0}ms]",
        ms, audio,
    );
    (realtime, lead_ms.max(tail_ms))
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let compute = args.get(1).cloned().unwrap_or_else(|| "cpu_and_gpu".into());
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    // Must be set BEFORE KokoroTts::new — the crate reads it when it builds the
    // CoreML session.
    std::env::set_var("KOKORO_COREML_COMPUTE_UNITS", &compute);

    let voice = "am_puck";
    let model = models_dir().join("kokoro-v1.0.onnx");
    let voices = models_dir().join("kokoro-voices");

    // A representative first sentence (what time-to-first-word pays today) and a
    // tiny opening fragment (what a fast-first-chunk would pay instead).
    let sentence =
        "Local dictation should feel instant, and read aloud should start speaking right away.";
    let tiny = "Local dictation should feel instant,";

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async move {
        let profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        };
        println!(
            "\n== bench_tts [{profile}] compute_units={compute} voice={voice} iters={iters} =="
        );

        let t_load = Instant::now();
        let tts = KokoroTts::new(&model, &voices).await.expect("load kokoro");
        println!(
            "model load: {:.0}ms",
            t_load.elapsed().as_secs_f32() * 1000.0
        );

        // Cold: first synth compiles the CoreML graph.
        synth_once(&tts, sentence, voice, "cold sentence (graph compile)").await;

        println!("-- warm: full first sentence (= current time-to-first-word) --");
        let mut min_rt = f32::MAX;
        let mut max_edge = 0f32;
        for i in 0..iters {
            let (rt, edge) = synth_once(&tts, sentence, voice, &format!("iter {i}")).await;
            min_rt = min_rt.min(rt);
            max_edge = max_edge.max(edge);
        }

        println!("-- warm: tiny opening fragment (= fast-first-chunk cost) --");
        for i in 0..3 {
            synth_once(&tts, tiny, voice, &format!("tiny {i}")).await;
        }

        let _ = max_edge; // measured for the log; the trim itself is unit-tested.

        // Release gate (threshold from scripts/bench.sh). Catches a synth-speed
        // regression — e.g. CoreML falling back to CPU, or a model swap that
        // drops read-aloud below real time so playback stalls mid-sentence.
        if let Ok(min) = std::env::var("MURMUR_GATE_MIN_REALTIME") {
            let min: f32 = min.parse().unwrap_or(0.0);
            if min_rt < min {
                eprintln!("GATE FAIL: TTS synth {min_rt:.1}x realtime < required {min:.1}x");
                std::process::exit(1);
            }
            println!("GATE OK: TTS synth {min_rt:.1}x realtime ≥ {min:.1}x");
        }
    });
}
