# Loadstar Speech — Design Specification

*Companion to `spec.md` and `audio-spec.md`. Status: draft 2, for review. Pre-implementation.*

*Distilled from the Wrath of the Righteous mod's speech layer (`wotr-access/src/Speech`, ~2,000 lines of C# that went through two refactors before it settled) and Say the Spire 2's, which it was ported from. Those are the validated behaviours; this document turns them into a crate so a new mod has them on day one.*

*Draft 2 reshapes draft 1 around two decisions: a profile is pure configuration the mod owns, and the queue is the stateful object the library owns. The queue was a stated non-goal in draft 1; it is admitted on the spec's own rule — several mods have hand-rolled the same one and fixed the same bugs in it.*

---

## 1. Purpose and scope

`loadstar-speech` speaks text for a blind player: through their screen reader when one is running, through a synthesizer when not, and — with the audio crate — from a position in the game world. It exists because every accessibility mod rebuilds the same stack: detect the screen reader, fall back to SAPI, let the user pick a voice and a rate, let *some* things speak in a *different* voice, queue announcements so they don't trample each other, never interrupt by default, and render speech to PCM so it can be placed in the soundscape. That stack is policy that is the same in every game, which is exactly what the admission rule (spec §1) asks for: wotr and Say the Spire 2 both need it, identically.

It follows the crate's rules — tools not a framework, no callbacks across the ABI, no update loop, the mod decides *what* is said — with **one deliberate exception, text crosses the ABI** (§9). Nothing else about the FFI conventions changes.

### In scope (v1)

- **Engines**: the things that can speak. Screen readers and system synthesizers, each described by a feature set the mod can query. Prism is loaded at runtime as an engine and contributes every screen reader it knows (§4). Native engines ship inside `loadstar.dll` (§5).
- **Profiles**: a flat description of *how* to speak — engine, voice, rate, pitch, volume — that the mod builds from its own settings and passes with each utterance. No handle, no lifetime.
- **Queues**: ordered lanes the library owns. Each utterance goes on a queue; the queue decides when it starts, what interrupts what, and how many may overlap.
- **Speak, output, braille, stop, flush, pause**, with explicit interrupt semantics (§3) and polled **utterance ids** in place of callbacks.
- **Render to PCM** on engines that can, as a polled job (§6), and **positioned speech**: render, upload and fire a positioned source as one queued utterance when the audio feature is on.
- **Voice enumeration** (ids, names, languages) through the two-call buffer pattern.

### Out of scope (v1)

- **Persistence of profiles and queue settings.** The mod's settings own the values (spec §2.5, "numbers only"); a profile is already a flat struct of numbers, so there is nothing to persist on the library side.
- **Text formatting, localization, pronunciation dictionaries, SSML authoring.** Text arrives ready to speak. (Engines that accept SSML are flagged, and an SSML flag on the speak call passes it through untouched; the crate never composes it.)
- **Priorities and categories.** A queue orders by submission; the two interrupt flags (§3) are the only reordering. "Events outrank ambience" is expressed by putting them on different queues, not by numbers on utterances. If two mods converge on a priority scheme it enters later.
- **Compiling Prism in.** Prism is CMake/C++ with fetched dependencies; Loadstar builds with cargo alone. Prism is dynamically loaded (§4).
- **Linux and macOS engines** beyond what Prism provides at runtime. The engine trait exists so speech-dispatcher and AVSpeech can be added natively when a consumer needs them.

---

## 2. Concepts

### Context

One per process: `lv_create(config) → handle`. Config: the Prism search policy (§4.3). Creation is cheap and never speaks; engines are probed lazily on first use because screen-reader detection and SAPI voice enumeration are slow on some machines and must never stall a game's boot. The context owns one worker thread (§8) and the default queue.

### Engines

An **engine** is a source of speech with a stable 64-bit id and a feature bitset. Ids are derived from a name hash so a mod can persist "the user chose NVDA" as a number (spec §2.5). Engines are discovered by `lv_engine_count / lv_engine_id_at / lv_engine_name / lv_engine_features`. The set is:

