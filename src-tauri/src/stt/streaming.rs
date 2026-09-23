//! Pause-delimited incremental dictation. Keep the recording intact for a full
//! retry if a chunk fails. No forced time cuts: uninterrupted speech stays in
//! the final tail rather than risking dropped words at an artificial boundary.

use super::{wav_to_mono_f32, Transcriber};
use crate::audio::{encode_wav_i16, AudioFeed};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

const RATE: usize = 16_000;
const FRAME: usize = RATE / 50; // 20 ms
const MIN_CHUNK: usize = 2 * RATE;
const PAUSE_FRAMES: usize = 20; // 400 ms; don't cut between syllables
const KEEP_QUIET: usize = RATE / 5; // 200 ms of leading silence for the next decode

#[derive(Default)]
struct Prefix {
    consumed: usize,
    chunks: Vec<String>,
    failed: bool,
}

pub struct LiveTranscription {
    stop: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
    task: Option<tauri::async_runtime::JoinHandle<Prefix>>,
    transcriber: Arc<dyn Transcriber>,
}

impl LiveTranscription {
    pub fn start(feed: AudioFeed, transcriber: Arc<dyn Transcriber>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let wake = Arc::new(tokio::sync::Notify::new());
        let wake_worker = wake.clone();
        let engine = transcriber.clone();
        let task = tauri::async_runtime::spawn(async move {
            let mut prefix = Prefix::default();
            let mut pending = Vec::new();
            while !stopping.load(Ordering::Acquire) {
                let _ =
                    tokio::time::timeout(Duration::from_millis(250), wake_worker.notified()).await;
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                let samples = match feed.since(prefix.consumed + pending.len()) {
                    Ok(samples) => samples,
                    Err(_) => {
                        prefix.failed = true;
                        break;
                    }
                };
                pending.extend(samples);
                let Some(end) = pause_boundary(&pending) else {
                    continue;
                };
                let wav = match encode_wav_i16(&pending[..end], RATE as u32) {
                    Ok(wav) => wav,
                    Err(_) => {
                        prefix.failed = true;
                        break;
                    }
                };
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                match engine.transcribe_chunk(&wav).await {
                    Ok(text) if !engine.finish_chunks(std::slice::from_ref(&text)).is_empty() => {
                        prefix.chunks.push(text);
                        prefix.consumed += end;
                        pending.drain(..end);
                        log::info!(
                            "stt: incremental prefix {:.1}s in {} chunk(s)",
                            prefix.consumed as f32 / RATE as f32,
                            prefix.chunks.len()
                        );
                    }
                    result => {
                        log::warn!("stt: incremental chunk failed or empty ({:?}); retrying whole recording on release", result.err());
                        prefix.failed = true;
                        break;
                    }
                }
            }
            prefix
        });
        Self {
            stop,
            wake,
            task: Some(task),
            transcriber,
        }
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake.notify_one();
    }

    pub async fn finish(mut self, wav: &[u8]) -> Result<String> {
        self.stop();
        let prefix = match self.task.take().expect("live task").await {
            Ok(prefix) => prefix,
            Err(e) => {
                log::warn!("stt: incremental worker failed: {e}; retrying whole recording");
                return self.transcriber.transcribe(wav).await;
            }
        };
        finish_prefix(self.transcriber.as_ref(), prefix, wav).await
    }
}

impl Drop for LiveTranscription {
    fn drop(&mut self) {
        // Cancel, mic error, and short/silent clips all drop this guard. A native
        // inference already underway may finish, but cannot paste or start more.
        self.stop();
    }
}

async fn finish_prefix(engine: &dyn Transcriber, mut prefix: Prefix, wav: &[u8]) -> Result<String> {
    if prefix.failed || prefix.chunks.is_empty() {
        return engine.transcribe(wav).await;
    }
    let samples = wav_to_mono_f32(wav)?;
    let Some(tail) = samples.get(prefix.consumed..) else {
        return engine.transcribe(wav).await;
    };
    // Retained pause audio includes microphone noise, not digital silence.
    // Use the same relative gate as the boundary detector so an empty decode
    // of that noise doesn't force an unnecessary full-recording retry.
    let threshold = (frame_peak(&samples) * 0.025).clamp(1e-5, 0.001);
    if frame_peak(tail) >= threshold {
        let tail_wav = encode_wav_i16(tail, RATE as u32)?;
        match engine.transcribe_chunk(&tail_wav).await {
            Ok(text) if !engine.finish_chunks(std::slice::from_ref(&text)).is_empty() => {
                prefix.chunks.push(text)
            }
            _ => return engine.transcribe(wav).await,
        }
    }
    Ok(engine.finish_chunks(&prefix.chunks))
}

