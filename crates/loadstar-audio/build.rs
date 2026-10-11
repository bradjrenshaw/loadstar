fn main() {
    #[cfg(feature = "miniaudio")]
    {
        println!("cargo:rerun-if-changed=vendor/miniaudio/loadstar_ma.c");
        println!("cargo:rerun-if-changed=vendor/miniaudio/miniaudio.h");
        let target = std::env::var("TARGET").unwrap_or_default();
        let mut build = cc::Build::new();
        build
            .file("vendor/miniaudio/loadstar_ma.c")
            .include("vendor/miniaudio")
            .warnings(false)
            .opt_level(2);
        if target.contains("linux") {
            println!("cargo:rustc-link-lib=dl");
            println!("cargo:rustc-link-lib=pthread");
            println!("cargo:rustc-link-lib=m");
        }
        build.compile("loadstar_ma");
    }
}
