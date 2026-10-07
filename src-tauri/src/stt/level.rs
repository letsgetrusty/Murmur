//! Conservative leveling for extreme volume changes across digital pauses.
//! Leave ordinary recordings and continuous/noisy audio untouched. This is not
//! VAD: a sustained exact-zero pause provides the only segmentation boundary.
const RATE: usize = 16_000;
const FRAME: usize = RATE / 50;
const PAUSE: usize = RATE * 2 / 5;

struct Region {
    start: usize,
    end: usize,
    level: f32,
    peak: f32,
}

/// Lift only speech-shaped regions at least 32x quieter than another region.
/// Bound gain to 64x and retain 8x headroom below the reference volume. Keep
/// sample count, digital silence, and all louder regions exactly unchanged.
pub(super) fn level_extreme_changes(samples: &mut [f32]) -> usize {
    let mut spans = Vec::new();
    let mut start = 0;
    let mut zero_start = None;
    for (i, sample) in samples.iter().enumerate() {
        if *sample == 0.0 {
            zero_start.get_or_insert(i);
        } else if let Some(zero) = zero_start.take() {
            if i - zero >= PAUSE {
                spans.push((start, zero));
                start = i;
            }
        }
    }
    spans.push((start, zero_start.unwrap_or(samples.len())));
    let regions: Vec<_> = spans
        .into_iter()
        .filter_map(|(start, end)| {
            let region = &samples[start..end];
            let peak = region.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
            let mut levels: Vec<_> = region
                .chunks(FRAME)
                .map(|frame| (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt())
                .filter(|rms| *rms > 1e-5)
                .collect();
            // Ignore short transients and steady noise/tone envelopes. This guard
            // cannot certify speech, but avoids a blanket quiet-audio gain boost.
            if levels.len() < 15 {
                return None;
            }
            levels.sort_by(f32::total_cmp);
            let low = levels[levels.len() / 10];
            let level = levels[(levels.len() * 9 / 10).min(levels.len() - 1)];
            if level < low * 2.5 {
                return None;
            }
            Some(Region {
                start,
                end,
                level,
                peak,
            })
        })
        .collect();
    let reference = regions.iter().map(|r| r.level).fold(0.0f32, f32::max);
    let mut changed = 0;
    for region in regions {
        if region.level * 32.0 >= reference {
            continue;
        }
        let gain = (reference / (8.0 * region.level))
            .min(64.0)
            .min(0.95 / region.peak);
        if gain <= 1.0 {
            continue;
        }
        let length = region.end - region.start;
        for (i, sample) in samples[region.start..region.end].iter_mut().enumerate() {
            // Ramp over 20ms at region edges rather than introducing a step.
            let ramp = (i.min(length - 1 - i) as f32 / FRAME as f32).min(1.0);
            *sample *= 1.0 + (gain - 1.0) * ramp;
        }
        changed += 1;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varying_audio(scale: f32) -> Vec<f32> {
        (0..RATE)
            .map(|i| {
                let envelope = if (i / FRAME) % 5 == 0 { 0.1 } else { 1.0 };
                scale * envelope * (i as f32 * 0.17).sin()
            })
            .collect()
    }

    #[test]
    fn lifts_quiet_region_without_changing_loud_audio_silence_or_length() {
        let loud = varying_audio(0.5);
        let quiet = varying_audio(0.001);
        let mut audio = loud.clone();
        audio.extend(vec![0.0; PAUSE]);
        let start = audio.len();
        audio.extend(&quiet);
        let length = audio.len();
        assert_eq!(level_extreme_changes(&mut audio), 1);
        assert_eq!(audio.len(), length);
        assert_eq!(&audio[..RATE], loud);
        assert!(audio[RATE..start].iter().all(|s| *s == 0.0));
        assert!(audio[start..]
            .iter()
            .all(|s| s.is_finite() && s.abs() <= 0.95));
        assert!(
            audio[start..].iter().map(|s| s.abs()).sum::<f32>()
                > quiet.iter().map(|s| s.abs()).sum::<f32>() * 10.0
        );
    }

    #[test]
    fn ordinary_corpus_audio_is_unchanged_and_only_mixed_volume_is_leveled() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        for path in [
            "benchmarks/fixtures/short.wav",
            "benchmarks/fixtures/long.wav",
            "benchmarks/fixtures/quiet-with-silence.wav",
            "src-tauri/tests/fixtures/mid-sentence-pause.wav",
        ] {
            let wav = std::fs::read(root.join(path)).unwrap();
            let mut samples = super::super::wav_to_mono_f32(&wav).unwrap();
            let original = samples.clone();
            assert_eq!(level_extreme_changes(&mut samples), 0, "{path}");
            assert_eq!(samples, original, "{path}");
        }
        let wav = std::fs::read(root.join("benchmarks/fixtures/multiple-pauses.wav")).unwrap();
        let mut samples = super::super::wav_to_mono_f32(&wav).unwrap();
        assert_eq!(level_extreme_changes(&mut samples), 1);
    }

    #[test]
    fn leaves_normal_volume_steady_noise_and_short_pauses_unchanged() {
        for tail in [varying_audio(0.1), vec![0.0001; RATE]] {
            let mut audio = varying_audio(0.5);
            audio.extend(vec![0.0; PAUSE]);
            audio.extend(tail);
            let original = audio.clone();
            assert_eq!(level_extreme_changes(&mut audio), 0);
            assert_eq!(audio, original);
        }
        let mut audio = varying_audio(0.5);
        audio.extend(vec![0.0; FRAME]);
        audio.extend(varying_audio(0.001));
        let original = audio.clone();
        assert_eq!(level_extreme_changes(&mut audio), 0);
        assert_eq!(audio, original);
        assert_eq!(level_extreme_changes(&mut []), 0);
        assert_eq!(level_extreme_changes(&mut vec![0.0; RATE]), 0);
    }
}
