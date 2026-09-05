# Loadstar Audio — Design Specification

*Companion to `spec.md`. Status: draft for review, pre-implementation.*

---

## 1. Purpose and scope

`loadstar-audio` places sounds in a space and mixes them for a listener, glitch-free and with the lowest latency the platform allows. It exists because game accessibility mods need cues — footsteps, wall proximity, object beacons, menu feedback — that managed audio stacks deliver with 50+ ms of latency and periodic crackling.

It follows the same rules as the navigation core: tools not a framework, data in and numbers out, no callbacks, no strings, and it decides nothing about *what* should play — the mod owns that, exactly as it owns input for navigation.

### In scope (v1)

- A native mixer thread on Windows via **WASAPI shared mode** (`IAudioClient3` low-latency periods, MMCSS scheduling), behind a backend trait.
- **Audio spaces**: a listener plus sources with geometry (`Bounds` from the core), 2D or 3D panning.
- Sources declared as data and diffed per frame, the same contract as entities.
- Distance attenuation, stereo panning, per-source gain/pitch with smoothing, looping, one-shots, procedural tones.
- PCM buffers uploaded through the ABI; WAV decoding as a companion convenience.

### Out of scope (v1)

- Exclusive-mode WASAPI (it silences the game's own audio).
- HRTF / binaural rendering — a later, deliberate addition once plain panning has been evaluated with blind players.
- Streaming, music, OGG/MP3 decoding, effects buses, reverb.
- Non-Windows backends (the trait exists so PipeWire/cpal can slot in for Linux and Steam Deck).
- Deciding what plays: no "footstep system", no "wall tones" — those are mod policy built from tones and sources.

---

## 2. Latency: targets and reality

| Path | Period | Notes |
|---|---|---|
| WASAPI shared, `IAudioClient` | ~10 ms | Engine default. Baseline fallback. |
| WASAPI shared, `IAudioClient3` | driver minimum, typically 2.67–3 ms @ 48 kHz | **Target.** `GetSharedModeEnginePeriod` → `InitializeSharedAudioStream`. |
| WASAPI exclusive | hardware minimum | Not used: takes over the device. |

Effective cue latency is *game frame time + audio period + engine overhead*; a mod issues commands once per frame, so ~3 ms periods put the audio path well under the frame. **The design goal is zero glitches at the smallest period the driver offers**, not the headline number: the crackling under NAudio comes from mixing on a managed thread (GC pauses, scheduler preemption), which is what a native MMCSS thread removes.

Rules the mixer thread lives by:

- Pinned to MMCSS task `"Pro Audio"` (`AvSetMmThreadCharacteristicsW`).
- **No allocation, no locks, no syscalls other than the WASAPI render calls** inside the period callback. Command intake is a lock-free SPSC ring buffer; parameter state is owned by the audio thread.
- Every audible parameter change (gain, pan, pitch, start/stop) is **ramped** over a few ms; nothing steps discontinuously.
- Falls back to the 10 ms path if `IAudioClient3` is unavailable, and logs which period it got.
- Device loss / format change (headphones unplugged) is detected and the stream reopened; sources keep their state.

---

## 3. Concepts

### Device

One per process: `la_device_open(config) → handle`. Config: preferred sample rate (0 = device mix format), requested period (0 = minimum), master gain. Opens the default render endpoint in shared mode; mixes in f32 at the device's mix format (typically 48 kHz stereo). Reports the period actually obtained.

### Buffers

Immutable PCM sample data: `la_buffer_create(device, samples: *const f32, frames, channels, sample_rate) → buffer_id`. Resampled to the device rate at load (once), stored mono or stereo. Buffers are reference-counted by sources; freeing a buffer in use is an error. Procedural buffers: `la_buffer_tone(device, hz, ms, attack_ms, release_ms)` and `la_buffer_click(device, ms)` synthesize on the spot — this replaces wayfinder's `ToneGenerator`.

### Space

An `AudioSpace` ≈ a navigation context: a listener plus sources. `la_space_create(device, mode) → space` where mode is `Planar` (pan by XZ bearing, elevation ignored — 2D games) or `Spatial` (full 3D vector). One type; the modes differ only in panning math, unlike map contexts whose *projection* differs.

Several spaces can coexist (overworld vs. dungeon, UI vs. world); each mixes into the device with its own gain.

### Listener

`la_listener_set(space, position, forward, up)` each frame. Planar mode ignores `up` and the vertical component of `forward`. The listener may also carry a `velocity` field for later Doppler; unused in v1.

### Sources

Sources carry **`Bounds`** (the core's tagged union, byte-identical `LsBoundsDesc`). The emitting point for panning and distance is `bounds.nearest_point(listener)`; when `bounds.contains(listener)` the source plays centered at full gain. A river along a polygon prism, a crowd in a box, a beacon at a point — all one mechanism, and it is the same geometry layer the tracker already uses.

```
LaSourceDesc {                 // 96 bytes; all numeric, mod-assigned id
    id:          u64           // reuse the nav entity id to tie a sound to an object
    buffer:      u32
    flags:       u32           // LOOP | PLAYING | RESTART | PAUSED | RELATIVE ...
    bounds:      LsBoundsDesc  // 40 bytes
    gain:        f32           // linear, 0..1 (+headroom allowed)
    pitch:       f32           // playback rate multiplier, 1.0 = native
    min_distance: f32          // full gain within this radius of the nearest point
    max_distance: f32          // silent beyond; 0 = never attenuate
    rolloff:     f32           // exponent for inverse-distance falloff between min and max
    lowpass_hz:  f32           // 0 = off; simple one-pole cut for occlusion/muffling
    priority:    u8            // voice stealing when over the voice cap
    _reserved:   [u8; 7]
}
```

Flags:
- `LOOP` — repeat until stopped.
- `PLAYING` — desired state; clearing it stops (ramped). One-shots clear themselves when done and are then reported as finished (see status).
- `RESTART` — edge-triggered: retrigger from the start this frame even if already playing (a footstep every step). The crate clears it after acting.
- `RELATIVE` — position is relative to the listener (UI sounds, breath, heartbeat): no spatialization, no attenuation.

### Declaring the soundscape

Same contract as entities:

- **`la_space_sync(space, sources[], count, stats*)`** — declarative snapshot: upsert every record, stop and remove every source absent from the snapshot. The crate diffs numerically; an unchanged record costs nothing, a moved one updates its pan/gain targets (ramped), a changed buffer restarts.
- **`la_source_upsert` / `la_source_remove`** — explicit deltas for mods that prefer event-driven control.

Either way the game thread never touches audio state directly: every call encodes a command into the ring buffer, and the audio thread applies it at the next period boundary. Consistency across a frame's calls is therefore natural — they land together.

### Reading back

No callbacks, so completion is polled: `la_source_state(space, id, out) → {playing: bool, position_frames: u64}` and `la_space_finished(space, out_ids[], cap, written)` drains the one-shots that ended since the last call. Mods that fire-and-forget never call either.

---

## 4. Spatialization (v1)

- **Emitting point**: `p = bounds.nearest_point(listener)`; `d = p - listener`.
- **Distance gain**: 1 inside `min_distance`; `(min / dist)^rolloff` between; 0 beyond `max_distance`; `max_distance == 0` disables attenuation. A source the listener is inside (`contains`) is at distance 0.
- **Planar pan**: bearing of `d` in the listener's local XZ frame → constant-power stereo pan. Directly ahead/behind are centered; a *front/back* cue is deliberately left to a later HRTF or filter stage — for tile games the mod usually supplies it in words.
- **Spatial pan**: same azimuth panning; elevation contributes a gentle high-shelf tilt (up = brighter) as a cheap cue, gated behind a flag so it can be turned off.
- **Low-pass**: one-pole at `lowpass_hz` when set; mods use it for occlusion ("behind a wall") computed from their own line-of-sight or the nav reachability.
- **Voice cap** (default 32): when exceeded, the quietest lowest-priority sources are stolen with a ramp-out.

All of this is per-period math on a handful of sources — negligible next to the resampling, which happens at load.

---

## 5. Crate layout and the backend trait

```
crates/loadstar-audio/         pure Rust: spaces, sources, mixer, resampler, DSP
  src/backend/mod.rs           trait Backend { fn open(..) -> Result<Stream>; ... }
  src/backend/wasapi.rs        windows crate, IAudioClient3, MMCSS  (cfg(windows))
  src/backend/null.rs          silent backend for tests and CI
crates/loadstar-ffi/           gains the `la_*` surface behind a `audio` cargo feature
bindings/csharp/Loadstar.Net/  Audio.cs: AudioDevice / AudioSpace / SourceDesc wrappers,
                               WavFile.Load(path) → f32 PCM convenience
```

The mixer is tested against the null backend with synthetic time: property tests for pan/gain math, ramp continuity (no sample-to-sample jump above a threshold), voice stealing, and sync diffing. The WASAPI backend is exercised by the smoke harness (a positioned tone from the 32-bit .NET process) — the same "hostile end first" approach as the navigation skeleton.

The backend trait is minimal on purpose: open a stream at a format and period, hand the mixer a `&mut [f32]` per period, report the achieved period, and signal device loss. Everything spatial lives above it.

---

## 6. ABI conventions

Identical to `spec.md` §9: `la_` prefix, `LsStatus` return codes (shared enum, extended with `DeviceUnavailable`, `BufferInUse`), opaque handles, flat structs with pinned sizes, no callbacks, no strings, cdecl, `[Out]` on output arrays in .NET. The same cross-boundary rules as navigation apply because the same lessons do.

Threading contract: the device and its spaces are driven from one game thread; the audio thread is internal and never calls out.

---

## 7. What the CSTW mod would build on it

- Footsteps: one `click` buffer, a `RELATIVE` source, `RESTART` flag set on each tile arrival.
- Wall proximity: four looped tone sources placed one tile away in each cardinal direction, gain driven by the ray-scan distances the nav grid can already answer; silent when open. (The old `WallToneSystem`, now ~20 lines of mod code.)
- Object beacons: a looped source per tracked entity, bounds = the entity's bounds, id = the entity's id, driven straight from the same snapshot the nav registry gets.
- Menu/dialog cues: `RELATIVE` one-shots.

None of that is in the crate; all of it is a few lines because the crate is the right shape.

---

## 8. Roadmap

1. **Spec review** (this document).
2. **Mixer with the null backend** — spaces, sources, sync diff, pan/gain/ramps, property tests.
3. **WASAPI backend** — `IAudioClient3` + MMCSS, device-loss recovery, period reporting.
4. **FFI + .NET**, smoke harness plays a positioned tone from x86 net472; then inside CSTW.
5. **CSTW footsteps and wall tones** on the new crate; retire `WaveOutToneOutput` and the inlined `ToneGenerator`.
6. Later: HRTF evaluation, Linux backend, streaming.

## 9. Open questions for review

- **Pan law and front/back**: constant-power stereo only in v1, or should Planar mode already offer a subtle front/back timbre cue?
- **Units**: distances in the space's own units (tiles for CSTW); should `min/max_distance` defaults be per-space config rather than per-source?
- **Voice cap and priority** — 32 voices enough? Should stealing be configurable per space?
- **Feature flag vs. separate DLL**: fold `la_*` into `loadstar.dll` behind a cargo feature (single native file, matches the single-DLL mod), or ship `loadstar_audio.dll` separately? Proposal: one DLL, one feature flag.
- **Buffer ownership**: upload-and-copy (simple, proposed) vs. mod-owned memory (zero-copy, lifetime hazards across the ABI). Proposed: copy.
