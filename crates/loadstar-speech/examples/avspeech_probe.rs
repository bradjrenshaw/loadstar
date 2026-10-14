//! AVSpeechSynthesizer threading probe (macOS): speak and render from a background thread while
//! the main thread only sleeps, i.e. never services the main queue. If either hangs, the native
//! backend cannot run on the worker. PROBE_THREAD_RUNLOOP=1 runs the speaking thread's own run
//! loop while it waits; PROBE_MAIN_RUNLOOP=1 services the main queue.
//!     cargo run -p loadstar-speech --example avspeech_probe

#[cfg(target_os = "macos")]
fn main() {
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2_avf_audio::{
        AVAudioBuffer, AVAudioPCMBuffer, AVSpeechBoundary, AVSpeechSynthesisVoice,
        AVSpeechSynthesizer, AVSpeechUtterance,
    };
    use objc2_foundation::{NSString, NSThread};

    let worker = std::thread::spawn(|| unsafe {
        let voices = AVSpeechSynthesisVoice::speechVoices();
        println!("voices: {}", voices.count());
        let lang = AVSpeechSynthesisVoice::currentLanguageCode();
        let def = AVSpeechSynthesisVoice::voiceWithLanguage(Some(&lang));
        println!(
            "default for {lang}: {:?}",
            def.as_ref().map(|v| v.name().to_string())
        );

        let synth = AVSpeechSynthesizer::new();
        let utt = AVSpeechUtterance::speechUtteranceWithString(&NSString::from_str(
            "Native AV speech, from a background thread.",
        ));
        utt.setVoice(def.as_deref());
        let t = Instant::now();
        synth.speakUtterance(&utt);
        println!("speakUtterance returned in {:?}, isSpeaking={}", t.elapsed(), synth.isSpeaking());
        let mut started = None;
        while t.elapsed() < Duration::from_secs(10) {
            let s = synth.isSpeaking();
            if s && started.is_none() {
                started = Some(t.elapsed());
            }
            if !s && started.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        println!(
            "isSpeaking true at {:?}, false again at {:?}",
            started,
            t.elapsed()
        );
        synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate);

        // Render.
        let r = AVSpeechSynthesizer::new();
        let utt = AVSpeechUtterance::speechUtteranceWithString(&NSString::from_str(
            "Rendered to memory.",
        ));
        utt.setVoice(def.as_deref());
        let got = Arc::new(Mutex::new((0usize, 0usize, 0.0f64, 0u32, false, false)));
        let g = Arc::clone(&got);
        let cb = RcBlock::new(move |buf: NonNull<AVAudioBuffer>| {
            let buf: &AVAudioBuffer = buf.as_ref();
            let mut g = g.lock().unwrap();
            g.5 |= NSThread::isMainThread_class();
            if let Some(pcm) = buf.downcast_ref::<AVAudioPCMBuffer>() {
                let fmt = pcm.format();
                let n = pcm.frameLength() as usize;
                if n == 0 {
                    g.4 = true; // a zero-length buffer marks the end
                }
                g.0 += 1;
                g.1 += n;
                g.2 = fmt.sampleRate();
                g.3 = fmt.channelCount();
            }
        });
        let t = Instant::now();
        r.writeUtterance_toBufferCallback(&utt, &*cb as *const _ as *mut _);
        while t.elapsed() < Duration::from_secs(10) && !got.lock().unwrap().4 {
            if std::env::var_os("PROBE_THREAD_RUNLOOP").is_some() {
                run_current_loop(Duration::from_millis(5));
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let g = got.lock().unwrap();
        println!(
            "render: {} buffers, {} frames @ {} Hz x{} ch, end={} on_main={} after {:?}",
            g.0, g.1, g.2, g.3, g.4, g.5, t.elapsed()
        );
    });
    // The main thread deliberately only sleeps.
    // PROBE_MAIN_RUNLOOP=1 services the main queue instead, as a game's event loop would.
    let pump = std::env::var_os("PROBE_MAIN_RUNLOOP").is_some();
    while !worker.is_finished() {
        if pump {
            run_current_loop(Duration::from_millis(5));
        } else {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    worker.join().unwrap();
}

/// Run the calling thread's own run loop (not the main one) for `d`.
#[cfg(target_os = "macos")]
fn run_current_loop(d: std::time::Duration) {
    use std::ffi::c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFRunLoopDefaultMode: *const c_void;
        fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    }
    // SAFETY: CoreFoundation's own mode constant; runs this thread's loop.
    unsafe {
        CFRunLoopRunInMode(kCFRunLoopDefaultMode, d.as_secs_f64(), 0);
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macOS only");
}
