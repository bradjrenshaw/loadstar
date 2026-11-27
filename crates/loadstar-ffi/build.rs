//! Windows builds of loadstar.dll carry identifying metadata: a VERSIONINFO resource (what
//! Explorer's Details tab and `Get-Item … | % VersionInfo` show) and a real PE checksum.
//!
//! This is not cosmetic. An unsigned native dll with no version resource and a zero checksum is
//! exactly the shape antivirus machine-learning models distrust; Microsoft Defender quarantined
//! an anonymous build of this library as `Trojan:Win32/Bearfoos.A!ml`, silencing a blind player's
//! game. Saying who we are does not guarantee a clean verdict, but it removes the cheapest
//! reasons for a bad one.
//!
//! The version comes from the workspace (`CARGO_PKG_VERSION`), so bumping it there is enough.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../LICENSE");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    // link.exe leaves the optional header's CheckSum at zero unless asked; /RELEASE fills it in.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-cdylib-link-arg=/RELEASE");
    }

    // The resource compiler (rc.exe from the Windows SDK, or windres) runs on the build machine,
    // so this needs a Windows host; a cross build from elsewhere simply ships without the block.
    #[cfg(windows)]
    {
        let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
        let repository = std::env::var("CARGO_PKG_REPOSITORY").unwrap_or_default();

        let mut res = winresource::WindowsResource::new();
        res.set("ProductName", "Loadstar")
            .set("FileDescription", "Loadstar: spatial audio, speech and navigation for game accessibility mods")
            .set("CompanyName", "Bradley Renshaw")
            .set("LegalCopyright", "Copyright (c) 2026 Bradley Renshaw. MIT License.")
            .set("InternalName", "loadstar")
            .set("OriginalFilename", "loadstar.dll")
            .set("FileVersion", &version)
            .set("ProductVersion", &version)
            .set("Comments", &repository)
            .set_language(0x0409); // en-US
        if let Err(e) = res.compile() {
            // Never fail the build over metadata: a missing SDK still yields a working dll.
            println!("cargo:warning=loadstar.dll built WITHOUT version info: {e}");
        }
    }
}