- Every Prism backend reported available at runtime (`PRISM_BACKEND_IS_SUPPORTED_AT_RUNTIME`), under its Prism name and Prism's own 64-bit backend id (so `PRISM_BACKEND_NVDA` is the same number in both libraries).
- The native engines of §5.
- Two virtual engines: **`auto`** (the best screen reader if one is running, else the best synthesizer — Prism's `registry_create_best` when Prism is present, the native order otherwise) and **`clipboard`** (copies the text; the last resort, always available on Windows).

Features (one bitset, shared vocabulary with Prism's so they map 1:1): `SPEAK`, `OUTPUT` (speech plus braille where the reader supports it), `BRAILLE`, `STOP`, `PAUSE`, `IS_SPEAKING`, `SET_RATE`, `SET_PITCH`, `SET_VOLUME`, `VOICES` (enumerable and selectable), `RENDER` (speak-to-memory), `SSML`, `MULTI_INSTANCE` (more than one utterance can be in flight on this engine at once — synthesizers yes, screen readers no), `SCREEN_READER` (the output is the user's reader, not a synthesizer — the UI usually lists these first and names them by reader).

### Voices

Each `VOICES` engine enumerates voices with a stable 64-bit **voice id** (hash of the engine's own voice identifier — the SAPI token id, the OneCore voice id, a Prism voice name), a display name and a language tag. Profiles refer to voices by id, never by index, so a saved profile survives a reinstall that reorders the list. A voice id the engine no longer has resolves to the engine's default voice and `lv_resolve` reports it (below).

### Profiles

A **profile** is a plain struct, owned by the mod, passed by pointer on every speak call:

```c
typedef struct LvProfile {
    uint64_t engine;    // engine id, or LV_ENGINE_AUTO
    uint64_t voice;     // voice id, or 0 for the engine's default
    float    rate;      // 0..1, 0.5 = the engine's default
    float    pitch;     // 0..1, 0.5 = default
    float    volume;    // 0..1
    uint32_t flags;     // NO_FALLBACK, RENDER_FALLBACK, TRIM_SILENCE (render only)
} LvProfile;
```

It maps one to one onto a mod's speech-settings record and is flat data under the FFI rules. The library keeps an internal cache of engine instances keyed on `(engine, voice)` — voice selection is the slow part of SAPI and OneCore; rate, pitch and volume are cheap and are applied per utterance — so two utterances a frame apart in different voices cost nothing extra. This is what lets an events system speak enemy damage in one voice and ally damage in another without the mod managing voice objects.

A profile whose engine is unavailable on this machine (`auto` resolved to nothing, or the user's saved reader isn't running) **falls back** along a fixed order — the chosen engine, then `auto`, then the first native synthesizer, then `clipboard` — unless `NO_FALLBACK` is set, in which case the call fails with `EngineUnavailable`. `lv_resolve(ctx, profile, out engine, out voice)` answers "what would this profile actually use right now" without speaking, so the settings UI can say "NVDA (not running — using SAPI)". This is wotr's `SpeechConfig.Handler` resolution, made a crate guarantee.

### Queues

A **queue** is an ordered lane the library owns: `lv_queue_create(ctx, options) → queue`, `lv_queue_destroy`. Queue 0 is the default and always exists. Options:

- **`max_concurrent`**: how many of this queue's utterances may be in flight at once. `1` = strictly sequential (UI, dialogue). `0` = unlimited (positional events, where two enemies struck in the same frame *should* overlap). Honoured only on `MULTI_INSTANCE` engines; a screen reader is one physical channel and is forced to 1 whatever the queue says.
- **`gap_ms`**: silence inserted between consecutive utterances on a sequential queue. Mods set 0..300; it replaces every hand-rolled "don't let announcements run together" timer.

Each utterance carries its own profile, so one queue may alternate engines and voices. The queue is a sequencer, not a voice.

### Utterances

`lv_speak` returns a **utterance id**, a per-context monotonically increasing `uint64_t`. `lv_utterance_state(ctx, id) → {Pending, Speaking, Done, Cancelled, Unknown}` is the whole feedback channel — no callbacks (spec §9). It covers the SAPI-bookmark use cases ("advance the cutscene when this line finishes", "don't start the next readout until this one began") without any of the callback machinery. Ids are never reused; querying one the context has forgotten returns `Unknown`.

**Where state is honest.** On synthesizer engines the library drives the instance and knows the exact state. On a screen reader the library knows Pending (not yet handed over) and, where the backend exposes `IS_SPEAKING`, Speaking and Done; where it does not, the utterance reports `Done` the moment it is handed to the reader. That degradation is deliberate and documented rather than papered over — a mod that needs end-of-speech timing on NVDA cannot have it from any library, and should know.

---

## 3. Speaking

```
lv_speak   (ctx, queue, profile*, text, len, flags, out utterance) → LsStatus   // voice only
lv_output  (ctx, queue, profile*, text, len, flags, out utterance) → LsStatus   // voice + braille where supported
lv_braille (ctx, queue, profile*, text, len,        out utterance) → LsStatus   // braille only
lv_gap     (ctx, queue, ms,                         out utterance) → LsStatus   // silence as a queue item
lv_stop    (ctx, queue)        // cut what is speaking on this queue, keep the rest
lv_flush   (ctx, queue)        // drop everything pending on this queue, keep what is speaking
lv_pause   (ctx, queue) / lv_resume(ctx, queue)
lv_cancel  (ctx, utterance)    // one item: cut if speaking, drop if pending
lv_stop_all(ctx)               // every queue, every engine, including Prism's
```

Flags on a speak call:

- **`INTERRUPT`** — stop what this queue is speaking, drop what it has pending, then speak this. The hard cut: a cursor readout that supersedes the previous one.
- **`FRONT`** — go to the head of this queue without cutting anything. The soft cut: a warning that should come next but not clip the word in progress.
- Neither — append. **This is the default, and it is the SayTheSpire rule**: nothing interrupts unless the caller says so. Dialogue in wotr never interrupts; a cursor readout does. Interrupt is a property of the *call*, never of the profile or the queue.
- **`SSML`** — text is SSML; `NotSupported` on engines without the feature.
- **`BRAILLE_TOO`** (on `lv_speak`) — same as `lv_output`; kept so bindings with a single speak method can express both.

Rules, all learned by ear in the mods:

- **Speaking never blocks the game thread.** The ABI call hands the utterance to the queue and returns; the worker (§8) talks to engines. Even where an engine's own speak call would block (some Prism backends synthesize before returning), the game thread never sees it.
- **Text is UTF-8 with an explicit length**, never NUL-terminated by contract (the mod's strings aren't). Invalid UTF-8 is `InvalidArgument`; the crate does not repair text.
- **Empty text is a no-op** that returns a valid, already-`Done` utterance id, not an error.
- **Screen readers are one channel across all queues.** Utterances from different queues reach the reader in submission order. `lv_stop` on one queue cuts the reader — which may be speaking another queue's text — because there is nothing else it can cut; it then drops only its own pending items. The spec states this rather than pretending queues are isolated on a reader.
- **Commands, not calls, inside.** Every verb above becomes a command on the queue's inbox, consumed by the worker. That is an implementation statement with user-visible consequences: ordering between a `speak` and a `stop` issued on the same frame is the call order, a debug tap can log and replay the command stream, and a future `lv_submit(queue, commands[], n)` for atomic batches ("interrupt, then A, then B" with no race) costs nothing to add. It is not in v1 because no mod has hit the race yet.

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

Only these are load-bearing; everything else is optional and feature-masked: `prism_config_init`, `prism_init`, `prism_shutdown`, the registry enumeration (`count`, `id_at`, `name`, `get_features`, `create`, `create_best`), `backend_initialize`, `backend_free`, `backend_get_features`, `backend_speak`, `backend_output`, `backend_braille`, `backend_stop`. Speak-to-memory, pause/resume, is-speaking, rate/pitch/volume and the voice functions are consulted only when their feature bit is set.

The mod's experience with Prism's own SAPI backend (slow, superseded by a direct COM path) is encoded as policy: **when a native engine covers the same synthesizer as a Prism backend, the native one is listed and Prism's is hidden** (`PRISM_BACKEND_SAPI`, and `PRISM_BACKEND_ONE_CORE` once §5.2 lands). Screen readers always come from Prism when it is present.

### 4.5 The 32-bit gap

Prism publishes no 32-bit Windows build, and Say the Spire 2 / CSTW are 32-bit processes. On `i686` the Prism engine is compiled in (same loader) but will find no library unless someone builds one from source; `tools/fetch-prism` has an opt-in `-Build32` step for a machine with CMake. The native engines (§5) carry 32-bit mods until then, which is the status quo those mods already live with.

---

## 5. Native engines

Inside `loadstar.dll`, no external files, each behind a feature of its own so a Linux build drops the Windows ones:

### 5.1 SAPI 5 (Windows)

COM via the `windows` crate (already a dependency of the audio crate): one `ISpVoice` per `(voice)` cache entry, and additional instances created on demand when a queue's `max_concurrent` asks for overlap, so overlapping utterances are real concurrent voices rather than one purged voice. Voice enumeration through the token category; rate/volume mapped from the normalized 0..1 to SAPI's −10..10 and 0..100. Speaks asynchronously (`SPF_ASYNC`), cuts with `SPF_PURGEBEFORESPEAK` on the instance that owns the utterance. **Render to memory** uses instances bound to a memory stream, never one that is speaking live — the mod's `SapiHandler` rule, now a property of the instance cache rather than a special case. This is the engine wotr ships today as "manual COM SAPI" and the one that proved Prism's SAPI path too slow.

### 5.2 OneCore (Windows 10+)

`Windows.Media.SpeechSynthesis` via WinRT, for the modern voices and the cleanest render path (`SynthesizeTextToStreamAsync` gives PCM directly, with SSML support). *Validation item*: WinRT activation from Rust on the 32-bit target.

### 5.3 NVDA controller client (Windows)

`nvdaControllerClient64.dll` / `32.dll` loaded dynamically: `nvdaController_testIfRunning`, `speakText`, `brailleMessage`, `cancelSpeech`. Four functions, and it covers the single most common reader on 32-bit hosts where Prism is absent. Hidden when Prism's NVDA backend is available (one entry per reader in the list). No `IS_SPEAKING`, so utterances on it report `Done` at hand-over (§2).

### 5.4 Clipboard (Windows)

Copies the text. Exists so "something happens" when nothing else can; the fallback chain's last link.

---

## 6. Rendering and the audio crate

```
lv_render(ctx, profile*, text, len, flags, out job)                       → LsStatus   // worker job, not queued
lv_render_poll(ctx, job, out state)                                       → {Pending | Done | Failed}
lv_render_take(ctx, job, out samples*, out frames, out channels, out rate)             // library-owned until lv_render_free
lv_render_free(ctx, job)
```

Rendering is blocking in every engine, so it runs on the worker and is **polled, never called back** — the same shape as `la_device_finished`. The result is f32 PCM at the engine's native rate; the caller resamples by handing it to the audio crate, which already resamples on upload. `lv_render` bypasses queues: it produces data, it does not speak.

With the `audio` feature on, positioned speech is an ordinary queued utterance:

```
lv_speak_at(ctx, queue, profile*, text, len, flags, device, space, bus, bounds*, gain, out utterance, out source_id)
```

The utterance enters its queue like any other and honours the same flags and `max_concurrent`. When the queue starts it, the worker renders → `la_buffer_create` → `la_space_fire` a one-shot source with the given bounds on the given bus, `AUTO_REMOVE`, buffer freed when the source finishes. The source id is allocated immediately (the source exists, silent, until the render lands — a few hundred milliseconds for SAPI), so the mod can stop or reposition it like any other, and `lv_utterance_state` tracks it through `Speaking` to `Done` off the source's lifetime. This is wotr's `PositionalSpeech` plus `SpatialSources` plus the render path collapsed into a line, and it is why the speech crate belongs next to the audio crate rather than in a companion. An events queue with `max_concurrent = 0` is the intended home for it.

Profiles on screen readers cannot render (`RENDER` unset); `lv_speak_at` with such a profile returns `NotSupported` unless `RENDER_FALLBACK` is set in the profile's flags, in which case the crate renders through the best native synthesizer with the profile's rate and volume — the mod's `RenderToAudioFallback`.

---

## 7. ABI surface (summary)

Prefix `lv_` (voice). `LsStatus` return codes, extended with `EngineUnavailable` (the chosen engine is absent/not running and `NO_FALLBACK` was set) and `NotSupported` (feature bit unset). Opaque `LvContext*`; queues, engines, voices, utterances and jobs are integers; profiles are caller-owned structs. cdecl, `_Bool` as one byte, `[Out]` on .NET output arrays — the standing rules.

| Group | Functions |
|---|---|
| Lifecycle | `lv_create`, `lv_destroy`, `lv_prism_version`, `lv_abi_version` (shared with `ls_abi_version`) |
| Engines | `lv_engine_count`, `lv_engine_id_at`, `lv_engine_name(id, buf, cap, out len)`, `lv_engine_features(id, out u64)`, `lv_engine_refresh` (re-probe readers that started after boot) |
| Voices | `lv_voice_count(engine)`, `lv_voice_id_at(engine, i)`, `lv_voice_name(engine, id, buf, cap, out len)`, `lv_voice_language(engine, id, buf, cap, out len)` |
| Profiles | `lv_resolve(profile*, out engine, out voice)` — the struct itself needs no functions |
| Queues | `lv_queue_create(options*)`, `lv_queue_destroy`, `lv_queue_set(queue, option, value)`, `lv_queue_depth(queue, out pending, out speaking)` |
| Speaking | `lv_speak`, `lv_output`, `lv_braille`, `lv_gap`, `lv_stop`, `lv_flush`, `lv_pause`, `lv_resume`, `lv_cancel`, `lv_stop_all` |
| Utterances | `lv_utterance_state(id, out state)` |
| Rendering | `lv_render`, `lv_render_poll`, `lv_render_take`, `lv_render_free`, `lv_speak_at` (audio feature) |

Strings **in**: `(const uint8_t* utf8, size_t len)`. Strings **out**: the two-call buffer pattern, UTF-8, length excludes any terminator. No other string crosses.

---

## 8. Threading

The context is driven from one game thread. The ABI verbs only push commands onto queue inboxes (lock-free where it matters, as the audio crate's `rtrb` rings are). The crate owns one **worker thread** that drains the inboxes, sequences each queue, talks to every engine, runs render jobs and fires positioned sources; the worker never calls the host. Prism's audio callback runs on whatever thread Prism chooses and only appends to a job buffer under a lock the game thread never takes. COM is initialized on the worker (MTA); the game thread never touches an engine, so its apartment is irrelevant and the crate never initializes or uninitializes COM there.

---

## 9. Spec amendments this requires

Two lines in `spec.md` move:

- **Non-goals, "Speech output."** → "Speech output *lives in `loadstar-speech`* (`speech-spec.md`), admitted 2026-10 on the audio crate's precedent: two consumers (wotr, Say the Spire 2) with the same stack."
- **Non-goals, "Strings…"** and **FFI conventions, "No strings anywhere in the ABI."** → unchanged for the core and audio surfaces. The speech surface is the single, explicit exception: text to be spoken goes in, voice and engine names come out, by the conventions in §7. The core still emits numbers only; labels still never enter it.

`§10 Companion layers` drops "Speech helper (Tolk binding)" — the companion's speech helper becomes a thin wrapper over `lv_*`.

---

## 10. What mods build on it

- **wotr's whole `src/Speech`** — `SpeechManager`, `SpeechConfig`, the three handlers, `SpeechOutputs`, the render fallback, the never-interrupt policy — becomes: `LvProfile` structs built straight from the Speech settings tree, three queues (UI, dialogue, events with `max_concurrent = 0`), `lv_speak`/`lv_output` behind the `Tts` facade, `lv_speak_at` for positional events. The settings UI keeps its "outputs" model (Auto, each reader by name, SAPI, Clipboard) because that is exactly the engine list with `SCREEN_READER` entries first.
- **Say the Spire 2** gets the same by construction, plus positioned speech it never had, plus the gap setting it faked with timers.
- **A new mod** calls `lv_create`, fills one profile with `LV_ENGINE_AUTO`, and speaks on queue 0. Voice settings are a dropdown over `lv_voice_name`. Nothing else to write.

---

## 11. Roadmap

1. **This document reviewed.**
2. **Crate skeleton: engine trait, queues and the worker, utterance states, `clipboard`**, with the Prism loader and generated bindings against the pinned header; smoke harness speaks through NVDA via Prism from x64 net472 and checks state degradation.
3. **SAPI 5 native** (speak + render + instance cache), the fallback chain, `lv_speak_at` against the audio crate; wotr's `SapiHandler` and `PositionalSpeech` retired behind it.
4. **NVDA direct + the 32-bit validation**; Say the Spire 2 moves over.
5. **OneCore**, SSML passthrough.
6. Later: `lv_submit` batches if a mod hits the ordering race; speech-dispatcher and AVSpeech natively; priorities only if two mods converge on the same scheme.

## 12. Open questions

- Should `auto` prefer a running screen reader over a synthesizer even when the profile sets a voice the reader can't honour? (wotr: yes, the reader always wins; the voice params apply only to synthesizers.)
- Prism's `output` on readers that lack braille: speak only, or `NotSupported`? (Proposed: speak only — `OUTPUT` means "the user's preferred modality set", not "both or fail".)
- `lv_stop` on a queue sharing a reader with another queue cuts the other queue's live utterance (§3). Should the cut utterance report `Cancelled` or `Done`? (Proposed: `Cancelled`, truthfully.)
- Whether `gap_ms` should also apply after an `INTERRUPT`, or only between naturally sequential items. (Proposed: only between sequential items; an interrupt is already a discontinuity.)
