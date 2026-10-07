//! Local release-only harness calling production speech code. No microphone,
//! clipboard, history, config writes, or network. Raw measurements go to stdout.
use crate::{
    audio, config,
    kokoro::KokoroTts,
    stt::{self, Transcriber, WhisperStt},
    tts,
};
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Deserialize)]
struct Corpus {
    stt: Vec<SttCase>,
    tts: Vec<TtsCase>,
}
#[derive(Deserialize)]
struct SttCase {
    id: String,
    wav: String,
    #[serde(default)]
    replay: bool,
}
#[derive(Deserialize)]
struct TtsCase {
    id: String,
    text: String,
    voice: String,
}
fn emit(row: Value) {
    println!("BENCH {row}");
}

pub fn run() -> Result<()> {
    ensure!(
        !cfg!(debug_assertions),
        "speech measurements require a release build"
    );
    let args: Vec<String> = std::env::args().collect();
    ensure!(
        args.len() == 7,
        "bench_speech <stt|tts|mixed> <root> <corpus> <rounds> <idle_secs> <artifact_dir>"
    );
    let root = PathBuf::from(&args[2]);
    let corpus: Corpus = serde_json::from_slice(&std::fs::read(&args[3])?)?;
    let rounds: usize = args[4].parse()?;
    let idle: u64 = args[5].parse()?;
    ensure!(rounds > 0, "rounds must be positive");
    let artifacts = PathBuf::from(&args[6]);
    std::fs::create_dir_all(&artifacts)?;
    // Explicit, version-controlled pronunciation dictionary; no user settings.
    std::env::set_var(
        "KOKORO_G2P_LEXICON",
        root.join("src-tauri/src/dev_terms.tab"),
    );
    std::env::set_var("KOKORO_ORT_PROVIDER", "coreml");
    std::env::set_var("KOKORO_COREML_COMPUTE_UNITS", "cpu_and_gpu");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        match args[1].as_str() {
            "stt" => measure_stt(&root, &corpus.stt, rounds, idle).await,
            "tts" => measure_tts(&artifacts, &corpus.tts, rounds, idle).await,
            "mixed" => measure_mixed(&root, &artifacts, &corpus, rounds).await,
            _ => anyhow::bail!("unknown engine"),
        }
    })
}

async fn measure_stt(root: &Path, cases: &[SttCase], rounds: usize, idle: u64) -> Result<()> {
    let engine = Arc::new(WhisperStt::new("small.en"));
    engine.set_vocabulary(config::DEFAULT_DICTATION_VOCABULARY);
    engine.set_corrections(config::DEFAULT_DICTATION_CORRECTIONS);
    let clips: Vec<_> = cases
        .iter()
        .map(|case| {
            let wav = std::fs::read(root.join(&case.wav))?;
            let samples = stt::wav_to_mono_f32(&wav)?;
            Ok((case, wav, samples))
        })
        .collect::<Result<_>>()?;
    // First case includes lazy model initialization. Every process is fresh,
    // but this does not flush macOS file caches or simulate a machine reboot.
    for (phase, count) in [("first", 1), ("warm", rounds), ("after_idle", 1)] {
        if phase == "after_idle" {
            tokio::time::sleep(Duration::from_secs(idle)).await;
        }
        for round in 0..count {
            for (case, wav, samples) in &clips {
                let start = Instant::now();
                let text = engine.transcribe(wav).await?;
                emit(
                    json!({"engine":"stt", "phase":phase,"round":round,"case":case.id,
                    "wall_ms":start.elapsed().as_secs_f64()*1000.0,
                    "audio_secs":samples.len() as f64 / 16000.0,"text":text}),
                );
            }
        }
    }
    // Replay actual samples at real-time pace through production snapshot /
    // release logic. A batch-only benchmark would miss the punctuation bug.
    for (case, wav, samples) in &clips {
        if !case.replay {
            continue;
        }
        let feed = audio::AudioFeed::test_samples(Vec::new());
        let live = stt::LiveTranscription::start(feed.clone(), engine.clone());
        let began = tokio::time::Instant::now();
        for (index, frame) in samples.chunks(1600).enumerate() {
            tokio::time::sleep_until(began + Duration::from_millis((index as u64 + 1) * 100)).await;
            feed.append_benchmark_samples(frame);
        }
        let release = Instant::now();
        let text = live.finish(wav).await?;
        emit(
            json!({"engine":"stt","phase":"replay","round":0,"case":case.id,
            "wall_ms":release.elapsed().as_secs_f64()*1000.0,
            "audio_secs":samples.len() as f64 / 16000.0,"text":text}),
        );
    }
    Ok(())
}