fn frame_peak(samples: &[f32]) -> f32 {
    samples
        .chunks(FRAME)
        .map(|frame| frame.iter().map(|v| v.abs()).sum::<f32>() / frame.len() as f32)
        .fold(0.0, f32::max)
}

/// Find a sustained quiet interval after at least two seconds. Cut inside the
/// silence and retain its last 200 ms in the next chunk. The threshold scales
/// down with quiet microphones; a noisy room simply falls back to batch STT.
fn pause_boundary(samples: &[f32]) -> Option<usize> {
    if samples.len() < MIN_CHUNK + PAUSE_FRAMES * FRAME {
        return None;
    }
    let levels: Vec<f32> = samples
        .chunks_exact(FRAME)
        .map(|frame| frame.iter().map(|v| v.abs()).sum::<f32>() / FRAME as f32)
        .collect();
    let peak = levels.iter().copied().fold(0.0f32, f32::max);
    if peak < 1e-4 {
        return None;
    }
    let threshold = (peak * 0.025).clamp(1e-5, 0.001);
    let mut quiet = 0;
    let mut heard_speech = false;
    for (i, level) in levels.into_iter().enumerate() {
        quiet = if level < threshold {
            quiet + 1
        } else {
            heard_speech = true;
            0
        };
        let end = (i + 1) * FRAME;
        if heard_speech && quiet >= PAUSE_FRAMES && end >= MIN_CHUNK + PAUSE_FRAMES * FRAME {
            return Some(end - KEEP_QUIET);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stt::TranscribeFuture;
    use std::sync::Mutex;

    #[test]
    fn cuts_inside_pause_without_losing_following_speech() {
        let mut audio = vec![0.1; RATE * 3];
        audio.extend(vec![0.0; RATE / 2]);
        audio.extend(vec![0.1; RATE]);
        let end = pause_boundary(&audio).unwrap();
        assert!(end > RATE * 3 && end < RATE * 3 + RATE / 2);
        assert!(audio[end..].contains(&0.1));
    }

    #[test]
    fn continuous_speech_silence_and_short_pauses_are_not_cut() {
        assert_eq!(pause_boundary(&vec![0.1; RATE * 20]), None);
        assert_eq!(pause_boundary(&vec![0.0; RATE * 20]), None);
        let mut audio = vec![0.01; RATE * 3];
        audio.extend(vec![0.0; RATE / 5]);
        audio.extend(vec![0.01; RATE * 3]);
        assert_eq!(pause_boundary(&audio), None);
    }

    #[test]
    fn leading_silence_is_not_mistaken_for_a_completed_phrase() {
        let mut audio = vec![0.0; RATE * 3];
        audio.extend(vec![0.1; RATE]);
        assert_eq!(pause_boundary(&audio), None);
        audio.extend(vec![0.0; RATE / 2]);
        assert!(pause_boundary(&audio).unwrap() > RATE * 4);
    }

    struct Fake(Mutex<Vec<usize>>);
    impl Transcriber for Fake {
        fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
            Box::pin(async move {
                self.0.lock().unwrap().push(wav_to_mono_f32(wav)?.len());
                Ok("tail".into())
            })
        }
    }

    #[test]
    fn release_decodes_only_tail_and_failure_retries_whole_recording() {
        let engine = Fake(Mutex::new(Vec::new()));
        let wav = encode_wav_i16(&vec![0.1; RATE * 5], RATE as u32).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let prefix = Prefix {
            consumed: RATE * 3,
            chunks: vec!["prefix".into()],
            failed: false,
        };
        assert_eq!(
            rt.block_on(finish_prefix(&engine, prefix, &wav)).unwrap(),
            "prefix tail"
        );
        assert_eq!(*engine.0.lock().unwrap(), vec![RATE * 2]);
        let prefix = Prefix {
            consumed: RATE * 3,
            chunks: vec!["prefix".into()],
            failed: true,
        };
        assert_eq!(
            rt.block_on(finish_prefix(&engine, prefix, &wav)).unwrap(),
            "tail"
        );
        assert_eq!(*engine.0.lock().unwrap(), vec![RATE * 2, RATE * 5]);
    }
    #[test]
    fn release_skips_retained_room_noise_but_keeps_quiet_speech() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for (tail_amplitude, expected) in [(0.0003, "prefix"), (0.01, "prefix tail")] {
            let engine = Fake(Mutex::new(Vec::new()));
            let mut samples = vec![0.1; RATE * 3];
            samples.extend(vec![tail_amplitude; RATE / 2]);
            let wav = encode_wav_i16(&samples, RATE as u32).unwrap();
            let prefix = Prefix {
                consumed: RATE * 3,
                chunks: vec!["prefix".into()],
                failed: false,
            };
            assert_eq!(
                rt.block_on(finish_prefix(&engine, prefix, &wav)).unwrap(),
                expected
            );
            assert_eq!(
                engine.0.lock().unwrap().len(),
                usize::from(tail_amplitude > 0.001)
            );
        }
    }

    struct Gated {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl Transcriber for Gated {
        fn transcribe<'a>(&'a self, _wav: &'a [u8]) -> TranscribeFuture<'a> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.entered.notify_one();
                self.release.notified().await;
                Ok("completed phrase".into())
            })
        }
    }

    #[test]
    fn release_keeps_inflight_prefix_and_cancel_does_not_decode_more() {
        tauri::async_runtime::block_on(async {
            for cancel in [false, true] {
                let mut audio = vec![0.1; RATE * 3];
                audio.extend(vec![0.0; RATE]);
                let wav = encode_wav_i16(&audio, RATE as u32).unwrap();
                let engine = Arc::new(Gated {
                    entered: tokio::sync::Notify::new(),
                    release: tokio::sync::Notify::new(),
                    calls: std::sync::atomic::AtomicUsize::new(0),
                });
                let mut live =
                    LiveTranscription::start(AudioFeed::test_samples(audio), engine.clone());
                tokio::time::timeout(Duration::from_secs(5), engine.entered.notified())
                    .await
                    .unwrap();
                if cancel {
                    let task = live.task.take().unwrap();
                    drop(live);
                    engine.release.notify_one();
                    tokio::time::timeout(Duration::from_secs(5), task)
                        .await
                        .unwrap()
                        .unwrap();
                } else {
                    live.stop();
                    engine.release.notify_one();
                    let text = tokio::time::timeout(Duration::from_secs(5), live.finish(&wav))
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(text, "completed phrase");
                }
                assert_eq!(engine.calls.load(Ordering::SeqCst), 1);
            }
        });
    }

    #[test]
    #[ignore = "needs the local Whisper model"]
    fn incremental_fixture_keeps_both_utterances() {
        let engine = super::super::WhisperStt::new("small.en");
        engine.set_vocabulary(crate::config::DEFAULT_DICTATION_VOCABULARY);
        engine.set_corrections(crate::config::DEFAULT_DICTATION_CORRECTIONS);
        let speech = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/speech.wav"
        ))
        .unwrap();
        let samples = wav_to_mono_f32(&speech).unwrap();
        let mut audio = samples.clone();
        audio.extend(vec![0.0; RATE / 2]);
        audio.extend(samples);
        let cut = pause_boundary(&audio).expect("fixture should have a pause");
        let first = encode_wav_i16(&audio[..cut], RATE as u32).unwrap();
        let all = encode_wav_i16(&audio, RATE as u32).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let chunk = rt.block_on(engine.transcribe_chunk(&first)).unwrap();
        let released = std::time::Instant::now();
        let result = rt
            .block_on(finish_prefix(
                &engine,
                Prefix {
                    consumed: cut,
                    chunks: vec![chunk],
                    failed: false,
                },
                &all,
            ))
            .unwrap();
        let release_time = released.elapsed();
        let batch_start = std::time::Instant::now();
        let batch = rt.block_on(engine.transcribe(&all)).unwrap();
        eprintln!(
            "STT RELEASE: incremental {:.0}ms; full recording {:.0}ms",
            release_time.as_secs_f32() * 1000.0,
            batch_start.elapsed().as_secs_f32() * 1000.0
        );
        assert_eq!(batch.to_lowercase().matches("dictation").count(), 2);
        eprintln!("INCREMENTAL: {result}");
        assert_eq!(
            result.to_lowercase().matches("dictation").count(),
            2,
            "lost or repeated phrase: {result}"
        );
    }
}
