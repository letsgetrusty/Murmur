//! Isolate ONNX dispatch from voice/text preparation without modifying kokoro-en.
use anyhow::{ensure, Result};
use ort::{
    ep::{coreml::ComputeUnits, CoreML},
    session::{RunOptions, Session},
    value::Tensor,
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub struct DirectSession {
    session: Session,
    voice: Vec<f32>,
    asynchronous: bool,
}

impl DirectSession {
    pub fn new(
        model: &Path,
        voices: &Path,
        voice: &str,
        compute: &str,
        asynchronous: bool,
    ) -> Result<Self> {
        let mut builder = Session::builder()?;
        if compute != "cpu" {
            let units = match compute {
                "cpu_and_gpu" => ComputeUnits::CPUAndGPU,
                "cpu_and_neural_engine" => ComputeUnits::CPUAndNeuralEngine,
                "cpu_only" => ComputeUnits::CPUOnly,
                "all" => ComputeUnits::All,
                _ => anyhow::bail!("unknown compute units"),
            };
            builder = builder
                .with_execution_providers([CoreML::default()
                    .with_compute_units(units)
                    .build()
                    .error_on_failure()])
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        }
        let session = builder.commit_from_file(model)?;
        let bytes = std::fs::read(voices.join(format!("{voice}.bin")))?;
        ensure!(bytes.len() % (256 * 4) == 0, "expected raw f32 voice pack");
        let voice = bytes
            .chunks_exact(4)
            .map(|v| f32::from_le_bytes(v.try_into().unwrap()))
            .collect();
        Ok(Self {
            session,
            voice,
            asynchronous,
        })
    }

    pub async fn synth(&mut self, text: &str) -> Result<(Vec<f32>, Duration)> {
        let phonemes = kokoro_en::g2p(text, false)?;
        let mut audio = Vec::new();
        let mut elapsed = Duration::ZERO;
        for chunk in kokoro_en::chunk_phonemes(&phonemes, kokoro_en::MAX_PHONEME_CHARS) {
            let tokens = kokoro_en::get_token_ids(&chunk, false);
            let start = (tokens.len() - 1) * 256;
            let style = self
                .voice
                .get(start..start + 256)
                .ok_or_else(|| anyhow::anyhow!("voice style index out of range"))?
                .to_vec();
            let inputs = ort::inputs![
                "input_ids" => Tensor::from_array(([1, tokens.len()], tokens))?,
                "style" => Tensor::from_array(([1, 256], style))?,
                "speed" => Tensor::from_array(([1], vec![1.0f32]))?,
            ];
            let options = RunOptions::new()?;
            let start = Instant::now();
            let outputs = if self.asynchronous {
                self.session.run_async(inputs, &options)?.await?
            } else {
                self.session.run(inputs)?
            };
            elapsed += start.elapsed();
            let (_, samples) = outputs["waveform"].try_extract_tensor::<f32>()?;
            audio.extend_from_slice(samples);
        }
        Ok((audio, elapsed))
    }
}

/// Benchmark the proposed production boundary: one persistent blocking worker,
/// an asynchronous caller, and synchronous ONNX execution on the worker.
pub struct Worker {
    requests: std::sync::mpsc::Sender<Request>,
}
struct Request {
    text: String,
    response: tokio::sync::oneshot::Sender<Result<(Vec<f32>, Duration)>>,
}
impl Worker {
    pub async fn new(model: &Path, voices: &Path, voice: &str, compute: &str) -> Result<Self> {
        let (requests, receiver) = std::sync::mpsc::channel::<Request>();
        let (ready, initialized) = tokio::sync::oneshot::channel();
        let (model, voices, voice, compute) = (
            model.to_owned(),
            voices.to_owned(),
            voice.to_owned(),
            compute.to_owned(),
        );
        std::thread::Builder::new()
            .name("kokoro-bench-worker".into())
            .spawn(move || {
                let session = DirectSession::new(&model, &voices, &voice, &compute, false);
                let mut session = match session {
                    Ok(session) => {
                        let _ = ready.send(Ok(()));
                        session
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("worker runtime");
                while let Ok(request) = receiver.recv() {
                    // The sync branch never awaits native inference; block_on merely
                    // shares preparation code with the direct async comparator.
                    let result = runtime.block_on(session.synth(&request.text));
                    let _ = request.response.send(result);
                }
            })?;
        initialized.await??;
        Ok(Self { requests })
    }
    pub async fn synth(&self, text: &str) -> Result<(Vec<f32>, Duration)> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.requests
            .send(Request {
                text: text.to_owned(),
                response,
            })
            .map_err(|_| anyhow::anyhow!("inference worker stopped"))?;
        result.await?
    }
}
