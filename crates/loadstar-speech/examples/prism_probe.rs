//! Step-by-step Prism loader probe for bisecting a crash in a new Prism build:
//!     cargo run -p loadstar-speech --example prism_probe -- path/to/prism.dll

use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::Write;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct PrismConfig {
    version: u8,
}

fn step(s: &str) {
    eprintln!("-> {s}");
    let _ = std::io::stderr().flush();
}

fn main() {
    let path = std::env::args().nth(1).expect("path to prism.dll");
    step(&format!("load {path}"));
    // SAFETY: a probe; every call follows prism.h at the pinned tag.
    unsafe {
        let lib = libloading::Library::new(&path).expect("load");
        step("resolve symbols");
        let config_init = lib
            .get::<unsafe extern "C" fn() -> PrismConfig>(b"prism_config_init\0")
            .ok()
            .map(|s| *s);
        let init = *lib
            .get::<unsafe extern "C" fn(*mut PrismConfig) -> *mut c_void>(b"prism_init\0")
            .unwrap();
        let count = *lib
            .get::<unsafe extern "C" fn(*mut c_void) -> usize>(b"prism_registry_count\0")
            .unwrap();
        let id_at = *lib
            .get::<unsafe extern "C" fn(*mut c_void, usize) -> u64>(b"prism_registry_id_at\0")
            .unwrap();
        let name = *lib
            .get::<unsafe extern "C" fn(*mut c_void, u64) -> *const c_char>(
                b"prism_registry_name\0",
            )
            .unwrap();
        let create = *lib
            .get::<unsafe extern "C" fn(*mut c_void, u64) -> *mut c_void>(
                b"prism_registry_create\0",
            )
            .unwrap();
        let features = *lib
            .get::<unsafe extern "C" fn(*mut c_void) -> u64>(b"prism_backend_get_features\0")
            .unwrap();
        let free = *lib
            .get::<unsafe extern "C" fn(*mut c_void)>(b"prism_backend_free\0")
            .unwrap();
        let initialize = *lib
            .get::<unsafe extern "C" fn(*mut c_void) -> c_int>(b"prism_backend_initialize\0")
            .unwrap();
        let speak = *lib
            .get::<unsafe extern "C" fn(*mut c_void, *const c_char, bool) -> c_int>(
                b"prism_backend_speak\0",
            )
            .unwrap();
        let shutdown = *lib
            .get::<unsafe extern "C" fn(*mut c_void)>(b"prism_shutdown\0")
            .unwrap();

        let mut cfg = PrismConfig { version: 2 };
        if std::env::var_os("SKIP_CONFIG_INIT").is_some() {
            step("skipping prism_config_init");
        } else if let Some(ci) = config_init {
            step("prism_config_init");
            cfg = ci();
            step(&format!("config = {cfg:?}"));
        } else {
            step("no prism_config_init export");
        }
        let mode = std::env::args().nth(2).unwrap_or_default();
        let ctx = if mode == "null" {
            step("prism_init(NULL)");
            init(std::ptr::null_mut())
        } else {
            step("prism_init(&cfg)");
            init(&mut cfg)
        };
        step(&format!("ctx = {ctx:?}"));
        if ctx.is_null() {
            return;
        }
        let n = count(ctx);
        step(&format!("registry count = {n}"));
        for i in 0..n {
            let id = id_at(ctx, i);
            let nm = name(ctx, id);
            let nm = if nm.is_null() {
                "?".to_string()
            } else {
                CStr::from_ptr(nm).to_string_lossy().into_owned()
            };
            step(&format!("[{i}] {id:#018x} {nm}: create"));
            let be = create(ctx, id);
            step(&format!("    be = {be:?}"));
            if be.is_null() {
                continue;
            }
            let f = features(be);
            step(&format!(
                "    features = {f:#x} (runtime-supported: {})",
                f & 1 != 0
            ));
            if f & 1 != 0 && nm.eq_ignore_ascii_case("nvda") {
                step("    initialize");
                let r = initialize(be);
                step(&format!("    init -> {r}"));
                if r == 0 || r == 15 {
                    step("    speak");
                    let r = speak(be, c"Prism probe speaking through NVDA.".as_ptr(), false);
                    step(&format!("    speak -> {r}"));
                    std::thread::sleep(std::time::Duration::from_millis(1500));
                }
            }
            step("    free");
            free(be);
        }
        step("shutdown");
        shutdown(ctx);
        step("done");
    }
}
