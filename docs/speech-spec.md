# Loadstar Speech — Design Specification

*Companion to `spec.md` and `audio-spec.md`. Status: draft 1, for review. Pre-implementation.*

*Distilled from the Wrath of the Righteous mod's speech layer (`wotr-access/src/Speech`, ~2,000 lines of C# that went through two refactors before it settled) and Say the Spire 2's, which it was ported from. Those are the validated behaviours; this document turns them into a crate so a new mod has them on day one.*

---

## 1. Purpose and scope

`loadstar-speech` speaks text for a blind player: through their screen reader when one is running, through a synthesizer when not, and — with the audio crate — from a position in the game world. It exists because every accessibility mod rebuilds the same stack: detect the screen reader, fall back to SAPI, let the user pick a voice and a rate, let *some* things speak in a *different* voice, never interrupt by default, and render speech to PCM so it can be placed in the soundscape. That stack is policy that is the same in every game, which is exactly what the admission rule (spec §1) asks for: wotr and Say the Spire 2 both need it, identically.

It follows the crate's rules — tools not a framework, no callbacks across the ABI, no update loop, the mod decides *what* is said — with **one deliberate exception, text crosses the ABI** (§9). Nothing else about the FFI conventions changes.

### In scope (v1)

- **Engines**: the things that can speak. Screen readers and system synthesizers, each described by a feature set the mod can query. Prism is loaded at runtime as an engine and contributes every screen reader it knows (§4). Native engines ship inside `loadstar.dll` (§5).
- **Profiles**: a named way to speak — an engine choice plus voice, rate, pitch and volume — that the mod builds from its own settings. One default profile drives all UI speech; any number of additional profiles let events speak in another voice.
- **Speak, output, braille, stop**, with explicit interrupt semantics (§3).
- **Render to PCM** on engines that can, as a polled job (§6), and **speak-at**: render, upload and fire a positioned source in one call when the audio feature is on.
- **Voice enumeration** (names, languages) through the two-call buffer pattern.

### Out of scope (v1)

- **Persistence of profiles.** The mod's settings own the values (spec §2.5, "numbers only"); the crate holds live runtime objects rebuilt at load.
- **Text formatting, localization, pronunciation dictionaries, SSML authoring.** Text arrives ready to speak. (Engines that accept SSML are flagged, and an SSML flag on the speak call passes it through untouched; the crate never composes it.)
- **A speech queue of its own with priorities and categories.** Queueing is a mod policy; v1 gives one channel per profile with speak/interrupt semantics and leaves "what wins" to the caller. If two mods converge on the same queue, it enters later (admission rule).
- **Compiling Prism in.** Prism is CMake/C++ with fetched dependencies; Loadstar builds with cargo alone. Prism is dynamically loaded (§4).
- **Linux and macOS engines** beyond what Prism provides at runtime. The engine trait exists so speech-dispatcher and AVSpeech can be added natively when a consumer needs them.

---

## 2. Concepts

### Context

One per process: `lv_create(config) → handle`. Config: the Prism search policy (§4.3) and a worker-thread flag. Creation is cheap and never speaks; engines are probed lazily on first use because screen-reader detection and SAPI voice enumeration are slow on some machines and must never stall a game's boot.

### Engines

An **engine** is a source of speech with a stable 64-bit id and a feature bitset. Ids are derived from a name hash so a mod can persist "the user chose NVDA" as a number (spec §2.5). Engines are discovered by `lv_engine_count / lv_engine_id_at / lv_engine_name / lv_engine_features`. The set is:

