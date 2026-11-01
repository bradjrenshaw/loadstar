//! Listen for clicks: menu-style lines through the root auto voice, each interrupting the last
//! every 350 ms the way arrowing through a menu does, first spoken by the OS and then with
//! PLAY_SAMPLES (rendered and played through Loadstar's own output, where a cut fades). Then
//! one line after two seconds of quiet, for the start-up pop.
//!
//!     cargo run -p loadstar-speech --example play_samples
//!     cargo run -p loadstar-speech --example play_samples -- samples   (only the PLAY_SAMPLES pass)

use std::time::{Duration, Instant};

use loadstar_speech::context::{profile_flags, speak_flags};
use loadstar_speech::{Config, Profile, Speech, UtteranceState};

const LINES: &[&str] = &[
    "Play, button",
    "Options, button",
    "Collection, button",
    "Quit, button",
    "Ace of Spades, card, selected",
    "Joker, common joker, sell 2 dollars",
    "Play hand, button",
    "Discard, button",
];

fn main() {
    let only_samples = std::env::args().nth(1).as_deref() == Some("samples");
    let s = Speech::new(Config::default());
    if !s.wait_ready(Duration::from_secs(10)) {
        eprintln!("first probe never published");
        std::process::exit(1);
    }
    idle(Duration::from_millis(1500)); // let the render voice warm up
    let os = Profile::default();
    let samples = Profile {
        flags: profile_flags::PLAY_SAMPLES,
        ..Profile::default()
    };
    let mut passes = vec![("played by Loadstar (PLAY_SAMPLES)", samples)];
    if !only_samples {
        passes.insert(0, ("spoken by the OS", os));
    }
    for (label, profile) in passes {
        println!("\n== {label}: interrupting every 350 ms");
        for line in LINES {
            let t = Instant::now();
            let id = s
                .speak(0, &profile, line, speak_flags::INTERRUPT)
                .expect("speak");
            // How long until it is Speaking (the OS hand-off, or the stream firing).
            while matches!(
                s.utterance(id).0,
                UtteranceState::Pending | UtteranceState::Unknown
            ) && t.elapsed() < Duration::from_secs(2)
            {
                idle(Duration::from_millis(1));
            }
            println!(
                "  {line:<40} speaking after {:>5.1} ms",
                t.elapsed().as_secs_f64() * 1000.0
            );
            idle(Duration::from_millis(350).saturating_sub(t.elapsed()));
        }
        idle(Duration::from_millis(2000));
        println!("== {label}: one line after two seconds of quiet");
        let id = s
            .speak(0, &profile, "Back after a pause", 0)
            .expect("speak");
        while !matches!(
            s.utterance(id).0,
            UtteranceState::Done | UtteranceState::Cancelled
        ) {
            idle(Duration::from_millis(10));
        }
        idle(Duration::from_millis(1000));
    }
}

/// Run the main thread's run loop while waiting: AVSpeech delivers rendered buffers on the main
/// queue, as a game's event loop would service it.
#[cfg(target_os = "macos")]
fn idle(d: Duration) {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    }
    // SAFETY: called on the main thread with CoreFoundation's own mode constant.
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, d.as_secs_f64(), 0);
    }
}

#[cfg(not(target_os = "macos"))]
fn idle(d: Duration) {
    std::thread::sleep(d);
}