// Software queue simulation: excludes file I/O, AVQueuePlayer, device startup,
// and OS scheduling. These are explicitly estimates, not audible latency.
fn simulate(chunks: &[(f64, f64, usize)], speed: f64) -> (f64, f64) {
    let mut now = 0.0;
    let mut buffer: f64 = 0.0;
    let mut playing = false;
    let mut first = None;
    let mut starved_at = None;
    let mut stalls = 0.0;
    let mut per_char = 0.0;
    for (i, &(ms, secs, chars)) in chunks.iter().enumerate() {
        let dt = ms / 1000.0;
        if playing {
            if buffer / speed < dt {
                starved_at = Some(now + buffer / speed);
                buffer = 0.0;
                playing = false;
            } else {
                buffer -= dt * speed;
            }
        }
        now += dt;
        buffer += secs;
        let observed = dt / chars.max(1) as f64;
        per_char = if i == 0 {
            observed
        } else {
            (per_char + observed) * 0.5
        };
        let target = tts::buffer_target(
            speed as f32,
            per_char as f32,
            chunks.get(i + 1).map_or(0, |c| c.2),
        );
        if !playing && (buffer >= target as f64 || i + 1 == chunks.len()) {
            if let Some(start) = starved_at.take() {
                stalls += now - start;
            }
            first.get_or_insert(now * 1000.0);
            playing = true;
        }
    }
    (first.unwrap_or(0.0), stalls * 1000.0)
}

async fn measure_tts(artifacts: &Path, cases: &[TtsCase], rounds: usize, idle: u64) -> Result<()> {
    let load = Instant::now();
    let engine = KokoroTts::new(tts::kokoro_model_path()?, tts::kokoro_voices_dir()?).await?;
    emit(
        json!({"engine":"tts","phase":"load","round":0,"case":"model","wall_ms":load.elapsed().as_secs_f64()*1000.0}),
    );
    for (phase, count) in [("first", 1), ("warm", rounds), ("after_idle", 1)] {
        if phase == "after_idle" {
            tokio::time::sleep(Duration::from_secs(idle)).await;
        }
        for round in 0..count {
            for case in cases {
                measure_tts_case(&engine, artifacts, case, phase, round).await?;
            }
        }
    }
    Ok(())
}

