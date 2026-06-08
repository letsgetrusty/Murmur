// Phase 1: dictation capture.
//
// cpal hands us whatever sample format and rate the OS prefers (usually 44.1 or
// 48 kHz f32). We mix to mono on the capture thread (cheap, locks held briefly),
// then resample to 16 kHz mono i16 on stop and emit a WAV blob ready to POST.
//
// Capture runs on cpal's audio thread, not tokio. Stop is synchronous and
// fast — the heavy work (transcribe) happens after we hand back the bytes.

use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream, StreamConfig};

const TARGET_SAMPLE_RATE: u32 = 16_000;

pub struct Recording {
    pub wav: Vec<u8>,
    pub duration_ms: u32,
    /// Mean absolute amplitude of the resampled signal in [0.0, 1.0].
    /// Near-zero strongly suggests a missing Microphone permission.
    pub mean_abs: f32,
}

pub struct Recorder {
    inner: Arc<Mutex<Inner>>,
    _stream: Stream,
    source_sample_rate: u32,
}

struct Inner {
    /// Captured mono f32 samples at the source device rate.
    samples: Vec<f32>,
}

/// List input devices reported by cpal. Empty if cpal can't enumerate (rare).
pub fn list_input_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(e) => {
            log::warn!("audio: enumerate input devices failed: {e}");
            Vec::new()
        }
    }
}

impl Recorder {
    /// Start capture on `device_name`, or the host's default input if `None`
    /// or if the named device can't be found (logged + falls back).
    pub fn start(device_name: Option<&str>) -> Result<Self> {
        let host = cpal::default_host();
        let device = match device_name {
            Some(name) => host
                .input_devices()
                .ok()
                .and_then(|mut iter| iter.find(|d| d.name().ok().as_deref() == Some(name)))
                .or_else(|| {
                    log::warn!(
                        "audio: input device {name:?} not found, falling back to default"
                    );
                    host.default_input_device()
                })
                .ok_or_else(|| anyhow!("no input device"))?,
            None => host
                .default_input_device()
                .ok_or_else(|| anyhow!("no default input device"))?,
        };

        let supported = device
            .default_input_config()
            .context("query default input config")?;

        let source_sample_rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();

        log::info!(
            "audio: device={:?} rate={}Hz channels={} format={:?}",
            device.name().ok(),
            source_sample_rate,
            channels,
            sample_format,
        );

        let inner = Arc::new(Mutex::new(Inner {
            samples: Vec::with_capacity(source_sample_rate as usize * 8), // ~8s
        }));

        let err_fn = |e| log::warn!("audio stream error: {e}");

        let stream = match sample_format {
            SampleFormat::F32 => {
                let inner_cl = inner.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| push_f32(&inner_cl, data, channels),
                    err_fn,
                    None,
                )?
            }
            SampleFormat::I16 => {
                let inner_cl = inner.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[i16], _| {
                        let f: Vec<f32> = data
                            .iter()
                            .map(|s| *s as f32 / i16::MAX as f32)
                            .collect();
                        push_f32(&inner_cl, &f, channels);
                    },
                    err_fn,
                    None,
                )?
            }
            SampleFormat::U16 => {
                let inner_cl = inner.clone();
                device.build_input_stream(
                    &config,
                    move |data: &[u16], _| {
                        let f: Vec<f32> = data
                            .iter()
                            .map(|s| (*s as f32 - 32768.0) / 32768.0)
                            .collect();
                        push_f32(&inner_cl, &f, channels);
                    },
                    err_fn,
                    None,
                )?
            }
            other => return Err(anyhow!("unsupported sample format: {other:?}")),
        };

        stream.play().context("start audio stream")?;

        Ok(Self {
            inner,
            _stream: stream,
            source_sample_rate,
        })
    }

    pub fn stop(self) -> Result<Recording> {
        // Dropping the stream stops it; the mutex is no longer contended after.
        let samples = std::mem::take(
            &mut self
                .inner
                .lock()
                .map_err(|_| anyhow!("audio buffer mutex poisoned"))?
                .samples,
        );
        drop(self._stream);

        let resampled = resample_linear(&samples, self.source_sample_rate, TARGET_SAMPLE_RATE);

        let mean_abs = if resampled.is_empty() {
            0.0
        } else {
            resampled.iter().map(|s| s.abs()).sum::<f32>() / resampled.len() as f32
        };
        let duration_ms = (resampled.len() as u32 * 1000) / TARGET_SAMPLE_RATE.max(1);
        let wav = encode_wav_i16(&resampled, TARGET_SAMPLE_RATE)?;

        log::info!(
            "audio: captured {}ms, mean|amp|={:.4}, wav={} bytes",
            duration_ms,
            mean_abs,
            wav.len()
        );

        Ok(Recording {
            wav,
            duration_ms,
            mean_abs,
        })
    }
}

fn push_f32(inner: &Arc<Mutex<Inner>>, data: &[f32], channels: usize) {
    let Ok(mut g) = inner.lock() else { return };
    if channels <= 1 {
        g.samples.extend_from_slice(data);
        return;
    }
    g.samples.reserve(data.len() / channels);
    for frame in data.chunks_exact(channels) {
        let sum: f32 = frame.iter().sum();
        g.samples.push(sum / channels as f32);
    }
}

/// Linear interpolation resampler. Good enough for speech at 16 kHz target.
fn resample_linear(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let out_len = ((samples.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let src = i as f64 * ratio;
        let idx = src.floor() as usize;
        let frac = (src - idx as f64) as f32;
        let a = samples[idx];
        let b = samples.get(idx + 1).copied().unwrap_or(a);
        out.push(a + (b - a) * frac);
    }
    out
}

/// Minimal RIFF/WAV writer — 16-bit PCM, mono. Avoids pulling in `hound`.
fn encode_wav_i16(samples: &[f32], sample_rate: u32) -> Result<Vec<u8>> {
    let bytes_per_sample = 2u16;
    let channels = 1u16;
    let byte_rate = sample_rate * channels as u32 * bytes_per_sample as u32;
    let block_align = channels * bytes_per_sample;
    let data_size = (samples.len() as u32) * bytes_per_sample as u32;
    let riff_size = 36 + data_size;

    let mut buf = Cursor::new(Vec::with_capacity(44 + data_size as usize));
    buf.write_all(b"RIFF")?;
    buf.write_all(&riff_size.to_le_bytes())?;
    buf.write_all(b"WAVE")?;
    buf.write_all(b"fmt ")?;
    buf.write_all(&16u32.to_le_bytes())?; // fmt chunk size
    buf.write_all(&1u16.to_le_bytes())?; // PCM
    buf.write_all(&channels.to_le_bytes())?;
    buf.write_all(&sample_rate.to_le_bytes())?;
    buf.write_all(&byte_rate.to_le_bytes())?;
    buf.write_all(&block_align.to_le_bytes())?;
    buf.write_all(&(bytes_per_sample as u16 * 8).to_le_bytes())?;
    buf.write_all(b"data")?;
    buf.write_all(&data_size.to_le_bytes())?;
    for s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let v = (clamped * i16::MAX as f32) as i16;
        buf.write_all(&v.to_le_bytes())?;
    }
    Ok(buf.into_inner())
}
