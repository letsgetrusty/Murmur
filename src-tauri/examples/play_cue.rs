// Smoke-test the real dictation cue playback path (src/sound.rs) end to end.
//
// Exercises the exact production code: `Cues::load()` (which now `prepareToPlay`s
// each player) and `play_start` / `play_stop`. Plays the start cue cold, waits so
// the output device idles back to sleep, then plays it again — the scenario where
// the cue used to be dropped. Listen: you should hear the "Pop" start blip on
// EVERY iteration, then the "Bottle" stop blip.
//
//   cargo run --example play_cue --release

use std::time::Duration;

use murmur_lib::sound::Cues;

fn main() {
    let cues = Cues::load();
    for i in 1..=3 {
        println!("[{i}] start cue…");
        cues.play_start();
        std::thread::sleep(Duration::from_millis(1800)); // let the blip finish
        println!("[{i}] stop cue…");
        cues.play_stop();
        // Long enough for the output device to idle to sleep, so the next
        // start cue hits a cold device — the case that used to go silent.
        std::thread::sleep(Duration::from_secs(4));
    }
    println!("done");
}