async fn measure_tts_case(
    engine: &KokoroTts,
    artifacts: &Path,
    case: &TtsCase,
    phase: &str,
    round: usize,
) -> Result<()> {
    let start = Instant::now();
    let chunks = tts::split_for_tts(&tts::normalize_for_tts(&case.text));
    let mut timeline = Vec::new();
    let mut pcm = Vec::new();
    for (i, (text, hard)) in chunks.iter().enumerate() {
        let gap = if i + 1 == chunks.len() {
            0
        } else if *hard {
            tts::SENTENCE_GAP_MS
        } else {
            tts::SOFT_GAP_MS
        };
        let tick = Instant::now();
        let wav = tts::synth_chunk_wav(engine, text, &case.voice, gap)
            .await
            .context("TTS chunk failed")?;
        let ms = tick.elapsed().as_secs_f64() * 1000.0;
        let samples = stt::wav_to_mono_f32(&wav)?; // PCM reader; no resampling
        timeline.push((ms, samples.len() as f64 / 24000.0, text.chars().count()));
        pcm.extend(samples);
    }
    let wall_ms = start.elapsed().as_secs_f64() * 1000.0;
    ensure!(
        !pcm.is_empty() && pcm.iter().all(|s| s.is_finite()),
        "invalid TTS waveform"
    );
    let rms = (pcm.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / pcm.len() as f64).sqrt();
    ensure!(rms > 0.001, "silent TTS output");
    let hash = pcm.iter().fold(0xcbf29ce484222325u64, |h, v| {
        (h ^ u64::from(v.to_bits())).wrapping_mul(0x100000001b3)
    });
    let (ready1, stall1) = simulate(&timeline, 1.0);
    let (ready2, stall2) = simulate(&timeline, 2.0);
    emit(
        json!({"engine":"tts","phase":phase,"round":round,"case":case.id,
                    "wall_ms":wall_ms,"audio_secs":pcm.len() as f64/24000.0,"rms":rms,"waveform_hash":hash.to_string(),
                    "first_chunk_ms":timeline[0].0,"buffer_ready_1x_ms":ready1,"buffer_ready_2x_ms":ready2,
                    "estimated_stall_1x_ms":stall1,"estimated_stall_2x_ms":stall2,"chunks":chunks.len()}),
    );
    if phase == "first" || (phase == "coexist" && round == 0) {
        std::fs::write(
            artifacts.join(format!("{}.wav", case.id)),
            audio::encode_wav_i16(&pcm, 24000)?,
        )?;
    }
    Ok(())
}

// Same-process overlap catches resource contention and process-wide settings
// that isolated STT/TTS runs miss. This is a stress scenario, not a UI gesture.
async fn measure_mixed(
    root: &Path,
    artifacts: &Path,
    corpus: &Corpus,
    rounds: usize,
) -> Result<()> {
    let stt_case = corpus
        .stt
        .iter()
        .find(|c| c.id == "pause")
        .context("missing pause case")?;
    let tts_case = corpus
        .tts
        .iter()
        .find(|c| c.id == "paragraph")
        .context("missing paragraph case")?;
    let wav = std::fs::read(root.join(&stt_case.wav))?;
    let audio_secs = stt::wav_to_mono_f32(&wav)?.len() as f64 / 16000.0;
    let stt = Arc::new(WhisperStt::new("small.en"));
    stt.set_vocabulary(config::DEFAULT_DICTATION_VOCABULARY);
    stt.set_corrections(config::DEFAULT_DICTATION_CORRECTIONS);
    stt.transcribe(&wav).await?;
    let tts = KokoroTts::new(tts::kokoro_model_path()?, tts::kokoro_voices_dir()?).await?;
    for round in 0..rounds {
        let engine = stt.clone();
        let input = wav.clone();
        let case_id = stt_case.id.clone();
        let transcribe = tokio::spawn(async move {
            let start = Instant::now();
            let text = engine.transcribe(&input).await?;
            emit(
                json!({"engine":"stt","case":case_id,"phase":"coexist","round":round,
                "wall_ms":start.elapsed().as_secs_f64()*1000.0,"audio_secs":audio_secs,"text":text}),
            );
            Ok::<_, anyhow::Error>(())
        });
        let synthesis = measure_tts_case(&tts, artifacts, tts_case, "coexist", round).await;
        transcribe.await.context("concurrent STT task")??;
        synthesis?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::simulate;

    #[test]
    fn queue_estimate_counts_synthesis_and_speed_dependent_underflow() {
        // First chunk clears the predicted target; next synthesis is unexpectedly slow.
        let chunks = [(100.0, 2.0, 100), (3000.0, 2.0, 100)];
        assert_eq!(simulate(&chunks, 1.0), (100.0, 1000.0));
        assert_eq!(simulate(&chunks, 2.0), (100.0, 2000.0));
    }

    #[test]
    fn queue_estimate_waits_for_buffer_but_always_plays_final_chunk() {
        assert_eq!(simulate(&[(500.0, 0.1, 10)], 2.0), (500.0, 0.0));
        assert_eq!(
            simulate(&[(500.0, 0.1, 10), (500.0, 0.1, 10)], 2.0),
            (1000.0, 0.0)
        );
    }
}
