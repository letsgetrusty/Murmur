//! Kokoro v1.0 inference on one persistent blocking worker. Text/phoneme logic
//! stays in kokoro-en (without its GPL espeak feature); ORT runs synchronously.
use anyhow::{bail, ensure, Context, Result};
use ort::{
    ep::{
        coreml::{ComputeUnits, ModelFormat},
        CoreML,
    },
    session::Session,
    value::Tensor,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

#[derive(Clone)]
pub struct Cancellation {
    generation: std::sync::Arc<std::sync::atomic::AtomicU64>,
    expected: u64,
}
impl Cancellation {
    pub fn new(generation: std::sync::Arc<std::sync::atomic::AtomicU64>, expected: u64) -> Self {
        Self {
            generation,
            expected,
        }
    }
    pub fn is_cancelled(&self) -> bool {
        self.generation.load(std::sync::atomic::Ordering::Acquire) != self.expected
    }
}

type Audio = (Vec<f32>, Duration);
struct Request {
    text: String,
    voice: String,
    response: oneshot::Sender<Result<Audio>>,
    cancellation: Option<Cancellation>,
}

impl Request {
    fn cancelled(&self) -> bool {
        self.response.is_closed()
            || self
                .cancellation
                .as_ref()
                .is_some_and(Cancellation::is_cancelled)
    }
}

pub struct KokoroTts {
    requests: mpsc::Sender<Request>,
}
impl KokoroTts {
    pub async fn new(model: impl AsRef<Path>, voices: impl AsRef<Path>) -> Result<Self> {
        let model = model.as_ref().to_owned();
        let voices = voices.as_ref().to_owned();
        // Bound queued text and apply asynchronous backpressure to callers.
        let (requests, receiver) = mpsc::channel(8);
        let (ready, initialized) = oneshot::channel();
        std::thread::Builder::new()
            .name("kokoro-inference".into())
            .spawn(move || {
                let mut engine = match Engine::new(&model, voices) {
                    Ok(engine) => engine,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                if ready.send(Ok(())).is_err() {
                    return;
                }
                log::info!("tts/kokoro: synchronous inference worker ready");
                serve(receiver, |request| engine.synth(request));
                // Session and cached voices are released here when all callers drop.
            })
            .context("start Kokoro worker")?;
        initialized
            .await
            .context("Kokoro worker stopped during initialization")??;
        Ok(Self { requests })
    }

    pub async fn synth(&self, text: &str, voice: &str) -> Result<Audio> {
        self.synth_cancellable(text, voice, None).await
    }

    pub async fn synth_cancellable(
        &self,
        text: &str,
        voice: &str,
        cancellation: Option<Cancellation>,
    ) -> Result<Audio> {
        let (response, result) = oneshot::channel();
        self.requests
            .send(Request {
                text: text.into(),
                voice: voice.into(),
                response,
                cancellation,
            })
            .await
            .map_err(|_| anyhow::anyhow!("Kokoro worker stopped"))?;
        result
            .await
            .context("Kokoro worker stopped during synthesis")?
    }
}

fn serve(mut receiver: mpsc::Receiver<Request>, mut synth: impl FnMut(&Request) -> Result<Audio>) {
    while let Some(request) = receiver.blocking_recv() {
        if request.cancelled() {
            let _ = request
                .response
                .send(Err(anyhow::anyhow!("Kokoro request cancelled")));
            continue;
        }
        let result = synth(&request);
        let _ = request.response.send(result);
    }
}

struct Engine {
    session: Session,
    voices_dir: PathBuf,
    voices: HashMap<String, Vec<f32>>,
}
impl Engine {
    fn new(model: &Path, voices_dir: PathBuf) -> Result<Self> {
        ensure!(voices_dir.is_dir(), "Kokoro voice directory is missing");
        let provider = std::env::var("KOKORO_ORT_PROVIDER")
            .unwrap_or_else(|_| "auto".into())
            .to_ascii_lowercase();
        ensure!(
            matches!(provider.as_str(), "auto" | "coreml" | "cpu"),
            "unknown Kokoro provider: {provider}"
        );
        let session = match build_session(model, provider != "cpu") {
            Ok(session) => session,
            Err(error) if provider == "auto" => {
                log::warn!("tts/kokoro: CoreML failed ({error:#}); falling back to CPU");
                build_session(model, false).context("Kokoro CPU fallback failed")?
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            session,
            voices_dir,
            voices: HashMap::new(),
        })
    }

    fn synth(&mut self, request: &Request) -> Result<Audio> {
        ensure!(valid_voice_id(&request.voice), "invalid Kokoro voice id");
        if !self.voices.contains_key(&request.voice) {
            let path = self.voices_dir.join(format!("{}.bin", request.voice));
            let bytes =
                std::fs::read(&path).with_context(|| format!("read voice {}", request.voice))?;
            self.voices
                .insert(request.voice.clone(), decode_voice(&bytes)?);
        }
        let pack = &self.voices[&request.voice];
        let phonemes = kokoro_en::g2p(&request.text, false)?;
        let mut audio = Vec::new();
        let mut elapsed = Duration::ZERO;
        for chunk in kokoro_en::chunk_phonemes(&phonemes, kokoro_en::MAX_PHONEME_CHARS) {
            // An already-running native call finishes, but cancelled requests
            // never start another phoneme chunk or publish their result.
            ensure!(!request.cancelled(), "Kokoro request cancelled");
            let tokens = kokoro_en::get_token_ids(&chunk, false);
            let start = tokens
                .len()
                .checked_sub(1)
                .context("empty token sequence")?
                * 256;
            let style = pack
                .get(start..start + 256)
                .context("Kokoro voice pack is too short")?
                .to_vec();
            let (samples, duration) = run(&mut self.session, tokens, style)?;
            ensure!(
                samples.iter().all(|sample| sample.is_finite()),
                "Kokoro produced invalid audio"
            );
            audio.extend(samples);
            elapsed += duration;
        }
        Ok((audio, elapsed))
    }
}

fn valid_voice_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn decode_voice(bytes: &[u8]) -> Result<Vec<f32>> {
    // Murmur downloads raw little-endian [N,256] f32 packs for Kokoro v1.0.
    ensure!(
        !bytes.is_empty() && bytes.len() % 1024 == 0,
        "invalid Kokoro voice pack size"
    );
    let pack: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
        .collect();
    ensure!(
        pack.iter().all(|v| v.is_finite()),
        "invalid Kokoro voice values"
    );
    Ok(pack)
}

fn build_session(model: &Path, coreml: bool) -> Result<Session> {
    let mut builder = Session::builder()?;
    if coreml {
        let units = match std::env::var("KOKORO_COREML_COMPUTE_UNITS")
            .unwrap_or_else(|_| "cpu_and_gpu".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "all" => ComputeUnits::All,
            "gpu" | "cpu_and_gpu" | "cpu-and-gpu" => ComputeUnits::CPUAndGPU,
            "ane"
            | "neural_engine"
            | "neural-engine"
            | "cpu_and_neural_engine"
            | "cpu-and-neural-engine" => ComputeUnits::CPUAndNeuralEngine,
            "cpu_only" | "cpu-only" | "cpuonly" => ComputeUnits::CPUOnly,
            other => bail!("unknown CoreML compute policy: {other}"),
        };
        let format = match std::env::var("KOKORO_COREML_MODEL_FORMAT")
            .unwrap_or_else(|_| "neuralnetwork".into())
            .to_ascii_lowercase()
            .as_str()
        {
            "mlprogram" | "ml_program" | "ml-program" => ModelFormat::MLProgram,
            "neuralnetwork" | "neural_network" | "neural-network" | "nn" => {
                ModelFormat::NeuralNetwork
            }
            other => bail!("unknown CoreML model format: {other}"),
        };
        let static_shapes = std::env::var("KOKORO_COREML_STATIC_INPUT_SHAPES")
            .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
            .unwrap_or(false);
        builder = builder
            .with_execution_providers([CoreML::default()
                .with_compute_units(units)
                .with_model_format(format)
                .with_static_input_shapes(static_shapes)
                .build()
                .error_on_failure()])
            .map_err(|e| anyhow::anyhow!(e.to_string()))?;
    }
    let mut session = builder.commit_from_file(model)?;
    // Preserve the original loader's compilation and dynamic-shape probes.
    // Murmur pins v1.0; an incompatible model returns a typed ORT input error.
    for len in [5, 12] {
        let mut tokens = vec![1; len];
        tokens[0] = 0;
        tokens[len - 1] = 0;
        run(&mut session, tokens, vec![0.0; 256])?;
    }
    Ok(session)
}
fn run(session: &mut Session, tokens: Vec<i64>, style: Vec<f32>) -> Result<Audio> {
    let inputs = ort::inputs![
        "input_ids" => Tensor::from_array(([1, tokens.len()], tokens))?,
        "style" => Tensor::from_array(([1, 256], style))?,
        "speed" => Tensor::from_array(([1], vec![1.0f32]))?,
    ];
    let start = Instant::now();
    let outputs = session.run(inputs)?;
    let duration = start.elapsed();
    let waveform = outputs
        .get("waveform")
        .context("Kokoro model has no waveform output")?;
    let (_, samples) = waveform.try_extract_tensor::<f32>()?;
    Ok((samples.to_vec(), duration))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    };

    #[test]
    fn rejects_corrupt_voice_packs_and_path_ids() {
        assert!(decode_voice(&[]).is_err());
        assert!(decode_voice(&[0; 1023]).is_err());
        let mut bytes = vec![0; 1024];
        bytes[..4].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(decode_voice(&bytes).is_err());
        bytes[..4].copy_from_slice(&0.25f32.to_le_bytes());
        assert_eq!(decode_voice(&bytes).unwrap()[0], 0.25);
        assert!(valid_voice_id("am_puck"));
        for id in ["", "../am_puck", "/af_heart", "a/b"] {
            assert!(!valid_voice_id(id));
        }
    }

    #[test]
    fn cancelled_queue_entries_are_skipped_and_errors_do_not_kill_worker() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (sender, receiver) = mpsc::channel(8);
            let epoch = Arc::new(AtomicU64::new(1));
            let (stale_tx, stale_rx) = oneshot::channel();
            sender
                .send(Request {
                    text: "stale".into(),
                    voice: "am_puck".into(),
                    response: stale_tx,
                    cancellation: Some(Cancellation::new(epoch.clone(), 1)),
                })
                .await
                .unwrap();
            epoch.fetch_add(1, Ordering::AcqRel);
            let (dropped_tx, dropped_rx) = oneshot::channel();
            sender
                .send(Request {
                    text: "dropped".into(),
                    voice: "am_puck".into(),
                    response: dropped_tx,
                    cancellation: None,
                })
                .await
                .unwrap();
            drop(dropped_rx);
            let (bad_tx, bad_rx) = oneshot::channel();
            sender
                .send(Request {
                    text: "bad".into(),
                    voice: "missing".into(),
                    response: bad_tx,
                    cancellation: None,
                })
                .await
                .unwrap();
            let (good_tx, good_rx) = oneshot::channel();
            sender
                .send(Request {
                    text: "good".into(),
                    voice: "af_heart".into(),
                    response: good_tx,
                    cancellation: None,
                })
                .await
                .unwrap();
            drop(sender);
            let thread = std::thread::spawn(move || {
                let mut seen = Vec::new();
                serve(receiver, |request| {
                    seen.push(request.text.clone());
                    if request.text == "bad" {
                        bail!("missing voice");
                    }
                    Ok((vec![0.25], Duration::ZERO))
                });
                seen
            });
            assert!(stale_rx.await.unwrap().is_err());
            assert!(bad_rx.await.unwrap().is_err());
            assert_eq!(good_rx.await.unwrap().unwrap().0, vec![0.25]);
            assert_eq!(thread.join().unwrap(), ["bad", "good"]);
        });
    }

    #[test]
    #[ignore = "requires installed Kokoro model and voices"]
    fn worker_matches_library_and_keeps_executor_responsive() {
        let root = PathBuf::from(std::env::var("HOME").unwrap())
            .join("Library/Application Support/murmur/models");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let model = root.join("kokoro-v1.0.onnx");
            let voices = root.join("kokoro-voices");
            let worker = Arc::new(KokoroTts::new(&model, &voices).await.unwrap());
            assert!(worker.synth("Hello", "../bad").await.is_err());
            assert!(worker.synth("Hello", "missing_voice").await.is_err());
            let baseline = kokoro_en::KokoroTts::new(&model, &voices).await.unwrap();
            for voice in ["am_puck", "af_heart"] {
                let text = "Local dictation should feel instant, and read aloud should start speaking right away.";
                let expected = baseline.synth(text, voice).await.unwrap().0;
                let actual = worker.synth(text, voice).await.unwrap().0;
                assert!(actual == expected, "waveform changed for {voice}");
            }
            let long = "Local dictation should feel instant, and reading should stay smooth. ".repeat(12);
            let phonemes = kokoro_en::g2p(&long, false).unwrap();
            assert!(kokoro_en::chunk_phonemes(&phonemes, kokoro_en::MAX_PHONEME_CHARS).len() > 1);
            let expected = baseline.synth(&long, "am_puck").await.unwrap().0;
            assert!(worker.synth(&long, "am_puck").await.unwrap().0 == expected, "multi-chunk waveform changed");
            // A timer must run while real inference is still in flight on this
            // single-thread executor. Blocking inference here would fail it.
            let handle = worker.clone();
            let synth = tokio::spawn(async move {
                handle.synth("A longer sentence exercises the inference worker while the asynchronous executor remains responsive to cancellation and interface events.", "am_puck").await
            });
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert!(!synth.is_finished(), "inference blocked the executor timer");
            assert!(!synth.await.unwrap().unwrap().0.is_empty());
            let handle = worker.clone();
            let a = tokio::spawn(async move { handle.synth("Hello.", "am_puck").await });
            let b = worker.synth("Hello.", "af_heart").await.unwrap();
            assert_ne!(a.await.unwrap().unwrap().0, b.0);
        });
    }
}
