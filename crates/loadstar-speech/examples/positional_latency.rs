//! Call-to-first-audible for positional speech on a null device pumped at wall-clock pace:
//! the speech worker streams the render into a stream source while this thread renders 128-frame
//! periods every 2.67 ms and notes the first non-silent one. Excludes the real device's output
//! buffer (about 13 ms on a shared-mode WASAPI endpoint), which is the same for every path.
//!
//!     cargo run --release -p loadstar-speech --example positional_latency -- "Enemy to the left."

use std::time::{Duration, Instant};

use loadstar_audio::backend::NullBackend;
use loadstar_audio::device::{Device, DeviceConfig};
use loadstar_audio::space::{SourceDesc, SpaceMode};
use loadstar_core::geometry::{Bounds, Vec3};
use loadstar_speech::context::{PositionalTarget, profile_flags};
use loadstar_speech::{Config, Profile, QueueOptions, Speech, UtteranceState, features};

fn main() {
    let text = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Enemy to the left.".to_string());
    let s = Speech::new(Config {
        prism_path: std::env::var_os("PRISM_PATH").map(Into::into),
        ..Config::default()
    });
    assert!(s.wait_ready(Duration::from_secs(10)));
    // Let the post-probe pre-warm finish, as it would during a game's boot before the first event.
    std::thread::sleep(Duration::from_millis(300));
    let tree = s.tree();
    let Some(voice) = tree.render_elsewhere().map(|i| tree.leaves[i].id) else {
        eprintln!("no voice can render here");
        return;
    };
    println!(
        "voice {:#x} ({})",
        voice,
        tree.leaf(voice).map(|l| l.name.as_str()).unwrap_or("?")
    );

    const RATE: u32 = 48000;
    const PERIOD: usize = 128;
    let mut dev = Device::open_null(DeviceConfig::default(), NullBackend::new(RATE, PERIOD));
    let space = dev.space_create(SpaceMode::Planar, 1.0, 1.0).unwrap();
    let queue = s.queue_create(QueueOptions {
        max_concurrent: 0,
        gap_ms: 0,
    });
    let period = Duration::from_secs_f64(PERIOD as f64 / RATE as f64);
    let mut out = vec![0.0f32; PERIOD * 2];

    for round in 0..4 {
        let target = PositionalTarget {
            intake: dev.intake(),
            space,
            desc: SourceDesc::buffer(0, 0, Bounds::Point(Vec3::new(-3.0, 0.0, 0.0))),
        };
        let profile = Profile {
            voice,
            flags: profile_flags::TRIM_SILENCE,
            ..Profile::default()
        };
        let t0 = Instant::now();
        let (utt, _) = s
            .speak_at(queue, &profile, &text, 0, target)
            .expect("speak_at");
        let call = t0.elapsed();
        let mut first_audible = None;
        let mut next = t0;
        let mut periods = 0u64;
        while t0.elapsed() < Duration::from_secs(10) {
            // Pump one period at the real device cadence.
            while Instant::now() < next {
                std::hint::spin_loop();
            }
            next += period;
            dev.render(&mut out);
            periods += 1;
            if first_audible.is_none() && out.iter().any(|v| v.abs() > 1e-4) {
                first_audible = Some(t0.elapsed());
            }
            if first_audible.is_some() && s.utterance(utt).0 == UtteranceState::Done {
                break;
            }
        }
        println!(
            "round {round}: speak_at call {call:?}; first audible after {}; utterance {:?} after {:?} ({} periods)",
            first_audible
                .map(|d| format!("{d:?}"))
                .unwrap_or_else(|| "never".into()),
            s.utterance(utt).0,
            t0.elapsed(),
            periods
        );
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = features::SPEAK;
}
