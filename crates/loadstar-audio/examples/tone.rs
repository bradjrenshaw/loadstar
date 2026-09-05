//! Plays a tone that orbits the listener for a few seconds through the WASAPI backend and
//! prints the format the driver actually gave us. Run: cargo run -p loadstar-audio --example tone
use std::time::Duration;

use loadstar_audio::device::{Device, DeviceConfig};
use loadstar_audio::space::{SourceDesc, SpaceMode};
use loadstar_audio::{Bounds, Vec3};

fn main() {
    #[cfg(windows)]
    {
        let mut d = Device::open_wasapi(DeviceConfig::default()).expect("open default audio device");
        let info = d.info();
        println!(
            "WASAPI: {} Hz, {} frames per period ({:.2} ms), low-latency path: {}",
            info.sample_rate,
            info.period_frames,
            info.period_frames as f32 * 1000.0 / info.sample_rate as f32,
            d.low_latency()
        );
        let space = d.space_create(SpaceMode::Planar, 1.0, 1.0).unwrap();
        // A bright ping: partials at 2, 4 and 6 kHz with a fast decay. The rear cue is a
        // high-shelf cut above 3 kHz, so the beacon must have energy up there to be heard as "behind".
        let rate = info.sample_rate;
        let ping: Vec<f32> = (0..(rate as usize * 120 / 1000))
            .map(|i| {
                let t = i as f32 / rate as f32;
                let env = (1.0 - t / 0.12).powi(2);
                let partial = |hz: f32, a: f32| a * (t * hz * std::f32::consts::TAU).sin();
                (partial(2000.0, 0.5) + partial(4000.0, 0.3) + partial(6000.0, 0.2)) * env
            })
            .collect();
        let tone = d.buffer_create(&ping, 1, rate).unwrap();
        d.set_listener(space, Vec3::default(), Vec3::new(0.0, 0.0, -1.0), Vec3::new(0.0, 1.0, 0.0)).unwrap();

        // A looping beacon circling the listener at 3 m over 6 seconds, ahead → right → behind → left.
        let steps = 120;
        for i in 0..steps {
            let a = i as f32 / steps as f32 * std::f32::consts::TAU;
            let pos = Vec3::new(3.0 * a.sin(), 0.0, -3.0 * a.cos());
            d.sync(space, &[SourceDesc::buffer(1, tone, Bounds::Point(pos)).repeats(0, 180.0).with_gain(0.5)]).unwrap();
            if i % 30 == 0 {
                println!("{}", ["ahead", "right", "behind", "left"][i / 30]);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        d.sync(space, &[]).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        println!("finished: {:?}", d.take_finished());
        println!("done");
    }
    #[cfg(not(windows))]
    println!("WASAPI example runs on Windows only");
}
