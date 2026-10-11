# Loadstar

Engine-agnostic navigation and spatial-awareness core for game accessibility mods, for games that don't expose their own pathfinding. Rust core, flat C ABI, bindings for C# (Unity, XNA/MonoGame), C++ (Unreal), and Lua (UE4SS).

Sibling of [Loadstone](https://github.com/bradjrenshaw) (mod manager): Loadstone loads the mods, Loadstar guides them.

Design: [`docs/spec.md`](docs/spec.md). Distilled from the Wayfinder/CSTW experiment; read the non-goals first.
Audio: [`docs/audio-spec.md`](docs/audio-spec.md). Speech: [`docs/speech-spec.md`](docs/speech-spec.md) — `loadstar-speech` (the `lv_*` surface): one categorised voice tree (screen readers through a runtime-loaded Prism, native SAPI 5, clipboard), queues with explicit interrupt semantics, polled utterance ids, render-to-PCM and positioned speech through the audio crate. Prism is pinned as the `external/prism` submodule; `tools/fetch-prism.ps1` downloads the matching `prism.dll`.

## Layout

```
crates/loadstar-core/   pure Rust, #![forbid(unsafe_code)], all logic and tests
crates/loadstar-ffi/    cdylib "loadstar", extern "C" translation layer, the only unsafe
bindings/csharp/        Loadstar.Net (P/Invoke + idiomatic wrapper) and Loadstar.Smoke (x86 net472 harness)
docs/                   spec and design notes
include/                generated C header
```

Deployment note: the native library is `loadstar.dll`. Never ship a managed assembly named
`Loadstar.dll` next to it — Windows filenames are case-insensitive and one will clobber the
other (the companion is therefore `Loadstar.Net.dll`). All exports are **cdecl**; on 32-bit
hosts, bindings must say so explicitly (`CallingConvention.Cdecl` in .NET).

## Building

The audio crate vendors [miniaudio](https://miniaud.io) (`crates/loadstar-audio/vendor`), compiled by the
`cc` crate, so a C compiler is needed (MSVC on Windows — the same toolchain Rust already uses).

```
cargo build --release                                  # x64
cargo build --release --target i686-pc-windows-msvc    # x86 (32-bit games)
cargo test --workspace
```

Regenerate the C header after changing the FFI crate:

```
cbindgen --crate loadstar-ffi --config crates/loadstar-ffi/cbindgen.toml --output include/loadstar.h
```
