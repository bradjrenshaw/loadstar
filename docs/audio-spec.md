# Loadstar Audio — Design Specification

*Companion to `spec.md`. Status: reviewed; decisions in §9. Pre-implementation.*

*Draft 2 folds in what the Wrath of the Righteous mod (`wotr-access`) learned by ear: the head model in its `Spatializer`, the block-rate retuning discipline in its `PositionalEmitter`, ear-fixed wall-tone banks, the facing-rotated listener frame, the slope oscillator, and positional speech. Those are the validated behaviours; this document turns them into data the crate evaluates.*

---

## 1. Purpose and scope

`loadstar-audio` places sounds in a space and mixes them for a listener, glitch-free and with the lowest latency the platform allows. It exists because game accessibility mods need cues — footsteps, wall proximity, object beacons, positioned speech, menu feedback — that managed audio stacks deliver with 50+ ms of latency and periodic crackling.

It follows the same rules as the navigation core: tools not a framework, data in and numbers out, no callbacks, no strings, and it decides nothing about *what* should play — the mod owns that, exactly as it owns input for navigation.

### In scope (v1)

- A native mixer thread on Windows via **WASAPI shared mode** (`IAudioClient3` low-latency periods, MMCSS scheduling), behind a backend trait.
- **Audio spaces**: a listener plus sources with geometry (`Bounds` from the core), planar or spatial panning.
- **Effect chains as data** on sources, buses, spaces and the device master — including the head-model spatializer with every cue individually switchable.
- Sources declared as data and diffed per frame, the same contract as entities; persistent sources are re-spatialized every period against the moving listener automatically.
- Buffer sources (PCM, including runtime TTS output) and procedural sources (oscillator, click), looping, one-shots, retrigger.
- PCM buffers uploaded through the ABI; WAV decoding as a companion convenience.

### Out of scope (v1)

