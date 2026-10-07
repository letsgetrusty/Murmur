//! Speculative whole-recording dictation. Pauses trigger an early decode, never
//! a sentence boundary. If speech continues after that snapshot, release decodes
//! the complete recording so Whisper can revise punctuation and word choices.

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
const KEEP_QUIET: usize = RATE / 5; // locate a point safely inside the detected pause

#[derive(Default)]
struct Prefix {
    consumed: usize,
    text: String,
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
                if pause_boundary(&pending).is_none() {
                    continue;
                }
                // Include all earlier speech and the full available pause. A
                // later snapshot replaces this text; independent chunk results
                // must never be concatenated into the final dictation.
                let snapshot = match feed.since(0) {
                    Ok(samples) => samples,
                    Err(_) => {
                        prefix.failed = true;
                        break;
                    }
                };
                let wav = match encode_wav_i16(&snapshot, RATE as u32) {
                    Ok(wav) => wav,
                    Err(_) => {
                        prefix.failed = true;
                        break;
                    }
                };
                if stopping.load(Ordering::Acquire) {
                    break;
                }
                match engine.transcribe(&wav).await {
                    Ok(text) if !text.trim().is_empty() => {
                        prefix.text = text;
                        prefix.consumed = snapshot.len();
                        pending.clear();
                        log::info!(
                            "stt: speculative whole-recording snapshot {:.1}s",
                            prefix.consumed as f32 / RATE as f32
                        );
                    }
                    result => {
                        log::warn!("stt: speculative snapshot failed or empty ({:?}); retrying whole recording on release", result.err());
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
                log::warn!("stt: speculative worker failed: {e}; retrying whole recording");
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

async fn finish_prefix(engine: &dyn Transcriber, prefix: Prefix, wav: &[u8]) -> Result<String> {
    if prefix.failed || prefix.text.is_empty() {
        return engine.transcribe(wav).await;
    }
    let samples = wav_to_mono_f32(wav)?;
    let Some(tail) = samples.get(prefix.consumed..) else {
        return engine.transcribe(wav).await;
    };
    let threshold = (frame_peak(&samples) * 0.025).clamp(1e-5, 0.001);
    if frame_peak(tail) >= threshold {
        // A pause inside a sentence isn't its end. Re-decode with both sides
        // of the pause instead of pasting "I would like. To change this.".
        log::info!("stt: speech continued after snapshot; decoding complete recording");
        return engine.transcribe(wav).await;
    }
    Ok(prefix.text)
}

fn frame_peak(samples: &[f32]) -> f32 {
    samples
        .chunks(FRAME)
        .map(|frame| frame.iter().map(|v| v.abs()).sum::<f32>() / frame.len() as f32)
        .fold(0.0, f32::max)
}

/// Find a sustained quiet interval after at least two seconds. This is only
/// a trigger for speculative whole-recording inference, never a cut in speech.
/// The returned position is inside the pause (also useful for regression tests).
/// A noisy room simply falls back to whole-recording STT on release.
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
    fn continued_speech_redecodes_whole_recording_instead_of_joining_chunks() {
        let engine = Fake(Mutex::new(Vec::new()));
        let wav = encode_wav_i16(&vec![0.1; RATE * 5], RATE as u32).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let prefix = Prefix {
            consumed: RATE * 3,
            text: "prefix".into(),
            failed: false,
        };
        assert_eq!(
            rt.block_on(finish_prefix(&engine, prefix, &wav)).unwrap(),
            "tail"
        );
        assert_eq!(*engine.0.lock().unwrap(), vec![RATE * 5]);
        let prefix = Prefix {
            consumed: RATE * 3,
            text: "prefix".into(),
            failed: true,
        };
        assert_eq!(
            rt.block_on(finish_prefix(&engine, prefix, &wav)).unwrap(),
            "tail"
        );
        assert_eq!(*engine.0.lock().unwrap(), vec![RATE * 5, RATE * 5]);
    }
    #[test]
    fn revises_speculative_periods_and_ellipses_without_stripping_final_punctuation() {
        struct CompleteSentence;
        impl Transcriber for CompleteSentence {
            fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
                Box::pin(async move {
                    assert_eq!(wav_to_mono_f32(wav)?.len(), RATE * 5);
                    Ok("Please keep this sentence together.".into())
                })
            }
        }
        let wav = encode_wav_i16(&vec![0.1; RATE * 5], RATE as u32).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for text in ["Please keep.", "Please keep...", "Please keep…"] {
            let prefix = Prefix {
                consumed: RATE * 3,
                text: text.into(),
                failed: false,
            };
            assert_eq!(
                rt.block_on(finish_prefix(&CompleteSentence, prefix, &wav))
                    .unwrap(),
                "Please keep this sentence together."
            );
        }
    }

    #[test]
    fn release_skips_retained_room_noise_but_keeps_quiet_speech() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        for (tail_amplitude, expected) in [(0.0003, "prefix"), (0.01, "tail")] {
            let engine = Fake(Mutex::new(Vec::new()));
            let mut samples = vec![0.1; RATE * 3];
            samples.extend(vec![tail_amplitude; RATE / 2]);
            let wav = encode_wav_i16(&samples, RATE as u32).unwrap();
            let prefix = Prefix {
                consumed: RATE * 3,
                text: "prefix".into(),
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
        samples: std::sync::atomic::AtomicUsize,
    }
    impl Transcriber for Gated {
        fn transcribe<'a>(&'a self, wav: &'a [u8]) -> TranscribeFuture<'a> {
            Box::pin(async move {
                self.samples
                    .store(wav_to_mono_f32(wav)?.len(), Ordering::SeqCst);
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
                    samples: std::sync::atomic::AtomicUsize::new(0),
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
                assert_eq!(engine.samples.load(Ordering::SeqCst), RATE * 4, "snapshot must include the entire recording available, not just the first pause-delimited fragment");
            }
        });
    }

    #[test]
    #[ignore = "needs the local Whisper model"]
    fn continued_speech_matches_whole_recording_fixture() {
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
        let chunk = rt.block_on(engine.transcribe(&first)).unwrap();
        let released = std::time::Instant::now();
        let result = rt
            .block_on(finish_prefix(
                &engine,
                Prefix {
                    consumed: cut,
                    text: chunk,
                    failed: false,
                },
                &all,
            ))
            .unwrap();
        let release_time = released.elapsed();
        let batch_start = std::time::Instant::now();
        let batch = rt.block_on(engine.transcribe(&all)).unwrap();
        eprintln!(
            "STT RELEASE: revised snapshot {:.0}ms; full recording {:.0}ms",
            release_time.as_secs_f32() * 1000.0,
            batch_start.elapsed().as_secs_f32() * 1000.0
        );
        assert_eq!(batch.to_lowercase().matches("dictation").count(), 2);
        assert_eq!(
            result, batch,
            "snapshot must not change the final whole-recording transcript"
        );
        eprintln!("WHOLE RECORDING: {result}");
        assert_eq!(
            result.to_lowercase().matches("dictation").count(),
            2,
            "lost or repeated phrase: {result}"
        );
    }
    #[test]
    #[ignore = "needs the local small.en model"]
    fn pause_fixture_preserves_sentence_context() {
        let wav = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/mid-sentence-pause.wav"
        ))
        .unwrap();
        let samples = wav_to_mono_f32(&wav).unwrap();
        let cut = pause_boundary(&samples).expect("recording needs a pause");
        let engine = super::super::WhisperStt::new("small.en");
        engine.set_vocabulary(crate::config::DEFAULT_DICTATION_VOCABULARY);
        engine.set_corrections(crate::config::DEFAULT_DICTATION_CORRECTIONS);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let first = encode_wav_i16(&samples[..cut], RATE as u32).unwrap();
            let tail = encode_wav_i16(&samples[cut..], RATE as u32).unwrap();
            let first_text = engine.transcribe_chunk(&first).await.unwrap();
            let tail_text = engine.transcribe_chunk(&tail).await.unwrap();
            eprintln!(
                "OLD FRAGMENTS: {}",
                engine.finish_chunks(&[first_text.clone(), tail_text])
            );
            let started = std::time::Instant::now();
            let text = finish_prefix(
                &engine,
                Prefix {
                    consumed: cut,
                    text: first_text,
                    failed: false,
                },
                &wav,
            )
            .await
            .unwrap();
            eprintln!(
                "WHOLE CONTEXT ({:.0}ms): {text}",
                started.elapsed().as_secs_f64() * 1000.0
            );
            assert_eq!(text, "The first important change that I would like us to make is to keep the whole sentence together while I am thinking about what to say next.");
        });
    }
}
