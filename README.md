# Loadstar

Engine-agnostic navigation and spatial-awareness core for game accessibility mods, for games that don't expose their own pathfinding. Rust core, flat C ABI, bindings for C# (Unity, XNA/MonoGame), C++ (Unreal), and Lua (UE4SS).

Sibling of [Loadstone](https://github.com/bradjrenshaw) (mod manager): Loadstone loads the mods, Loadstar guides them.

Design: [`docs/spec.md`](docs/spec.md). Distilled from the Wayfinder/CSTW experiment; read the non-goals first.

## Layout

```
crates/loadstar-core/   pure Rust, #![forbid(unsafe_code)], all logic and tests
crates/loadstar-ffi/    cdylib "loadstar", extern "C" translation layer, the only unsafe
bindings/csharp/        .NET companion (P/Invoke + idiomatic wrapper + default formatters)
docs/                   spec and design notes
```

## Building

```
cargo build --release                                  # x64
cargo build --release --target i686-pc-windows-msvc    # x86 (32-bit games)
cargo test --workspace
```

Regenerate the C header after changing the FFI crate:

```
cbindgen --crate loadstar-ffi --config crates/loadstar-ffi/cbindgen.toml --output include/loadstar.h
```