- Exclusive-mode WASAPI (it silences the game's own audio).
- HRTF / binaural rendering *in v1*. The head model below is deliberately simpler and was tuned by ear. The renderer stage is pluggable from day one (§4.8) so Steam Audio (HRTF, geometry-based occlusion, reflections) can be added without changing the model — it is the first planned plug-in, not an afterthought.
- Streaming, music, OGG/MP3 decoding, reverb, arbitrary routing graphs (v1 is chains and buses — see §4).
- Non-Windows backends (the trait exists so PipeWire/cpal can slot in for Linux and Steam Deck).
- Deciding what plays: no "footstep system", no "wall tones" — those are mod policy built from sources and chains (§7 shows how small they become).

---

## 2. Latency: targets and reality

| Path | Period | Notes |
|---|---|---|
| WASAPI shared, `IAudioClient` | ~10 ms | Engine default. Baseline fallback. |
| WASAPI shared, `IAudioClient3` | driver minimum, typically 2.67–3 ms @ 48 kHz | **Target.** `GetSharedModeEnginePeriod` → `InitializeSharedAudioStream`. |
| WASAPI exclusive | hardware minimum | Not used: takes over the device. |

Effective cue latency is *game frame time + audio period + engine overhead*; a mod issues commands once per frame, so ~3 ms periods put the audio path well under the frame. **The design goal is zero glitches at the smallest period the driver offers**, not the headline number: the crackling under NAudio comes from mixing on a managed thread (GC pauses, scheduler preemption), which a native MMCSS thread removes.

Rules the mixer thread lives by:

- Pinned to MMCSS task `"Pro Audio"` (`AvSetMmThreadCharacteristicsW`).
- **No allocation, no locks, no syscalls other than the WASAPI render calls** inside the period callback. Command intake is a lock-free SPSC ring buffer; all audio state is owned by the audio thread.
- Every audible parameter is a **target**: gains, pans, delays and filter parameters ramp toward their targets; nothing steps discontinuously (§4.5).
- Falls back to the 10 ms path if `IAudioClient3` is unavailable, and reports which period it got.
- Device loss / format change (headphones unplugged) is detected and the stream reopened; sources keep their state.

---

## 3. Concepts

### Device

One per process: `la_device_open(config) → handle`. Config: preferred sample rate (0 = device mix format), requested period (0 = minimum), master gain, voice cap (0 = default 64). Opens the default render endpoint in shared mode; mixes in f32 at the device's mix format (typically 48 kHz stereo). Reports the period actually obtained. Owns the **master chain** (§4).

### Buffers

Immutable PCM: `la_buffer_create(device, samples: *const f32, frames, channels, sample_rate) → buffer_id`. Resampled to the device rate at load (once). Reference-counted by sources; freeing a buffer in use is an error. Runtime speech PCM from a TTS engine is just another buffer — that is how positioned, overlapping utterances work (§7).

Procedural convenience: `la_buffer_click(device, ms)` and `la_buffer_tone(device, hz, ms, attack_ms, release_ms)` synthesize one-shot buffers (the old `ToneGenerator`). Continuous tones are a *source kind*, not a buffer (below).

### Space

An `AudioSpace` ≈ a navigation context: a listener, buses, sources and a chain. `la_space_create(device, mode, gain, unit_scale) → space` with mode `Planar` (pan by XZ bearing, elevation ignored) or `Spatial` (full 3D), and `unit_scale` = the space's units per metre (1 for a metric game, e.g. 1/1.5 for a tile game whose tiles are 1.5 m) so metre-based defaults land sensibly. One type; the modes differ only in how the spatializer reads the geometry.

Spaces coexist (overworld vs. dungeon; a UI space with no spatialization at all) and sum into the device master.

A space chooses its **renderer** (§4.8) — the built-in head model by default — and may carry **acoustic geometry** for renderers that use it: `la_space_set_geometry(space, vertices[], triangles[], materials[])`, or `la_space_geometry_from_tiles(space, tile_context)` to export a nav grid's solid cells as boxes. The head model ignores geometry; Steam Audio's occlusion and reflections consume it.

Renderers that simulate (occlusion rays, reflections) need a per-frame step: **`la_space_update(space, dt)`**, called by the mod once per frame after positioning the listener and syncing sources. It runs cheap work on the calling thread and hands heavy work to an internal worker; results reach the audio thread through the command queue. The head model's update is a no-op, so v1 mods can call it unconditionally.

### Listener

`la_listener_set(space, position, forward, up)` each frame. **Facing matters even in 2D**: the ear frame rotates with `forward` so sounds sit around the head, not around the map. What stays world-aligned — spoken bearings, grid movement — is a mod decision; wotr keeps speech world-aligned while the ears turn, and pings a "north" cue positioned at world north whenever the facing crosses a 45° line, which is a few lines of mod code on this API.

### Buses

Within a space, sources belong to a **bus**: `la_bus_create(space, gain) → bus_id`. A bus is a stereo sum with its own gain and chain — "wall tones", "beacons", "speech", "footsteps" each get one, which is exactly the per-system volume sliders wotr exposes, and lets a whole system be muted, ducked or filtered together. Bus 0 exists by default.

### Sources

Sources carry **`Bounds`** (the core's tagged union, byte-identical `LsBoundsDesc`). The emitting point is `bounds.nearest_point(listener)`; when `bounds.contains(listener)` the source is at distance zero, centered, full gain. A river along a polygon prism, a crowd in a box, a beacon at a point — one mechanism, the same geometry layer the tracker uses.

Sources are **persistent objects re-evaluated every period**: a one-shot fired from a chest keeps tracking pan, gain, delay and filters as the listener walks past it, until it drains. wotr had to bolt this on (`SpatialSources`); here it is the only mode.

```
LaSourceDesc {                   // 96 bytes; all numeric, mod-assigned id
    id:           u64            // reuse the nav entity id to tie a sound to an object; 0 = anonymous (fired only)
    kind:         u8             // BUFFER | OSCILLATOR | NOISE
    waveform:     u8             // oscillator: SINE | TRIANGLE | SQUARE | SAW; noise: WHITE | PINK | BROWN
    priority:     u8             // voice stealing when over the voice cap
    _reserved0:   u8
    repeat_count: u16            // plays per trigger: 1 = once, N = N times, 0 = forever (LOOP)
    _reserved1:   u16
    buffer:       u32            // BUFFER kind
    bus:          u32
    flags:        u32            // PLAYING | RESTART | RELATIVE | AUTO_REMOVE | ...
    bounds:       LsBoundsDesc   // 40 bytes
    gain:         f32            // linear
    pitch:        f32            // BUFFER: playback rate; OSCILLATOR: frequency in Hz (phase-continuous)
    pan_width:    f32            // lateral crossover (units of the space); see spatializer
    repeat_gap_ms: f32           // silence between repeats (a sonar ping every 800 ms)
    _reserved2:   [u32; 2]
}
```

`repeat_count` replaces a separate loop flag: 0 loops forever, 1 is a one-shot, N plays N times with `repeat_gap_ms` between plays (0 = back to back). A **trigger** is what starts a play sequence: the record first arriving with `PLAYING` set, `PLAYING` going 0→1, or a `RESTART` pulse.

Flags:
- `PLAYING` — desired state, level-triggered; clearing it stops with a ramp. **A finished sequence stays finished while its record is unchanged** — re-sending the same record with `PLAYING` still set does *not* retrigger, which is what makes one-shots safe to leave in a per-frame snapshot. **Gain 0 with `PLAYING` set keeps a voice alive and silent** — wotr's mute-not-dispose, so a system muted during a cutscene resumes seamlessly.
- `RESTART` — edge-triggered: start the sequence again this frame even if already playing (a footstep every step). Cleared by the crate after acting.
- `RELATIVE` — position is relative to the listener; bypasses the renderer's geometry (UI sounds, breath, heartbeat, centered speech).
- `AUTO_REMOVE` — remove the source from the space when its sequence finishes, so upsert-style mods never have to clean up one-shots. Fired sources (below) always behave this way.

Per-source **effect chain** and **distance model** are set separately (§4) because they are lists, not fixed-size fields.

An `OSCILLATOR` source is the slope/elevation tone: frequency changes are phase-continuous (glissando), gain ramps ~20 ms, so pitch can track a cursor's height without clicks. A `NOISE` source is a continuous generator (white, pink or brown) for wind, water and static-style cues; its colour and the source chain do the shaping.

### Declaring the soundscape: two ownership models

**Declared sources** — the mod owns their lifetime, same contract as entities:

- **`la_space_sync(space, sources[], count, stats*)`** — declarative snapshot: upsert every record, stop and remove every *declared* source absent from the snapshot. The crate diffs numerically; an unchanged record costs nothing, a moved one updates its targets, a changed buffer restarts. A one-shot can sit in the snapshot indefinitely: it plays once on arrival and stays finished until its record changes (see `PLAYING`).
- **`la_source_upsert` / `la_source_remove`** — explicit deltas. With `AUTO_REMOVE` a one-shot removes itself when done.

**Fired sources** — the crate owns their lifetime:

- **`la_space_fire(space, desc*)`** — play a record's sequence (`repeat_count` times, `repeat_gap_ms` apart) and remove the source when it drains. Fired sources are invisible to `la_space_sync` (never removed by a snapshot that omits them) and are still full sources while alive: positioned from their bounds, re-spatialized as the listener moves, on their bus, with their chains. `id` may be 0 for anonymous fire-and-forget; a nonzero id allows `la_source_stop`, state polling and completion reporting, and firing again with the same id retriggers the existing voice instead of stacking a new one.

The common cases: a chest opening is `la_space_fire` with `repeat_count = 1`; a three-ping sonar reply is `la_space_fire` with `repeat_count = 3, repeat_gap_ms = 250`; a footstep is a declared `RELATIVE` source pulsed with `RESTART`; a beacon that pings every second while the object exists is a declared source with `repeat_count = 0, repeat_gap_ms = 1000`.

Every call encodes a command into the ring buffer; the audio thread applies a frame's commands together at the next period boundary.

### Reading back

No callbacks, so completion is polled: `la_source_state(space, id, out) → {playing, position_frames, plays_remaining}` and `la_space_finished(space, out_ids[], cap, written)` drains the ids (nonzero only) whose sequences ended since the last call. Fire-and-forget mods never call either.

---

## 4. Signal path and effect chains

### 4.1 The path

```
source (mono)
  → distance model                      gain from nearest-point distance
  → source chain (mono effects)         e.g. LOWPASS for occlusion — cheap, before stereo
  → RENDERER                            mono → bus format: the head model (pan, ITD, head
                                        shadow, rear shelf) or a plug-in such as Steam Audio
  → source post-chain (bus-format effects)  rare; e.g. a per-source EQ
  → bus gain + bus chain (stereo)
  → space gain + space chain (stereo)
  → device master gain + master chain   e.g. a LIMITER
  → output
```

`RELATIVE` sources skip the distance model and replace the spatializer with a plain constant-power pan from the record's `pan_width` field interpreted as a pan (−1..1). Oscillator sources enter the same path as a mono signal.

### 4.2 Chains are lists of nodes, as data

An effect chain is an ordered list of nodes, each a fixed-size record. Chains attach to a source (pre/post), a bus, a space or the device. Arbitrary routing graphs are out of scope for v1: **chains plus buses express everything wotr needed**, they cross the ABI as flat arrays, and they keep per-period cost predictable. Sends/returns (reverb) can be added later as a bus feature without changing the node model.

```
LaEffectDesc {                   // 40 bytes
    kind:     u32                // LaEffectKind; values >= 0x1000 are renderer-specific nodes
    flags:    u32                // ENABLED | node-specific cue toggles
    p:        [f32; 8]           // node-specific parameters (unused = 0)
}

la_source_set_chain(space, id, stage: PRE | POST, nodes[], n)
la_bus_set_chain(space, bus, nodes[], n)
la_space_set_chain(space, nodes[], n)
la_device_set_chain(device, nodes[], n)
la_*_set_effect_param(..., index, param, value)     // retune one parameter without resending the chain
la_*_set_effect_enabled(..., index, enabled)         // A/B by ear, crossfaded
```

### 4.3 Node kinds (v1)

| Kind | Parameters | Notes |
|---|---|---|
| `GAIN` | gain | Linear; ramped. |
| `LOWPASS` / `HIGHPASS` | corner_hz, q | RBJ biquad. The occlusion/muffling cut. |
| `LOWSHELF` / `HIGHSHELF` | corner_hz, gain_db, slope | RBJ biquad; **identity at 0 dB** so a "no cue" shelf is transparent. |
| `PEAK` | center_hz, gain_db, q | Parametric band. |
| `DELAY` | ms, feedback, mix | Fixed delay, tail drained on stop. |
| `LIMITER` | threshold_db, release_ms | Master safety; brickwall-ish. |
| `DISTANCE` | min, max, rolloff, model | The source's distance model (below); one per source, implicit if absent. |
| `SPATIALIZER` | see §4.4 | Parameters and cue flags of the head-model renderer for this source, overriding the space's defaults. |
| `RENDER_*` (≥ 0x1000) | renderer-defined | Renderer-specific per-source controls, e.g. Steam Audio's air absorption, transmission, directivity. Ignored by renderers that don't know them. |

### 4.4 The spatializer node (head model)

Ported from wotr's `Spatializer`, whose constants were tuned by ear with headphone users. Four independent perceptual channels, each a flag bit so they can be A/B'd:

- **Lateral pan with capped ILD** (`ILD` always on): constant-power pan whose interaural level difference is capped (default 12 dB) instead of driving the far ear to silence — a real head shadows by 8–20 dB, never −∞, and the far ear must keep signal for the time cue to exist. The lateral fraction is `|lat|^exponent` (default 0.82) for center expansion: lining a cursor up on a source by ear needs a sharp null at zero. **`pan_width`** is the lateral crossover: within it, pan tracks absolute sideways offset; beyond it, pure bearing.
- **ITD** (`ITD` flag): interaural time difference on the far ear, Woodworth spherical head, up to 0.66 ms, applied as a fractional delay from a tiny ring buffer. Below ~1.5 kHz this is the dominant localization cue and it externalizes the sound on headphones.
- **Far-ear head shadow** (`SHADOW` flag): a high-shelf cut on the far ear only, scaled by laterality (default corner 1.5 kHz, up to −8 dB). Reads as a head between two ears rather than a mixer pan.
- **Rear cue** (`REAR` flag): sources behind the listener get a high-shelf cut ramping to −10 dB (default corner 3 kHz) at due-behind — dark = behind, bright = ahead, the audiogame convention. A shelf, not a lowpass: bright narrowband cues would vanish under a lowpass, and a parallel dry/wet blend comb-filters.
- **Elevation tilt** (`ELEVATION` flag, Spatial mode only): a gentle high-shelf tilt with elevation (up = brighter). Off by default until evaluated.

Parameters (`p`): ild_cap_db, pan_exponent, itd_max_ms, shadow_corner_hz/shadow_max_db, rear_corner_hz/rear_max_db (packed as documented in the header). Space-level defaults apply to every source; a per-source `SPATIALIZER` node overrides them.

The mod exposes the flags as user settings if it wants (`audio.itd`, `audio.head_shadow`, `audio.front_back_filter` in wotr); the crate just honours the bits.

### 4.5 Distance model

`DISTANCE {min, max, rolloff, model}`: gain 1 inside `min`; between `min` and `max`, `INVERSE` (`(min/d)^rolloff`), `LINEAR`, or `REFERENCE` (`ref/(ref+d)`, wotr's speech/sonar curve, clamped to a floor); 0 beyond `max`; `max = 0` never attenuates. Distance is measured to the nearest point of the bounds; inside the bounds it is 0.

### 4.6 Parameter discipline (the no-click rules)

Every node keeps **target** and **current** values. Per period: gains, pans and delays ramp linearly across the block; biquad coefficients are retuned **once per block** toward their target (per-sample retuning is too costly, and state is preserved across retunes, so a moving shelf does not click). A node's `ENABLED` toggle crossfades dry/wet over a few ms rather than switching. New voices prime their current values to the targets on the first block (no ramp from silence) unless the source is starting from a stop. Delay lines drain their tail after a source ends before the voice is released.

### 4.7 Voices

Cap configurable at `la_device_open`, default 64. Over the cap, the quietest lowest-priority sources are stolen with a ramp-out. Looping sources at gain 0 still occupy a voice (that is the point of mute-not-dispose); mods with many silent loops should raise the cap or remove them.

### 4.8 Renderers and extensibility

The mono → bus-format stage is a **`Renderer` trait**, not a fixed node. It receives, per voice per period, the mono block and a `Placement` (nearest point in the listener's frame, distance, whether the listener is inside the bounds, the source's `pan_width`), and writes into the bus format. Implementations:

- **`HeadModel`** — the built-in §4.4 model. Always available; the default.
- **`SteamAudio`** (planned) — binaural HRTF via `iplBinauralEffectApply`, with `iplDirectEffect` for occlusion/transmission from the space's geometry and, optionally, reflections. Loaded **dynamically** (`phonon.dll` found at runtime; absent → the space silently falls back to `HeadModel`, logged once), so `loadstar.dll` never hard-depends on it. Steam Audio is Apache-2.0.
- Room for others (a future HRTF-from-SOFA renderer, an ambisonic renderer).

Contract every renderer must honour, designed around what Steam Audio needs:

1. **Fixed block size and sample rate at init** — `init(sample_rate, block_frames, bus_format)`. The mixer's period is the block size. On stream reopen (device loss, format change) renderers are re-initialised and per-voice state recreated.
2. **Real-time safe `process`**, allocation-free; all per-voice objects are created on the game thread when a source is upserted and moved to the audio thread through the command queue (Steam Audio effect creation is not RT-safe; application is).
3. **Optional `update(dt)`** on the game thread via `la_space_update`, plus an optional worker for heavy simulation; results are posted as targets like any other parameter, so the ramp discipline (§4.6) applies to occlusion and reflection changes too.
4. **Optional geometry** — `set_geometry(...)` is called with the space's triangles when present; renderers that don't use it ignore it.
5. **Bus format aware** — output channel count comes from the bus (v1: stereo everywhere; ambisonic buses later without changing this trait).
6. **Distance stays ours.** The `DISTANCE` node applies before the renderer; a renderer's own attenuation is disabled or its parameters routed through `RENDER_*` nodes, so switching renderers never changes loudness-vs-distance behaviour that a mod tuned.

Selection: `la_space_set_renderer(space, kind, config[])` and `la_source_set_renderer(space, id, kind)` for per-source overrides (a mod might render beacons binaurally but keep wall tones on the ear-fixed head model). `la_renderers_available(out[])` reports what loaded.

---

## 5. Crate layout and the backend trait

```
crates/loadstar-audio/           pure Rust: spaces, buses, sources, chains, DSP nodes, resampler
  src/dsp/                       biquads, delay lines, ramps, pan laws — pure functions
  src/render/mod.rs              trait Renderer (§4.8) + Placement
  src/render/head_model.rs       the built-in §4.4 renderer
  src/render/steam_audio.rs      (later) dynamic-loading phonon binding  (cfg(feature = "steam-audio"))
  src/mixer.rs                   period loop: sources → chains → renderer → buses → spaces → master
  src/backend/mod.rs             trait Backend { open(format, period) -> Stream; ... }
  src/backend/wasapi.rs          windows crate, IAudioClient3, MMCSS  (cfg(windows))
  src/backend/null.rs            silent backend with synthetic time, for tests and CI
crates/loadstar-ffi/             gains the `la_*` surface behind an `audio` cargo feature (one DLL)
bindings/csharp/Loadstar.Net/    Audio.cs: AudioDevice / AudioSpace / SourceDesc / EffectDesc wrappers,
                                 WavFile.Load(path) → f32 PCM convenience
```

Testing against the null backend with synthetic time: property tests for pan-law power constancy, ILD cap, ITD monotonicity, shelf identity at 0 dB, **ramp continuity** (no sample-to-sample jump above a threshold across any parameter change or toggle), voice stealing, sync diffing, and drain/tail behaviour. The WASAPI backend is exercised by the smoke harness — a positioned tone from the 32-bit .NET process, then inside CSTW.

The backend trait is minimal on purpose: open a stream at a format and period, hand the mixer a `&mut [f32]` per period, report the achieved period, and signal device loss. Everything above it is platform-free.

---

## 6. ABI conventions

Identical to `spec.md` §9: `la_` prefix, `LsStatus` return codes (shared enum, extended with `DeviceUnavailable`, `BufferInUse`), opaque handles, flat structs with pinned sizes, no callbacks, no strings, cdecl, `[Out]` on output arrays in .NET. Same lessons, same rules.

Threading contract: the device and its spaces are driven from one game thread; the audio thread is internal and never calls out.

---

## 7. What mods build on it (and how small it gets)

Each of these was a bespoke subsystem in wotr or CSTW; here they are configuration.

- **Wall tones** (wotr `WallToneSystem` + `NAudioEngine.WallTones`): four looping sources on a "walls" bus, `RELATIVE` with fixed pans (ahead centered, behind rear-shelved via a per-source `SPATIALIZER` override, right, left); each frame the mod traces its four directions in *world* space rotated by the facing (the nav grid's ray scan answers this for tile games) and sets each source's gain from a proximity curve (`t²`). A second bank on another bus for obstacles vs. walls is just four more sources. Muting during cutscenes is gain 0.
- **Object beacons / sonar**: one looping or periodically retriggered source per tracked entity, bounds = the entity's bounds, id = the entity's id, driven from the same snapshot the nav registry gets. Category → sound assignment (wotr's `ScanSounds` taxonomy) stays mod-side data.
- **Positioned, overlapping speech** (wotr `PositionalSpeech`): TTS PCM uploaded as a buffer, played as a one-shot source on a "speech" bus with a `REFERENCE` distance model; centered speech is `RELATIVE`. Many can play at once because they are just sources.
- **Slope/elevation tone** (wotr `ToneEngine`): one `OSCILLATOR` source, `pitch` = frequency tracking the cursor's height each frame, gain 0 when idle.
- **Footsteps** (CSTW `FootstepPatch`, wotr `TerrainSounds`): a click or terrain buffer on a `RELATIVE` source with `RESTART` set on each arrival; stop-on-key-release is clearing `PLAYING` (20 ms ramp, no click).
- **Interaction feedback** (chest opened, door unlocked, menu confirm): `la_space_fire` with a one-shot record — no bookkeeping, the source removes itself.
- **Compass ping**: a one-shot placed at world north through the rotated ear frame — the listener API does the rotation.
- **Menu/dialog cues**: `RELATIVE` one-shots on a "ui" bus, or a whole UI space.

Per-system volume sliders are bus gains; a master slider is the device gain; "occlusion" is a `LOWPASS` on the source pre-chain driven by the mod's own line-of-sight or reachability.

---

## 8. Roadmap

1. **Spec review** (this document).
2. **DSP + mixer against the null backend** — nodes, chains, buses, the head model, sync diffing, property tests including ramp continuity.
3. **WASAPI backend** — `IAudioClient3` + MMCSS, device-loss recovery, period reporting.
4. **FFI + .NET**, smoke harness plays a positioned, ITD/shadowed tone from x86 net472; then inside CSTW.
5. **CSTW footsteps and wall tones** on the crate; retire `WaveOutToneOutput` and the inlined `ToneGenerator`.
6. **Steam Audio renderer**: dynamic loading, binaural + direct effect, geometry export from tile contexts, evaluated against the head model with blind players.
7. Later: sends/reverb, ambisonic buses, Linux backend, streaming.

## 9. Decisions (review of draft 2)

- **Routing**: linear chains on four attachment points plus buses. No general graph; sends/returns are the extension path if reverb-style routing is ever needed.
- **Head-model defaults**: wotr's ear-tuned constants are the crate defaults — 12 dB ILD cap, 0.82 pan exponent, 0.66 ms max ITD, head shadow 1.5 kHz / −8 dB, rear cue 3 kHz / −10 dB — and every one of them is a parameter of the `SPATIALIZER` node, settable per space and per source.
- **Distance units**: a space declares `unit_scale` (units per metre) at creation; `DISTANCE` defaults and the head model's `pan_width` default are expressed in metres and scaled, so a tile game and a metric 3D game get sensible behaviour without per-source tuning.
- **Voice cap**: configurable at `la_device_open`; **default 64**. Mute-not-dispose keeps voices alive, and busier games will want more.
- **Packaging**: the `la_*` surface ships inside `loadstar.dll` behind the `audio` cargo feature (on by default). One native DLL.
- **Buffer ownership**: **upload-and-copy.** It is also the faster choice in practice: the copy is one-time and the crate resamples and de-interleaves into its own mixer-ready layout at upload, so the per-period path reads contiguous, already-converted samples with no rate conversion, no pinning and no lifetime hazards across the ABI. Mod-owned memory would save a one-time copy and cost every period.
- **Noise source**: included in v1 as `NOISE` kind with `waveform` ∈ {WHITE, PINK, BROWN}; gain and the source chain shape it (a `LOWPASS` on pink noise is wind; brown noise under a river prism is water). Cheap to add, useful immediately.
