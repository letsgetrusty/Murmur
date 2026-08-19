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

async fn synth_once(tts: &KokoroTts, text: &str, voice: &str, label: &str) {
    let t = Instant::now();
    let (samples, _) = tts.synth(text, voice).await.expect("synth");
    let ms = t.elapsed().as_secs_f32() * 1000.0;
    let audio = samples.len() as f32 / SR;
    // Edge silence: samples below ~-50 dBFS at head/tail. If Kokoro pads each
    // chunk, trimming it makes small-chunk seams gapless.
    const THRESH: f32 = 0.003;
    let lead = samples.iter().take_while(|s| s.abs() < THRESH).count();
    let tail = samples
        .iter()
        .rev()
        .take_while(|s| s.abs() < THRESH)
        .count();
    println!(
        "  {label}: {:.0}ms → {:.2}s audio ({:.1}x realtime) [edge silence: lead {:.0}ms, tail {:.0}ms]",
        ms,
        audio,
        audio / (ms / 1000.0),
        lead as f32 / SR * 1000.0,
        tail as f32 / SR * 1000.0,
    );
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
        for i in 0..iters {
            synth_once(&tts, sentence, voice, &format!("iter {i}")).await;
        }

        println!("-- warm: tiny opening fragment (= fast-first-chunk cost) --");
        for i in 0..3 {
            synth_once(&tts, tiny, voice, &format!("tiny {i}")).await;
        }
    });
}
