// Standalone Kokoro TTS latency benchmark. Drives the same `kokoro-en` backend
// the app uses, sweeping the CoreML compute path (KOKORO_COREML_COMPUTE_UNITS)
// to compare eligible-device policies and latency under sustained load.
// Policy and elapsed time do not identify which device executes each operator.
//
// Usage: cargo run --example bench_tts --release -- <compute_units> [iters]
//   compute_units ∈ { cpu_and_gpu, cpu_and_neural_engine, all, cpu_only, cpu }
// `cpu` uses ONNX Runtime's CPU provider; `cpu_only` uses CoreML on CPU.
// MURMUR_TTS_PROFILE=1 adds identical short/medium/long cases, post-idle runs,
// and machine-readable PROFILE rows with separate inference/other timings.

#[allow(dead_code)]
#[path = "../src/kokoro.rs"]
mod production;

#[path = "support/dispatch.rs"]
mod dispatch;
#[path = "support/runtime.rs"]
mod tts_runtime;

use kokoro_en::KokoroTts;
use std::time::Instant;

const SR: f32 = 24_000.0;

fn models_dir() -> std::path::PathBuf {
    let home = std::env::var("HOME").expect("HOME");
    std::path::PathBuf::from(home).join("Library/Application Support/murmur/models")
}

