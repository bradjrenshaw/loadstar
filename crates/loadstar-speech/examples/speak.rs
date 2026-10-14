//! Smoke harness: open the real backends, print the tree, speak one line through the root auto,
//! render one line through the first synthesizer, and time the game-thread calls.
//!
//!     cargo run -p loadstar-speech --example speak -- "Hello from Loadstar"
//!     PRISM_PATH=path/to/prism.dll cargo run ...   (otherwise: beside the exe, then the loader's search)

use std::time::{Duration, Instant};

use loadstar_speech::context::{profile_flags, speak_flags};
use loadstar_speech::{Config, Profile, RenderState, Speech, UtteranceState, features, ids};

fn main() {
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Loadstar speech is working.".to_string());
    let cfg = Config {
        prism_path: std::env::var_os("PRISM_PATH").map(Into::into),
        ..Config::default()
    };
    let t0 = Instant::now();
    let s = Speech::new(cfg);
    println!("create: {:?}", t0.elapsed());
    match s.prism_loaded_from() {
        Some(p) => println!("prism: loaded from {}", p.display()),
        None => println!("prism: not loaded"),
    }
    if !s.wait_ready(Duration::from_secs(10)) {
        eprintln!("first probe never published");
        std::process::exit(1);
    }
    println!("first probe published after {:?}", t0.elapsed());

    let tree = s.tree();
    for c in &tree.categories {
        println!(
            "[{}] {} (rank {}, auto {:#x})",
            c.key, c.name, c.rank, c.auto
        );
        for l in tree.leaves_in(c.id) {
            println!(
                "    {:#018x} {:<32} {:<6} {} {}",
                l.id,
                l.name,
                l.language,
                if l.available { "available" } else { "absent" },
                features::describe(l.features)
            );
        }
    }

    let r = s.resolve(&Profile::default(), features::SPEAK);
    println!(
        "root auto → {:#x}{}",
        r.voice,
        if r.fell_back { " (fell back)" } else { "" }
    );

    let t = Instant::now();
    let id = s.speak(0, &Profile::default(), &text, 0).expect("speak");
    let call = t.elapsed();
    let start = Instant::now();
    let mut state = s.utterance(id);
    while matches!(state.0, UtteranceState::Pending | UtteranceState::Speaking)
        && start.elapsed() < Duration::from_secs(15)
    {
        idle(Duration::from_millis(5));
        state = s.utterance(id);
    }
    println!(
        "speak call {call:?}; utterance {id} → {:?} via {:#x} after {:?}",
        state.0,
        state.2,
        start.elapsed()
    );

    // Render through whichever leaf can.
    let synth = tree.render_elsewhere().map(|i| tree.leaves[i].id);
    if let Some(v) = synth {
        let p = Profile {
            voice: v,
            flags: profile_flags::TRIM_SILENCE,
            ..Profile::default()
        };
        let t = Instant::now();
        match s.render(&p, &text, false) {
            Ok(job) => {
                while s.render_poll(job) == RenderState::Pending
                    && t.elapsed() < Duration::from_secs(15)
                {
                    idle(Duration::from_millis(5));
                }
                match s.render_take_owned(job) {
                    Some(pcm) => println!(
                        "render via {:#x}: {} frames @ {} Hz = {:?}, took {:?}",
                        v,
                        pcm.frames(),
                        pcm.rate,
                        pcm.duration(),
                        t.elapsed()
                    ),
                    None => println!("render failed ({:?})", s.render_poll(job)),
                }
            }
            Err(e) => println!("render refused: {e:?}"),
        }
    }

    // Render latency anatomy: cold (instance creation + synthesis) vs warm (synthesis only), short phrase.
    if let Some(v) = synth {
        let p = Profile {
            voice: v,
            ..Profile::default()
        };
        for label in ["cold", "warm", "warm"] {
            let t = Instant::now();
            if let Ok(job) = s.render(&p, "Left side.", false) {
                while s.render_poll(job) == RenderState::Pending
                    && t.elapsed() < Duration::from_secs(5)
                {
                    idle(Duration::from_micros(200));
                }
                let took = t.elapsed();
                if let Some(pcm) = s.render_take_owned(job) {
                    println!(
                        "render \"Left side.\" {label}: {took:?} for {:?} of audio",
                        pcm.duration()
                    );
                }
            }
        }
    }

    // Game-thread cost of the calls that run every frame.
    let n = 10_000;
    let t = Instant::now();
    for _ in 0..n {
        let _ = s.resolve(&Profile::default(), features::SPEAK);
    }
    println!("resolve: {:?} per call", t.elapsed() / n);
    let t = Instant::now();
    for _ in 0..n {
        let _ = s.utterance(id);
    }
    println!("utterance state: {:?} per call", t.elapsed() / n);
    let probe = Profile {
        voice: ids::ROOT_AUTO,
        ..Profile::default()
    };
    let t = Instant::now();
    let _ = s.speak(0, &probe, "", speak_flags::FRONT); // empty: the shortest full call path
    println!("speak (empty) call: {:?}", t.elapsed());
}

/// Wait like a game's main loop does. On macOS, Prism runs every AVSpeech call on the main
/// thread (dispatch_sync from the worker), so the waiting thread must service the main queue the
/// way a host's event loop would; a bare sleep deadlocks the worker.
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