- Every Prism backend reported available at runtime (`PRISM_BACKEND_IS_SUPPORTED_AT_RUNTIME`), under its Prism name and Prism's own 64-bit backend id (so `PRISM_BACKEND_NVDA` is the same number in both libraries).
- The native engines of §5.
- Two virtual engines: **`auto`** (the best screen reader if one is running, else the best synthesizer — Prism's `registry_create_best` when Prism is present, the native order otherwise) and **`clipboard`** (copies the text; the last resort, always available on Windows).

Features (one bitset, shared vocabulary with Prism's so they map 1:1): `SPEAK`, `OUTPUT` (speech plus braille where the reader supports it), `BRAILLE`, `STOP`, `PAUSE`, `IS_SPEAKING`, `SET_RATE`, `SET_PITCH`, `SET_VOLUME`, `VOICES` (enumerable and selectable), `RENDER` (speak-to-memory), `SSML`, `SCREEN_READER` (the output is the user's reader, not a synthesizer — the UI usually lists these first and names them by reader).

### Profiles

A **profile** is a handle: `lv_profile_create(ctx, engine_id) → profile`, then `lv_profile_set(profile, param, value)` for `RATE`, `PITCH`, `VOLUME` (floats 0..1, normalized the Prism way) and `VOICE` (index into the engine's voice list), and `lv_profile_set_flag` for `INTERRUPT_DEFAULT` (off by default — the SayTheSpire rule) and `TRIM_SILENCE`. A profile owns one live engine instance, so its voice and rate are applied once and stay applied: switching profiles between utterances costs nothing, which is what lets events speak enemy damage in one voice and ally damage in another without re-selecting a SAPI voice per call. Two profiles on the same engine are two instances where the engine allows it (SAPI, OneCore) and a shared one with per-call parameter restore where it does not (a screen reader is one process).

A profile whose engine is unavailable on this machine (`auto` resolved to nothing, or the user's saved reader isn't running) **falls back** along a fixed order — the user's choice, then `auto`, then the first native synthesizer, then `clipboard` — and reports which engine it actually resolved to (`lv_profile_resolved(profile)`), so the settings UI can say "NVDA (not running — using SAPI)". This is wotr's `SpeechConfig.Handler` resolution, made a crate guarantee.

### Channels and utterances

Each profile is a **channel**: speaking on it queues behind what that profile is already saying; `interrupt` on the call (or the profile's default) cuts that channel only. Two profiles speak over each other — that is the point of them. `lv_stop(profile)` silences one channel, `lv_stop_all(ctx)` every channel including Prism's. **Screen readers are one channel regardless of profiles**: interrupting NVDA interrupts NVDA, and the crate does not pretend otherwise (§3).

An **utterance** is one speak call. v1 returns no handle for live speech (readers give none back); `lv_is_speaking(profile)` is the only state. Render jobs (§6) do return ids.

---

## 3. Speaking

```
lv_speak   (profile, text, len, flags) → LsStatus      // voice only
lv_output  (profile, text, len, flags) → LsStatus      // voice + braille where the engine supports both
lv_braille (profile, text, len)        → LsStatus      // braille only
lv_stop    (profile) / lv_stop_all(ctx)
lv_is_speaking(profile, out bool)
```

Flags: `INTERRUPT` (cut this channel first; absent = queue, unless the profile's default says otherwise), `SSML` (text is SSML; rejected with `InvalidArgument` on engines without the feature), `NO_FALLBACK` (fail rather than speak through a fallback engine — for cues that only make sense on the chosen output).

Rules, all learned by ear in the mods:

- **Never interrupt by default.** Announcements queue; the caller opts into cutting. Dialogue in wotr never interrupts; a cursor readout does.
- **Speaking never blocks the game thread.** Screen readers and SAPI/OneCore all accept text asynchronously; where an engine would block (some Prism backends synthesize before returning), the crate hands the call to its worker. The ABI call returns when the text has been *accepted*, not spoken.
- **Text is UTF-8 with an explicit length**, never NUL-terminated by contract (the mod's strings aren't). Invalid UTF-8 is `InvalidArgument`; the crate does not repair text.
- **Empty text is a no-op**, not an error. (The mods' `Tap` observer pattern — a debug tap of everything spoken — is a companion concern, not the crate's.)

---

## 4. Prism as an engine

Prism (ethindp/prism, MPL-2.0) unifies the screen-reader abstraction libraries and keeps gaining obscure readers — nineteen backend ids at 0.18.3, from NVDA and JAWS to PC-Talker and SenseReader. That breadth is the reason to depend on it, and the pace of change is the reason not to compile it in.

### 4.1 Loaded, not linked

The crate binds Prism's C header and loads `prism.dll` (`libprism.so`, `libprism.dylib`) **at runtime with `libloading`**, resolving **every symbol individually by name**. If the library is absent, the Prism engines simply don't exist. If a symbol is absent, the feature that needs it is masked off for that session. Nothing links at build time; cargo alone builds Loadstar.

Prism's `speak_to_memory` delivers audio through a **callback**. That callback lives entirely inside the crate — Prism calls Loadstar, Loadstar appends to a job buffer — and never crosses Loadstar's own ABI. The no-callbacks rule applies to Loadstar's surface, not to what it does privately with a library it loaded.

### 4.2 Why dynamic beats static here

- **New readers for free.** Backends come from Prism's registry at runtime; dropping in a newer `prism.dll` exposes a newer reader to every Loadstar mod with no rebuild of Loadstar or the mod.
- **No toolchain coupling.** Prism needs CMake and fetches dependencies; a Loadstar contributor needs `cargo build`.
- **Partial failure instead of total.** A missing library degrades to native engines; a static link that fails to resolve fails the whole dll at load.

### 4.3 Pinning and tracking

- Loadstar carries **Prism's header as a git submodule pinned to a release tag** (`external/prism`), and generates the Rust declarations from that header at build time (`bindgen`, behind the default-on `prism` feature; the generated file is checked in so a build without LLVM still works). A signature change in a newer header fails the build at generation time, loudly, instead of corrupting a call at runtime.
- `tools/fetch-prism.ps1 / .sh` downloads the pinned tag's release zips (Prism publishes per-platform binaries for every tag: Windows x64/ARM64, Linux, macOS, mobile) into the companion's distribution directory, so a mod that ships Loadstar can ship the matching Prism beside it.
- **Bumping Prism** is: move the submodule, regenerate, run the smoke harness, refetch binaries. The crate's version note records which Prism tag it was validated against; `lv_prism_version` reports what was loaded (from the library's own version export when Prism adds one; the file version resource until then).
- **Search order** for the library: a path passed in `lv_create`'s config, then beside `loadstar.dll`, then the process directory, then the platform loader's default search. The config can also say *never load Prism* — a mod that wants deterministic behaviour under test.

### 4.4 Prism's contract, as Loadstar relies on it

Only these are load-bearing; everything else is optional and feature-masked: `prism_config_init`, `prism_init`, `prism_shutdown`, the registry enumeration (`count`, `id_at`, `name`, `get_features`, `create`, `create_best`), `backend_initialize`, `backend_free`, `backend_get_features`, `backend_speak`, `backend_output`, `backend_braille`, `backend_stop`. Speak-to-memory, pause/resume, rate/pitch/volume and the voice functions are consulted only when their feature bit is set.

The mod's experience with Prism's own SAPI backend (slow, superseded by a direct COM path) is encoded as policy: **when a native engine covers the same synthesizer as a Prism backend, the native one is listed and Prism's is hidden** (`PRISM_BACKEND_SAPI`, and `PRISM_BACKEND_ONE_CORE` once §5.2 lands). Screen readers always come from Prism.

### 4.5 The 32-bit gap

Prism publishes no 32-bit Windows build, and Say the Spire 2 / CSTW are 32-bit processes. On `i686` the Prism engine is compiled in (same loader) but will find no library unless someone builds one from source; `tools/fetch-prism` has an opt-in `-Build32` step for a machine with CMake. The native engines (§5) carry 32-bit mods until then, which is the status quo those mods already live with.

---

## 5. Native engines

Inside `loadstar.dll`, no external files, each behind a feature of its own so a Linux build drops the Windows ones:

### 5.1 SAPI 5 (Windows)

COM via the `windows` crate (already a dependency of the audio crate): `ISpVoice` per profile instance, voice enumeration through the token category, rate/volume mapped from the normalized 0..1 to SAPI's −10..10 and 0..100. Speaks asynchronously (`SPF_ASYNC`), interrupts with `SPF_PURGEBEFORESPEAK`. **Render to memory** uses a *second*, independent voice bound to a memory stream so rendering never cuts, or is cut by, live speech — the mod's `SapiHandler` rule. This is the engine wotr ships today as "manual COM SAPI" and the one that proved Prism's SAPI path too slow.

### 5.2 OneCore (Windows 10+)

`Windows.Media.SpeechSynthesis` via WinRT, for the modern voices and the cleanest render path (`SynthesizeTextToStreamAsync` gives PCM directly, with SSML support). *Validation item*: WinRT activation from Rust on the 32-bit target.

### 5.3 NVDA controller client (Windows)

`nvdaControllerClient64.dll` / `32.dll` loaded dynamically: `nvdaController_testIfRunning`, `speakText`, `brailleMessage`, `cancelSpeech`. Four functions, and it covers the single most common reader on 32-bit hosts where Prism is absent. Hidden when Prism's NVDA backend is available (one entry per reader in the list).

### 5.4 Clipboard (Windows)

Copies the text. Exists so "something happens" when nothing else can; the fallback chain's last link.

---

## 6. Rendering and the audio crate

```
lv_render(profile, text, len, flags, out job)      → LsStatus   // starts a worker job
lv_render_poll(ctx, job, out state)                → {Pending | Done | Failed}
lv_render_take(ctx, job, out samples*, out frames, out channels, out rate)   // library-owned, valid until lv_render_free
lv_render_free(ctx, job)
```

Rendering is blocking in every engine, so it runs on the crate's worker thread and is **polled, never called back** — the same shape as `la_device_finished`. The result is f32 PCM at the engine's native rate; the caller (or the convenience below) resamples by handing it to the audio crate, which already resamples on upload.

With the `audio` feature on, the two crates meet in one call:

```
lv_speak_at(profile, text, len, flags, device, space, bus, bounds*, gain, out source_id)
```

Render → `la_buffer_create` → `la_space_fire` a one-shot source with the given bounds on the given bus, `AUTO_REMOVE`, buffer freed when the source finishes. The source id comes back immediately (the source exists, silent, until the render lands — a few hundred milliseconds for SAPI), so the mod can stop or reposition it like any other. This is wotr's `PositionalSpeech` plus `SpatialSources` plus the render path collapsed into a line, and it is why the speech crate belongs next to the audio crate rather than in a companion.

Profiles on screen readers cannot render (`RENDER` unset); `lv_speak_at` on such a profile returns `InvalidArgument` unless `ALLOW_RENDER_FALLBACK` is set, in which case the crate renders through the best native synthesizer with the profile's rate and volume — the mod's `RenderToAudioFallback`.

---

## 7. ABI surface (summary)

Prefix `lv_` (voice). `LsStatus` return codes, extended with `EngineUnavailable` (the chosen engine is absent/not running and `NO_FALLBACK` was set) and `NotSupported` (feature bit unset). Opaque `LvContext*`, `LvProfile*`; ids and jobs are integers. cdecl, `_Bool` as one byte, `[Out]` on .NET output arrays — the standing rules.

| Group | Functions |
|---|---|
| Lifecycle | `lv_create`, `lv_destroy`, `lv_prism_version`, `lv_abi_version` (shared with `ls_abi_version`) |
| Engines | `lv_engine_count`, `lv_engine_id_at`, `lv_engine_name(id, buf, cap, out len)`, `lv_engine_features(id, out u64)`, `lv_engine_refresh` (re-probe readers that started after boot) |
| Voices | `lv_voice_count(engine)`, `lv_voice_name(engine, i, buf, cap, out len)`, `lv_voice_language(engine, i, buf, cap, out len)` |
| Profiles | `lv_profile_create`, `lv_profile_destroy`, `lv_profile_set(param, f32)`, `lv_profile_set_flag(flag, bool)`, `lv_profile_set_voice(index)`, `lv_profile_resolved(out engine id)` |
| Speaking | `lv_speak`, `lv_output`, `lv_braille`, `lv_stop`, `lv_stop_all`, `lv_is_speaking`, `lv_pause`, `lv_resume` |
| Rendering | `lv_render`, `lv_render_poll`, `lv_render_take`, `lv_render_free`, `lv_speak_at` (audio feature) |

Strings **in**: `(const uint8_t* utf8, size_t len)`. Strings **out**: the two-call buffer pattern, UTF-8, length excludes any terminator. No other string crosses.

---

## 8. Threading

The context is driven from one game thread. The crate owns one **worker thread** for render jobs and for engines whose speak call would block; the worker never calls the host. Prism's audio callback runs on whatever thread Prism chooses and only appends to a job buffer under a lock the game thread never takes. COM is initialized on the worker (MTA) and on the calling thread if it isn't already (STA is fine for SAPI); the crate never uninitializes COM it did not initialize.

---

## 9. Spec amendments this requires

Two lines in `spec.md` move:

- **Non-goals, "Speech output."** → "Speech output *lives in `loadstar-speech`* (`speech-spec.md`), admitted 2026-10 on the audio crate's precedent: two consumers (wotr, Say the Spire 2) with the same stack."
- **Non-goals, "Strings…"** and **FFI conventions, "No strings anywhere in the ABI."** → unchanged for the core and audio surfaces. The speech surface is the single, explicit exception: text to be spoken goes in, voice and engine names come out, by the conventions in §7. The core still emits numbers only; labels still never enter it.

`§10 Companion layers` drops "Speech helper (Tolk binding)" — the companion's speech helper becomes a thin wrapper over `lv_*`.

---

## 10. What mods build on it

- **wotr's whole `src/Speech`** — `SpeechManager`, `SpeechConfig`, the three handlers, `SpeechOutputs`, the render fallback — becomes: profiles built from the Speech settings tree, `lv_speak`/`lv_output` behind the `Tts` facade, `lv_speak_at` for positional events. The settings UI keeps its "outputs" model (Auto, each reader by name, SAPI, Clipboard) because that is exactly the engine list with `SCREEN_READER` entries first.
- **Say the Spire 2** gets the same by construction, plus positioned speech it never had.
- **A new mod** calls `lv_create`, makes one profile on `auto`, and speaks. Voice settings are a dropdown over `lv_voice_name`. Nothing else to write.

---

## 11. Roadmap

1. **This document reviewed.**
2. **Crate skeleton + engine trait + profiles + `clipboard`**, with the Prism loader and generated bindings against the pinned header; smoke harness speaks through NVDA via Prism from x64 net472.
3. **SAPI 5 native** (speak + render), the fallback chain, `lv_speak_at` against the audio crate; wotr's `SapiHandler` and `PositionalSpeech` retired behind it.
4. **NVDA direct + the 32-bit validation**; Say the Spire 2 moves over.
5. **OneCore**, SSML passthrough.
6. Later: speech-dispatcher and AVSpeech natively; a shared queue if two mods ask for the same one.

## 12. Open questions

- Should `auto` prefer a running screen reader over a synthesizer even when the profile sets a voice the reader can't honour? (wotr: yes, the reader always wins; the voice params apply only to synthesizers.)
- Prism's `output` on readers that lack braille: speak only, or `NotSupported`? (Proposed: speak only — `OUTPUT` means "the user's preferred modality set", not "both or fail".)
- Whether the crate should expose Prism's `pause`/`resume` at all; no consumer uses them today.