/// Returns `(realtime_factor, max_edge_ms)` for gating in main.
async fn synth_once(tts: &mut Engine, text: &str, voice: &str, label: &str) -> (f32, f32) {
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
    match compute.as_str() {
        "cpu" => std::env::set_var("KOKORO_ORT_PROVIDER", "cpu"),
        "cpu_and_gpu" | "cpu_and_neural_engine" | "all" | "cpu_only" => {
            std::env::set_var("KOKORO_ORT_PROVIDER", "coreml");
            std::env::set_var("KOKORO_COREML_COMPUTE_UNITS", &compute);
        }
        _ => panic!("unknown compute setting: {compute}"),
    }
    assert!(iters > 0, "iterations must be positive");

    let mut environment = ort::init();
    if std::env::var("MURMUR_TTS_RUNTIME").as_deref() == Ok("tuned")
        || std::env::var_os("MURMUR_TTS_THREADS").is_some()
    {
        let mut settings = tts_runtime::RuntimeSettings::default();
        if let Ok(threads) = std::env::var("MURMUR_TTS_THREADS") {
            settings.threads = threads.parse().expect("thread count");
        }
        assert!(
            settings.threads != 1,
            "async inference needs at least two threads"
        );
        if let Ok(spin) = std::env::var("MURMUR_TTS_SPIN") {
            settings.spin = spin == "1";
        }
        if let Ok(flush) = std::env::var("MURMUR_TTS_FLUSH") {
            settings.flush_denormals = flush == "1";
        }
        let mut pool = settings.pool().expect("thread pool options");
        let qos = std::env::var("MURMUR_TTS_QOS").as_deref() == Ok("1");
        if qos {
            pool = pool
                .with_thread_manager(UserInitiatedThreads)
                .expect("thread manager");
        }
        environment = environment.with_global_thread_pool(pool);
        eprintln!("kokoro bench | intra_threads={}, inter_threads=1, spinning={}, user_initiated={qos}, flush_denormals={}", settings.threads, settings.spin, settings.flush_denormals);
    }
    if std::env::var("MURMUR_TTS_TRACE").as_deref() == Ok("1") {
        environment = environment.with_logger(std::sync::Arc::new(
            |level, category, id, location, message| {
                eprintln!("ORT {level:?} {category} {id} {location}: {message}");
            },
        ));
    }
    assert!(environment.commit(), "ORT environment already configured");

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
        let mut tts = match std::env::var("MURMUR_TTS_DISPATCH").as_deref() {
            Ok("production") => Engine::Production(
                production::KokoroTts::new(&model, &voices)
                    .await
                    .expect("load production worker"),
            ),
            Ok("worker") => Engine::Worker(
                dispatch::Worker::new(&model, &voices, voice, &compute)
                    .await
                    .expect("load worker"),
            ),
            Ok(mode @ ("sync" | "async")) => Engine::Direct(
                dispatch::DirectSession::new(&model, &voices, voice, &compute, mode == "async")
                    .expect("load direct session"),
            ),
            _ => Engine::Kokoro(KokoroTts::new(&model, &voices).await.expect("load kokoro")),
        };
        let load_ms = t_load.elapsed().as_secs_f64() * 1000.0;
        println!("model load: {load_ms:.0}ms");
        if std::env::var("MURMUR_TTS_PROFILE").as_deref() == Ok("1") {
            profile_compute(&mut tts, &compute, voice, iters, load_ms).await;
            return;
        }

        // Cold: first synth compiles the CoreML graph.
        synth_once(&mut tts, sentence, voice, "cold sentence (graph compile)").await;

        println!("-- warm: full first sentence (= current time-to-first-word) --");
        let mut min_rt = f32::MAX;
        let mut max_edge = 0f32;
        for i in 0..iters {
            let (rt, edge) = synth_once(&mut tts, sentence, voice, &format!("iter {i}")).await;
            min_rt = min_rt.min(rt);
            max_edge = max_edge.max(edge);
        }

        println!("-- warm: tiny opening fragment (= fast-first-chunk cost) --");
        for i in 0..3 {
            synth_once(&mut tts, tiny, voice, &format!("tiny {i}")).await;
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

/// The duration returned by kokoro-en encloses ONNX run_async + completion,
/// but excludes its session mutex, voice lookup, G2P, and tensor preparation.
/// It does not identify which processor CoreML used for individual operators.
async fn profile_compute(tts: &mut Engine, compute: &str, voice: &str, iters: usize, load_ms: f64) {
    let cases = [
        ("short", "Local dictation should feel instant,"),
        ("medium", "Local dictation should feel instant, and read aloud should start speaking right away."),
        ("long", "Local dictation should feel instant and reading a long opening sentence without commas should start quickly while the rest of the text is generated in the background."),
    ];
    let idle_secs: u64 = std::env::var("MURMUR_TTS_IDLE_SECS")
        .ok()
        .map(|s| s.parse().expect("idle seconds"))
        .unwrap_or(15);
    for (phase, rounds) in [("first", 1), ("warm", iters), ("after_idle", 1)] {
        if phase == "after_idle" {
            tokio::time::sleep(std::time::Duration::from_secs(idle_secs)).await;
        }
        for round in 0..rounds {
            for (case, text) in cases {
                let started = Instant::now();
                let (samples, inference) = tts.synth(text, voice).await.expect("profile synth");
                let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
                let inference_ms = inference.as_secs_f64() * 1000.0;
                assert!(
                    samples.len() > 1000 && samples.iter().all(|s| s.is_finite()),
                    "invalid waveform"
                );
                let audio_secs = samples.len() as f64 / SR as f64;
                let rms = (samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>()
                    / samples.len() as f64)
                    .sqrt();
                assert!(rms > 0.001, "silent waveform");
                println!(
                    "PROFILE {}",
                    serde_json::json!({
                        "compute": compute, "voice": voice, "phase": phase, "round": round,
                        "case": case, "chars": text.chars().count(), "load_ms": load_ms,
                        "wall_ms": wall_ms, "inference_ms": inference_ms,
                        "other_ms": (wall_ms - inference_ms).max(0.0),
                        "audio_secs": audio_secs, "realtime": audio_secs * 1000.0 / wall_ms,
                        "rms": rms, "idle_secs": idle_secs,
                        "waveform_hash": samples.iter().fold(0xcbf29ce484222325u64, |hash, sample| (hash ^ u64::from(sample.to_bits())).wrapping_mul(0x100000001b3)).to_string(),
                        "dispatch": std::env::var("MURMUR_TTS_DISPATCH").unwrap_or_else(|_| "kokoro".into()),
                        "runtime": std::env::var("MURMUR_TTS_RUNTIME").unwrap_or_else(|_| "legacy".into()),
                        "threads": std::env::var("MURMUR_TTS_THREADS").unwrap_or_else(|_| "0".into()),
                        "spin": std::env::var("MURMUR_TTS_SPIN").unwrap_or_else(|_| "1".into()),
                        "qos": std::env::var("MURMUR_TTS_QOS").unwrap_or_else(|_| "0".into()),
                        "flush": std::env::var("MURMUR_TTS_FLUSH").unwrap_or_else(|_| "0".into()),
                    })
                );
            }
        }
    }
}

struct UserInitiatedThreads;
impl ort::environment::ThreadManager for UserInitiatedThreads {
    type Thread = std::thread::JoinHandle<()>;
    fn create(&self, work: impl FnOnce() + Send + 'static) -> ort::Result<Self::Thread> {
        std::thread::Builder::new()
            .name("kokoro-inference".into())
            .spawn(move || {
                extern "C" {
                    fn pthread_set_qos_class_self_np(class: u32, priority: i32) -> i32;
                }
                // SAFETY: macOS SDK declares these integer arguments; 0x19 is
                // QOS_CLASS_USER_INITIATED, with default relative priority zero.
                let result = unsafe { pthread_set_qos_class_self_np(0x19, 0) };
                assert_eq!(result, 0, "set inference QoS");
                work();
            })
            .map_err(|e| ort::Error::new(e.to_string()))
    }
    fn join(thread: Self::Thread) -> ort::Result<()> {
        thread
            .join()
            .map_err(|_| ort::Error::new("inference thread panicked"))
    }
}

enum Engine {
    Kokoro(KokoroTts),
    Direct(dispatch::DirectSession),
    Worker(dispatch::Worker),
    Production(production::KokoroTts),
}
impl Engine {
    async fn synth(
        &mut self,
        text: &str,
        voice: &str,
    ) -> anyhow::Result<(Vec<f32>, std::time::Duration)> {
        match self {
            Self::Kokoro(tts) => Ok(tts.synth(text, voice).await?),
            Self::Direct(tts) => tts.synth(text).await,
            Self::Worker(tts) => tts.synth(text).await,
            Self::Production(tts) => tts.synth(text, voice).await,
        }
    }
}
