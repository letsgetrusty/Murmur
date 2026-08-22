// Measure the two halves of the Fn-press -> mic-capturing critical path:
//   1. start-cue play()  — waking/acquiring the OUTPUT device (AVAudioPlayer)
//   2. Recorder::start()  — opening the INPUT device (cpal build+play)
// The dictation worker runs these SEQUENTIALLY (cue first, since 8ba5e32), so
// their sum is the delay before the mic actually captures. This tells us which
// half dominates and whether moving the cue off the critical path is worth it.
//
// Run cold (let the machine sit idle a few seconds first) to mimic a real
// first-of-session Fn press:  cargo run --example bench_start --release

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use objc2::runtime::AnyObject;
use objc2::{class, msg_send};
use objc2_foundation::NSString;

#[link(name = "AVFoundation", kind = "framework")]
extern "C" {}

const START_SOUND: &str = "/System/Library/Sounds/Pop.aiff";

fn make_player(path: &str) -> Option<NonNull<AnyObject>> {
    // SAFETY: mirrors sound.rs::make_player — fileURLWithPath: / alloc /
    // initWithContentsOfURL:error: are valid selectors with these arg types; the
    // result is null-checked before use.
    unsafe {
        let ns_path = NSString::from_str(path);
        let url: *mut AnyObject = msg_send![class!(NSURL), fileURLWithPath: &*ns_path];
        if url.is_null() {
            return None;
        }
        let alloc: *mut AnyObject = msg_send![class!(AVAudioPlayer), alloc];
        let err: *mut *mut AnyObject = std::ptr::null_mut();
        let player: *mut AnyObject = msg_send![alloc, initWithContentsOfURL: url, error: err];
        NonNull::new(player)
    }
}

fn time_cue_play(player: NonNull<AnyObject>, label: &str) {
    let t = Instant::now();
    // SAFETY: `player` is a live AVAudioPlayer from make_player; setCurrentTime:
    // and play are valid selectors with these arg/return types.
    unsafe {
        let p = player.as_ptr();
        let _: () = msg_send![p, setCurrentTime: 0.0f64];
        let _: bool = msg_send![p, play];
    }
    println!(
        "  cue play() [{label}]: {:.0}ms (blocking, on the mic-open critical path)",
        t.elapsed().as_secs_f32() * 1000.0
    );
}

fn time_mic_open(label: &str) {
    let host = cpal::default_host();
    let Some(device) = host.default_input_device() else {
        println!("  mic open [{label}]: no input device");
        return;
    };
    let Ok(supported) = device.default_input_config() else {
        println!("  mic open [{label}]: config query failed");
        return;
    };
    let config: cpal::StreamConfig = supported.clone().into();
    // Micros-since-play for: first callback, and first callback carrying real
    // (non-near-zero) audio. 0 = not seen yet. This is the "misses first words"
    // window — how long after play() the mic delivers usable samples.
    let play_at: Arc<std::sync::Mutex<Option<Instant>>> = Arc::new(std::sync::Mutex::new(None));
    let first_cb = Arc::new(AtomicU64::new(0));
    let first_audio = Arc::new(AtomicU64::new(0));
    let (pa, fc, fa) = (play_at.clone(), first_cb.clone(), first_audio.clone());
    let on_data = move |data: &[f32]| {
        let Some(t0) = *pa.lock().unwrap() else {
            return;
        };
        let us = t0.elapsed().as_micros() as u64;
        let _ = fc.compare_exchange(0, us, Ordering::Relaxed, Ordering::Relaxed);
        if data.iter().any(|s| s.abs() > 0.0005) {
            let _ = fa.compare_exchange(0, us, Ordering::Relaxed, Ordering::Relaxed);
        }
    };
    let t = Instant::now();
    let od = on_data.clone();
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &_| od(data),
            |e| eprintln!("stream error: {e}"),
            None,
        )
        .or_else(|_| {
            device.build_input_stream(
                &config,
                move |data: &[i16], _: &_| {
                    let f: Vec<f32> = data.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    on_data(&f)
                },
                |e| eprintln!("stream error: {e}"),
                None,
            )
        })
        .expect("build input stream");
    let build_ms = t.elapsed().as_secs_f32() * 1000.0;
    *play_at.lock().unwrap() = Some(Instant::now());
    stream.play().expect("play stream");
    // Let the mic run ~600ms so we see when real audio starts flowing.
    std::thread::sleep(std::time::Duration::from_millis(600));
    let fcb = first_cb.load(Ordering::Relaxed) as f32 / 1000.0;
    let fau = first_audio.load(Ordering::Relaxed) as f32 / 1000.0;
    println!(
        "  mic open [{label}]: build {:.0}ms | after play(): first callback {:.0}ms, first real audio {:.0}ms (device={:?})",
        build_ms,
        fcb,
        if fau > 0.0 { fau } else { -1.0 },
        device.name().ok()
    );
    drop(stream);
}

fn main() {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!("\n== bench_start [{profile}] ==");
    let player = make_player(START_SOUND).expect("load Pop.aiff");

    // COLD: first play/open of the run (closest to a first-of-session Fn press).
    println!("-- cold (first press after idle) --");
    time_cue_play(player, "cold");
    std::thread::sleep(std::time::Duration::from_millis(50));
    time_mic_open("cold");

    // WARM: repeat presses while hardware is awake.
    println!("-- warm (repeat presses) --");
    for i in 0..3 {
        time_cue_play(player, &format!("warm{i}"));
        time_mic_open(&format!("warm{i}"));
    }
}
