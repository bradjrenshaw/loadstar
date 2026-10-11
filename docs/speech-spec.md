# Loadstar Speech — Design Specification

*Companion to `spec.md` and `audio-spec.md`. Status: implemented 2026-10-11 (`crates/loadstar-speech`, `lv_*` in `loadstar-ffi`, `Loadstar.Net/Speech.cs`); roadmap items 2–3 shipped, wotr migrated.*

*Distilled from the Wrath of the Righteous mod's speech layer (`wotr-access/src/Speech`, ~2,000 lines of C# that went through two refactors before it settled) and Say the Spire 2's, which it was ported from. Those are the validated behaviours; this document turns them into a crate so a new mod has them on day one.*

*Draft 2 made a profile pure configuration and the queue the stateful object. Draft 3 removes the engine as a user-facing concept: the user picks a **voice** from one categorised tree the library provides, and which backend speaks it is the library's business. Categories are an open set so extensions can add their own.*

---

## 1. Purpose and scope

`loadstar-speech` speaks text for a blind player: through their screen reader when one is running, through a synthesizer when not, and — with the audio crate — from a position in the game world. It exists because every accessibility mod rebuilds the same stack: detect the screen reader, fall back to SAPI, let the user pick a voice and a rate, let *some* things speak in a *different* voice, queue announcements so they don't trample each other, never interrupt by default, and render speech to PCM so it can be placed in the soundscape. That stack is policy that is the same in every game, which is exactly what the admission rule (spec §1) asks for: wotr and Say the Spire 2 both need it, identically.

It follows the crate's rules — tools not a framework, no callbacks across the ABI, no update loop, the mod decides *what* is said — with **one deliberate exception, text crosses the ABI** (§9). Nothing else about the FFI conventions changes.

### Latency is first-class

A blind player hears the startup delay of every utterance thousands of times a session, so per-call latency is a design metric here, not a nicety, and "imperceptible" is never a reason to add a stage. The budget this document designs to, measured from the ABI call:

| stage | target |
|---|---|
| `lv_speak` returns (resolve, feature check, enqueue) | < 50 µs, no heap allocation beyond one copy of the text |
| worker wakes and dispatches to a backend | < 200 µs (park/unpark on the inbox; the worker never sleeps on a timer) |
| screen reader hand-off | same frame as the call |
| synthesizer first audio | the backend's own start latency, nothing added |
| positional: queued behind another item | **zero render wait** — rendered ahead while the previous item plays (§6) |
| positional: first in a burst | the render itself; streaming playback (§11) is the roadmap item that cuts it to first-chunk |

Where a simpler design and a faster one exist, this spec takes the faster one and keeps the complexity inside the library (the per-frame pump was rejected on exactly this ground, §6). Per-utterance work the game thread never sees: voice selection (instance cache), resampling (worker, at the device rate), COM calls (worker). The smoke harness measures every row above and a regression fails it.

### In scope (v1)

- **The voice tree**: everything that can speak, as leaves grouped into categories (screen readers, SAPI 5, clipboard, …), each leaf with a stable id and a feature set. Screen readers found through Prism (§4) and the crate's own backends (§5) are leaves in the same tree; the user never sees a backend.
- **Profiles**: a flat description of *how* to speak — one voice id plus rate, pitch, volume — that the mod builds from its own settings and passes with each utterance. No handle, no lifetime.
- **Queues**: ordered lanes the library owns. Each utterance goes on a queue; the queue decides when it starts, what interrupts what, and how many may overlap.
- **Speak, output, braille, stop, flush, pause**, with explicit interrupt semantics (§3) and polled **utterance ids** in place of callbacks.
- **Render to PCM** on voices that can, as a polled job (§6), and **positioned speech**: render, upload and fire a positioned source as one queued utterance when the audio feature is on.

### Out of scope (v1)

- **Persistence of profiles and queue settings.** The mod's settings own the values (spec §2.5, "numbers only"); a profile is already a flat struct of numbers, so there is nothing to persist on the library side.
- **Text formatting, localization, pronunciation dictionaries, SSML authoring.** Text arrives ready to speak. (Voices that accept SSML are flagged, and an SSML flag on the speak call passes it through untouched; the crate never composes it.)
- **Priorities.** A queue orders by submission; the two interrupt flags (§3) are the only reordering. "Events outrank ambience" is expressed by putting them on different queues, not by numbers on utterances. If two mods converge on a priority scheme it enters later.
- **Changing the user's voice behind their back.** The library never substitutes a different synthesizer because the chosen voice cannot do something (position itself, render, speak SSML). It reports `NotSupported` and the mod decides, per call and visibly, what to do instead (§6).
- **Compiling Prism in.** Prism is CMake/C++ with fetched dependencies; Loadstar builds with cargo alone. Prism is dynamically loaded (§4).
- **A runtime plugin ABI for third-party backends.** The tree is designed so one can be added without breaking anything (§2, "Open set"); the plugin contract itself is a later spec section.
- **Linux and macOS backends** beyond what Prism provides at runtime. The backend trait exists so speech-dispatcher and AVSpeech can be added natively when a consumer needs them.

---

## 2. Concepts

### Context

One per process: `lv_create(config) → handle`. Config: the Prism search policy (§4.3). Creation is cheap and never speaks; backends are probed lazily on first use because screen-reader detection and SAPI voice enumeration are slow on some machines and must never stall a game's boot. The context owns one worker thread (§8) and the default queue.

### The voice tree

Everything that can speak is a **leaf** in one tree, two levels deep: **categories** containing **voices**. The library builds the tree; the mod displays it. A mod's "pick a voice" setting is a tree view over this structure and nothing else — there is no separate backend or engine choice anywhere in the UI.

**Categories** are an open set identified by a string **key**, hashed to a stable 64-bit id the same way voice ids are. Each category carries its key, a default English display name, and a **rank** that fixes its position in the list. Built-in keys on Windows, in rank order:

| key | contents |
|---|---|
| `screen_readers` | one leaf per screen reader the machine can reach |
| `sapi5` | the installed SAPI 5 voices |
| `onecore` | the Windows 10+ voices (once §5.2 lands) |
| `sapi4` | reserved; native, later |
| `clipboard` | the single clipboard leaf |

macOS adds `avspeech`, Linux `speech_dispatcher`. Built-in ranks are spaced (screen readers lowest, clipboard highest) so a later or third-party category slots between them by choosing a rank; a third party may also add leaves to an *existing* category by using its key, which is how a new reader implementation lands under Screen readers rather than in a category of its own.

**Voices** are the leaves. A leaf has a stable 64-bit **id** (hash of the backend's own identifier — the SAPI token id, the OneCore voice id, a reader's name), a display name, a language tag where the backend knows one, its category, and a **feature** bitset (below). Under `screen_readers` a leaf is a reader — NVDA, JAWS, Narrator — because the user picks their actual voice inside the reader; under a synthesizer category a leaf is one installed voice. Both are "voices" to the ABI and to the user. Ids are never positional, so a saved profile survives a reinstall that reorders a list, a reader that is not running today, or a tree refresh.

**Autos** are leaves too, with fixed ids. Voice id **0** is the **root auto**: the running screen reader if there is one, else the default voice of the first synthesizer category. Each category has a **category auto** (`lv_category_auto(category)`): the running reader, the system default SAPI voice, and so on. A zeroed profile therefore speaks sensibly.

**How the tree is populated**, in priority order:

1. Screen readers the crate implements itself (§5.3).
2. Screen readers Prism reports available (§4), *except* any reader already covered by step 1 — one leaf per reader, native wins.
3. The synthesizer categories, each from its native backend. Prism's synthesizer backends (`SAPI`, `ONE_CORE`, …) never become leaves; they are slower than the direct paths and would duplicate them.
4. Clipboard.

This encodes the mods' experience as list policy: Prism contributes breadth of *readers*, the crate owns every *synthesizer*, and the user never has to know why a laggy entry isn't there.

**The tree is a snapshot.** Readers start and stop after boot. `lv_tree_refresh(ctx, out changed)` re-probes and rebuilds; `changed` tells the mod to redraw its view. Ids are stable across refreshes. **Resolution reads the snapshot and never probes**: asking whether NVDA is running is a remote procedure call, and Prism's runtime-support check can be one too, so `auto` and every leaf's availability are answered from the last probe. The worker re-probes on `lv_tree_refresh`, on a backend's failure to speak, and on a slow cadence of its own (seconds), so a reader started mid-session appears without the game thread ever paying for the question.

**Open set.** Because categories are keyed strings with ranks rather than an enum, and leaves carry their own features, a backend added later — compiled in behind a cargo feature, or eventually loaded as a plugin — appears in the tree without an ABI change. For the mod's localization rule this means: built-in keys are fixed and documented, so a mod maps them to its own locale entries; an unknown key falls back to the display name the backend supplied, exactly as OS voice names already pass through untranslated.

### Features

One bitset per leaf, shared vocabulary with Prism's so they map 1:1: `SPEAK`, `OUTPUT` (speech plus braille where the reader supports it), `BRAILLE`, `STOP`, `PAUSE`, `IS_SPEAKING`, `SET_RATE`, `SET_PITCH`, `SET_VOLUME`, `RENDER` (speak-to-memory), `SSML`, `MULTI_INSTANCE` (more than one utterance may be in flight at once — synthesizers yes, readers no), `SCREEN_READER`.

Features drive the mod's UI directly: a leaf without `SET_RATE` gets no rate slider; a leaf without `RENDER` cannot be chosen for a positional profile unless the mod decides to allow it and handle the consequence (§6). Nothing is silently ignored.

### Profiles

A **profile** is a plain struct, owned by the mod, passed by pointer on every speak call:

```c
typedef struct LvProfile {
    uint64_t voice;     // leaf id; 0 = root auto
    float    rate;      // 0..1, 0.5 = the voice's default; ignored without SET_RATE
    float    pitch;     // 0..1, 0.5 = default; ignored without SET_PITCH
    float    volume;    // 0..1; ignored without SET_VOLUME
    uint32_t flags;     // NO_FALLBACK, TRIM_SILENCE (render only)
} LvProfile;
```

It maps one to one onto a mod's speech-settings record and is flat data under the FFI rules. The library keeps an internal cache of backend instances keyed on the leaf — voice selection is the slow part of SAPI and OneCore; rate, pitch and volume are cheap and are applied per utterance — so two utterances a frame apart in different voices cost nothing extra. This is what lets an events system speak enemy damage in one voice and ally damage in another without the mod managing voice objects.

### Resolution and fallback

Resolving a profile means finding the leaf that will actually speak. If the chosen leaf is present and its backend is reachable, that is the answer. If not — the user's saved reader is not running, a SAPI voice was uninstalled — resolution **walks up the tree**: the leaf's category auto, then the root auto, then clipboard. `NO_FALLBACK` stops the walk and the call fails with `VoiceUnavailable`.

The walk is the *only* substitution the library ever makes, and it is only ever for *absence*. A leaf that is present but lacks a feature the call needs is `NotSupported`, never swapped for one that has it (§6). The two cases are deliberately different: "your reader isn't running, using SAPI" is something every user understands; "your voice changed because this message was positional" is not.

`lv_resolve(ctx, profile*, required_features, out voice, out status)` answers "what would this profile use right now, and would it work for a call needing these features" without speaking, so the settings UI can say "NVDA (not running — using SAPI 5, Zira)" and a positional-profile picker can grey out the leaves that cannot render. This is wotr's `SpeechConfig.Handler` resolution, made a crate guarantee.

### Queues

A **queue** is an ordered lane the library owns: `lv_queue_create(ctx, options) → queue`, `lv_queue_destroy`. Queue 0 is the default and always exists. Options:

- **`max_concurrent`**: how many of this queue's utterances may be in flight at once. `1` = strictly sequential (UI, dialogue). `0` = unlimited (positional events, where two enemies struck in the same frame *should* overlap). Honoured only on `MULTI_INSTANCE` leaves; a screen reader is one physical channel and is forced to 1 whatever the queue says.
- **`gap_ms`**: silence inserted between consecutive utterances on a sequential queue. Mods set 0..300; it replaces every hand-rolled "don't let announcements run together" timer.

Each utterance carries its own profile, so one queue may alternate voices and backends. The queue is a sequencer, not a voice.

### Utterances

`lv_speak` returns a **utterance id**, a per-context monotonically increasing `uint64_t`. `lv_utterance_state(ctx, id) → {Pending, Speaking, Done, Cancelled, Unknown}` is the whole feedback channel — no callbacks (spec §9). It covers the SAPI-bookmark use cases ("advance the cutscene when this line finishes", "don't start the next readout until this one began") without any of the callback machinery. Ids are never reused. **The state table is bounded**: a fixed ring of the most recent utterances (1024 by default, set in `lv_create`'s config); anything older — or any id the context never issued — answers `Unknown`. A mod that polls an utterance polls it soon after speaking it, so the bound costs nothing in practice and the table never grows with session length. `lv_utterance_voice(ctx, id, out voice)` reports which leaf actually spoke it after fallback.

**Where state is honest.** On synthesizer leaves the library drives the instance and knows the exact state. On a screen reader the library knows Pending (not yet handed over) and, where the backend exposes `IS_SPEAKING`, Speaking and Done; where it does not, the utterance reports `Done` the moment it is handed to the reader. That degradation is deliberate and documented rather than papered over — a mod that needs end-of-speech timing on NVDA cannot have it from any library, and should know.

---

## 3. Speaking

```
lv_speak   (ctx, queue, profile*, text, flags, out utterance) → LsStatus   // voice only
lv_output  (ctx, queue, profile*, text, flags, out utterance) → LsStatus   // voice + braille where supported
lv_braille (ctx, queue, profile*, text,        out utterance) → LsStatus   // braille only
lv_gap     (ctx, queue, ms,                    out utterance) → LsStatus   // silence as a queue item
lv_stop    (ctx, queue)        // cut what is speaking on this queue, keep the rest
lv_flush   (ctx, queue)        // drop everything pending on this queue, keep what is speaking
lv_pause   (ctx, queue) / lv_resume(ctx, queue)
lv_cancel  (ctx, utterance)    // one item: cut if speaking, drop if pending
lv_stop_all(ctx)               // every queue, every backend, including Prism's
```

Flags on a speak call:

- **`INTERRUPT`** — stop what this queue is speaking, drop what it has pending, then speak this. The hard cut: a cursor readout that supersedes the previous one.
- **`FRONT`** — go to the head of this queue without cutting anything. The soft cut: a warning that should come next but not clip the word in progress.
- Neither — append. **This is the default, and it is the SayTheSpire rule**: nothing interrupts unless the caller says so. Dialogue in wotr never interrupts; a cursor readout does. Interrupt is a property of the *call*, never of the profile or the queue.
- **`SSML`** — text is SSML; `NotSupported` on leaves without the feature.
- **`BRAILLE_TOO`** (on `lv_speak`) — same as `lv_output`; kept so bindings with a single speak method can express both.

Rules, all learned by ear in the mods:

- **Speaking never blocks the game thread.** The ABI call resolves the profile, checks features, hands the utterance to the queue and returns; the worker (§8) talks to backends. Even where a backend's own speak call would block (some Prism backends synthesize before returning), the game thread never sees it. Feature failures are therefore **synchronous**: a call that cannot work returns `NotSupported` before anything is queued, never later and silently.
- **Text is a NUL-terminated UTF-8 `const char*`.** No length parameter: the one place a caller could miscount is removed, every binding generator and marshaller handles the form natively (.NET `LPUTF8Str` on a plain `string`, no byte array on the GC heap, no pinning in mod code), and Prism's own API takes exactly this so text passes through uncopied. The crate reads to the first NUL; embedded NULs do not occur in text meant to be spoken. Invalid UTF-8 is `InvalidArgument`; the crate does not repair text.
- **Empty text is a no-op** that returns a valid, already-`Done` utterance id, not an error.
- **Screen readers are one channel across all queues.** Utterances from different queues reach the reader in submission order. `lv_stop` on one queue cuts the reader — which may be speaking another queue's text — because there is nothing else it can cut; it then drops only its own pending items. The spec states this rather than pretending queues are isolated on a reader.
- **Commands, not calls, inside.** Every verb above becomes a command on the queue's inbox, consumed by the worker. That is an implementation statement with user-visible consequences: ordering between a `speak` and a `stop` issued on the same frame is the call order, a debug tap can log and replay the command stream, and a future `lv_submit(queue, commands[], n)` for atomic batches ("interrupt, then A, then B" with no race) costs nothing to add. It is not in v1 because no mod has hit the race yet.

---

## 4. Prism as a backend

Prism (ethindp/prism, MPL-2.0) unifies the screen-reader abstraction libraries and keeps gaining obscure readers — nineteen backend ids at 0.18.3, from NVDA and JAWS to PC-Talker and SenseReader. That breadth is the reason to depend on it, and the pace of change is the reason not to compile it in. In the tree, Prism is invisible: its readers appear as leaves under `screen_readers` beside the crate's own, and the user cannot tell which is which.

### 4.1 Loaded, not linked

The crate binds Prism's C header and loads `prism.dll` (`libprism.so`, `libprism.dylib`) **at runtime with `libloading`**, resolving **every symbol individually by name**. If the library is absent, the Prism-sourced leaves simply don't exist. If a symbol is absent, the feature that needs it is masked off for that session. Nothing links at build time; cargo alone builds Loadstar.

Prism's `speak_to_memory` delivers audio through a **callback**. That callback lives entirely inside the crate — Prism calls Loadstar, Loadstar appends to a job buffer — and never crosses Loadstar's own ABI. The no-callbacks rule applies to Loadstar's surface, not to what it does privately with a library it loaded.

### 4.2 Why dynamic beats static here

- **New readers for free.** Backends come from Prism's registry at runtime; dropping in a newer `prism.dll` exposes a newer reader to every Loadstar mod with no rebuild of Loadstar or the mod — it is a new leaf in the tree on the next refresh.
- **No toolchain coupling.** Prism needs CMake and fetches dependencies; a Loadstar contributor needs `cargo build`.
- **Partial failure instead of total.** A missing library degrades to native backends; a static link that fails to resolve fails the whole dll at load.

### 4.3 Pinning and tracking

- Loadstar carries **Prism's header as a git submodule pinned to a release tag** (`external/prism`), and generates the Rust declarations from that header at build time (`bindgen`, behind the default-on `prism` feature; the generated file is checked in so a build without LLVM still works). A signature change in a newer header fails the build at generation time, loudly, instead of corrupting a call at runtime.
- `tools/fetch-prism.ps1 / .sh` downloads the pinned tag's release zips (Prism publishes per-platform binaries for every tag: Windows x64/ARM64, Linux, macOS, mobile) into the companion's distribution directory, so a mod that ships Loadstar can ship the matching Prism beside it.
- **Bumping Prism** is: move the submodule, regenerate, run the smoke harness, refetch binaries. The crate's version note records which Prism tag it was validated against; `lv_prism_version` reports what was loaded (from the library's own version export when Prism adds one; the file version resource until then).
- **Search order** for the library: a path passed in `lv_create`'s config, then beside `loadstar.dll`, then the process directory, then the platform loader's default search. The config can also say *never load Prism* — a mod that wants deterministic behaviour under test.

### 4.4 Prism's contract, as Loadstar relies on it

Only these are load-bearing; everything else is optional and feature-masked: `prism_init` (called with a **null** config: `prism_config_init` is resolved but never called — in the v0.18.3 Windows x64 release the call itself faults, the by-value one-byte struct return not matching the binary, and a null config means the same defaults), `prism_shutdown`, the registry enumeration (`count`, `id_at`, `name`, `get_features`, `create`, `create_best`), `backend_initialize`, `backend_free`, `backend_get_features`, `backend_speak`, `backend_output`, `backend_braille`, `backend_stop`. Speak-to-memory, pause/resume, is-speaking, rate/pitch/volume and the voice functions are consulted only when their feature bit is set.

Which Prism backends become leaves is the tree's population rule (§2): readers not already implemented natively, never synthesizers.

### 4.5 The 32-bit gap

Prism publishes no 32-bit Windows build, and Say the Spire 2 / CSTW are 32-bit processes. On `i686` the Prism loader is compiled in but will find no library unless someone builds one from source; `tools/fetch-prism` has an opt-in `-Build32` step for a machine with CMake. The native backends (§5) carry 32-bit mods until then, which is the status quo those mods already live with.

---

## 5. Native backends

Inside `loadstar.dll`, no external files, each behind a feature of its own so a Linux build drops the Windows ones:

### 5.1 SAPI 5 (Windows) — category `sapi5`

COM via the `windows` crate (already a dependency of the audio crate): one `ISpVoice` per leaf cache entry, and additional instances created on demand when a queue's `max_concurrent` asks for overlap, so overlapping utterances are real concurrent voices rather than one purged voice. Voice enumeration through the token category; rate/volume mapped from the normalized 0..1 to SAPI's −10..10 and 0..100. Speaks asynchronously (`SPF_ASYNC`), cuts with `SPF_PURGEBEFORESPEAK` on the instance that owns the utterance. **Render to memory** uses instances bound to a memory stream, never one that is speaking live — the mod's `SapiHandler` rule, now a property of the instance cache rather than a special case. This is the engine wotr ships today as "manual COM SAPI" and the one that proved Prism's SAPI path too slow.

### 5.2 OneCore (Windows 10+) — category `onecore`

`Windows.Media.SpeechSynthesis` via WinRT, for the modern voices and the cleanest render path (`SynthesizeTextToStreamAsync` gives PCM directly, with SSML support). *Validation item*: WinRT activation from Rust on the 32-bit target.

### 5.3 NVDA controller client (Windows) — leaf under `screen_readers`

`nvdaControllerClient64.dll` / `32.dll` loaded dynamically: `nvdaController_testIfRunning`, `speakText`, `brailleMessage`, `cancelSpeech`. Four functions, and it covers the single most common reader on 32-bit hosts where Prism is absent. Under the population rule it is *the* NVDA leaf whenever NVDA is running; Prism's NVDA backend is not added beside it. (If Prism's backend ever offers a feature this one lacks, the fix is to add it here, not to list both.) No `IS_SPEAKING`, so utterances on it report `Done` at hand-over (§2).

### 5.4 Clipboard (Windows) — category `clipboard`

Copies the text. Exists so "something happens" when nothing else can; the fallback walk's last step.

---

## 6. Rendering and the audio crate

```
lv_render(ctx, profile*, text, flags, out job)                            → LsStatus   // worker job, not queued
lv_render_poll(ctx, job, out state)                                       → {Pending | Done | Failed}
lv_render_take(ctx, job, out samples*, out frames, out channels, out rate)             // library-owned until lv_render_free
lv_render_free(ctx, job)
```

Rendering is blocking in every backend, so it runs on the worker and is **polled, never called back** — the same shape as `la_device_finished`. The result is f32 PCM at the backend's native rate; the caller resamples by handing it to the audio crate, which already resamples on upload. `lv_render` bypasses queues: it produces data, it does not speak. It requires `RENDER` on the resolved leaf and returns `NotSupported` otherwise.

With the `audio` feature on, positioned speech is an ordinary queued utterance:

```
lv_speak_at(ctx, queue, profile*, text, flags, device, space, desc*, out utterance, out source_id)
```

`desc` is the mod's own `LsAudioSourceDesc` — bus, bounds, gain, pan width, priority — exactly as it would fire any one-shot; the library fills `id` (the returned source id), `buffer` (the render) and the PLAYING/AUTO_REMOVE flags. Nothing about placement is re-expressed in the speech ABI. The utterance enters its queue like any other and honours the same flags and `max_concurrent`. The pipeline, per utterance:

1. **Call** (game thread): resolve the profile, check `RENDER`, allocate the utterance id and a source id, push the command. Nothing touches the audio device.
2. **Render** (worker): the backend produces PCM; the worker converts it to mono f32 *at the device's rate* (queried once via `la_device_info`), so the resample never runs on the game thread. **Render-ahead**: on a sequential queue the worker renders pending positional items *while the current one plays*, on a spare backend instance, so that when an item reaches the head its audio is already in memory and it fires at once. On an unlimited-concurrency queue every item renders immediately on arrival. An item that is flushed or cancelled while rendering ahead is discarded; the only wasted work is one render. The first item of a burst still waits for its own render — the streaming source (§11) is what removes that.
3. **Hand-off** (worker, directly): `buffer_create` → `fire` the mod's `SourceDesc` with the buffer filled in and `AUTO_REMOVE` → `buffer_free` immediately, through the audio device's in-process intake (below). The free is safe because the playing voice holds its own `Arc` to the buffer, and it is *cheap* because a retiring voice hands that reference to the garbage ring rather than dropping it in the callback (audio-spec, threading rules) — so the last reference to seconds of speech is released on the game thread, never on the audio thread.
4. **State**: `Speaking` from the fire; `Done` once the buffer's duration (frames ÷ rate ÷ pitch) has elapsed. The speech crate does **not** consume the device's finished ring — that is single-consumer and the mod already drains it for its own sources.

**Why the worker fires directly.** The audio device's command path was single-producer (one `rtrb` producer, `&mut` methods, one control thread). The alternative — a per-frame `lv_pump` the mod calls to hand completed renders to the device on the game thread — was considered and rejected: its latency (one frame on a few-hundred-millisecond render) is harmless, but it is a call a mod can forget, and forgetting it fails *silently* (positional speech renders and never plays), and it stalls whenever the game thread stalls even though the audio thread is fine. Instead the audio crate gains a **locked in-process intake**: the command producer behind a mutex and buffer ids from an atomic, so a second in-process thread can upload and fire. The game thread's own FFI calls take the same uncontended lock (tens of nanoseconds); contention exists only in the instant a render completes. The ring stays single-producer underneath because the lock serialises the producers. Nothing about the ABI changes, no new call exists for mods to forget, and speech keeps flowing through GC pauses and load hitches. (`audio-spec.md` §threading records the relaxed contract.) This is not a library-driven update loop: the worker acts only on utterances the mod submitted.

The source id is allocated at call time and is the mod's handle from then on: `la_source_stop` cuts it, and `la_source_upsert` with fresh bounds on that id each frame makes the line follow a unit that walks while it speaks (the audio crate accepts upserts on fired voices). That replaces wotr's live-tracked `SpatialSources` for speech. Bounds are in the space's frame and units — right-handed, north on −Z, `unit_scale` — exactly as for every other source. This is wotr's `PositionalSpeech` plus `SpatialSources` plus the render path collapsed into one queued item, and it is why the speech crate belongs next to the audio crate rather than in a companion. An events queue with `max_concurrent = 0` is the intended home for it.

### Positioning a voice that cannot render

No screen reader can hand back PCM — NVDA, JAWS and the rest have no render-to-memory API — so a reader leaf can never be positioned, and the root auto resolves to a reader whenever one is running. The library's answer is `NotSupported`, synchronously, and **nothing else happens**: it never quietly renders through a synthesizer the user did not choose, because a voice that changes with the kind of message is a bug report waiting to happen. The mod chooses, per call, with one of two flags, or handles the status itself:

- **`RENDER_ELSEWHERE`** — render through the first synthesizer category's auto voice at the profile's rate and volume. The words land where the event is, in a voice that is not the user's. A mod exposes this as a visible setting ("position events using SAPI") or not at all.
- **`SPEAK_FLAT`** — speak unpositioned through the resolved leaf. The words arrive in the user's own voice; the position is lost, and `lv_utterance_voice` plus a `FLAT` bit in the utterance state tell the mod so it can fire its own positional cue beside the speech. For a reader user this is usually the better experience, and it is the one wotr's events system can do today because every event already has a sound.

**Concurrency follows the physical channel.** Rendered utterances on an unlimited queue overlap freely, each on its own instance and its own audio source. A `SPEAK_FLAT` utterance that went to a reader is serialised on the reader's single channel like any other text, regardless of the queue setting. Rendered audio and reader speech never compete: one goes to the audio crate, the other to the reader.

**UI implication.** When a mod lets the user pick a voice for a positional profile, `lv_resolve` with `RENDER` in `required_features` tells it which leaves to grey out. wotr's setup wizard steering event feedback toward distinct SAPI voices is this rule applied by hand.

---

## 7. ABI surface (summary)

Prefix `lv_` (voice). `LsStatus` return codes, extended with `VoiceUnavailable` (the chosen voice is absent/not running and `NO_FALLBACK` was set) and `NotSupported` (feature bit unset on the resolved leaf). Opaque `LvContext*`; queues, categories, voices, utterances and jobs are integers; profiles are caller-owned structs. cdecl, `_Bool` as one byte, `[Out]` on .NET output arrays — the standing rules.

| Group | Functions |
|---|---|
| Lifecycle | `lv_create`, `lv_destroy`, `lv_prism_version`, `lv_abi_version` (shared with `ls_abi_version`) |
| Tree | `lv_tree_refresh(out changed)`, `lv_category_count`, `lv_category_at(i, out id)`, `lv_category_key(id, buf, cap, out len)`, `lv_category_name(id, buf, cap, out len)`, `lv_category_rank(id, out rank)`, `lv_category_auto(id, out voice)` |
| Voices | `lv_voice_count(category)`, `lv_voice_at(category, i, out id)`, `lv_voice_name(id, buf, cap, out len)`, `lv_voice_language(id, buf, cap, out len)`, `lv_voice_category(id, out category)`, `lv_voice_features(id, out u64)` |
| Profiles | `lv_resolve(profile*, required_features, out voice, out status)` — the struct itself needs no functions |
| Queues | `lv_queue_create(options*)`, `lv_queue_destroy`, `lv_queue_set(queue, option, value)`, `lv_queue_depth(queue, out pending, out speaking)` |
| Speaking | `lv_speak`, `lv_output`, `lv_braille`, `lv_gap`, `lv_stop`, `lv_flush`, `lv_pause`, `lv_resume`, `lv_cancel`, `lv_stop_all` |
| Utterances | `lv_utterance_state(id, out state)`, `lv_utterance_voice(id, out voice)` |
| Rendering | `lv_render`, `lv_render_poll`, `lv_render_take`, `lv_render_free`, `lv_speak_at` (audio feature) |

Strings **in**: NUL-terminated UTF-8 `const char*`, no length (§3). Strings **out**: the two-call buffer pattern — null buffer returns the required capacity *including* the terminator; a real buffer is filled, always NUL-terminated within capacity, and the reported length excludes the terminator, so a C caller reads the buffer directly and a .NET caller makes one `PtrToStringUTF8` call. No other string crosses.

---

## 8. Threading

The context is driven from one game thread. The ABI verbs resolve, check and push commands onto queue inboxes (lock-free where it matters, as the audio crate's `rtrb` rings are). The crate owns one **worker thread** that drains the inboxes, sequences each queue, talks to every backend, runs render jobs and fires positioned sources through the audio device's locked in-process intake (§6); the worker never calls the host. Nothing in the speech crate requires a per-frame call from the mod. **The worker never polls**: it parks on the inbox and is unparked by every push; its only timers are `gap_ms` expiries and duration-based `Done` transitions, armed exactly, and a backend whose completion must be polled (SAPI's stream end) is polled at the backend's own event, not on a tick. Render-ahead runs on the same worker between dispatches, and is pre-empted by any new command so a flat `lv_speak` never waits behind a render — if a backend's render call cannot be interrupted, renders run on a second thread dedicated to them. Prism's audio callback runs on whatever thread Prism chooses and only appends to a job buffer under a lock the game thread never takes. COM is initialized on the worker (MTA); the game thread never touches a backend, so its apartment is irrelevant and the crate never initializes or uninitializes COM there. Tree refresh runs on the worker too; the ABI call returns the previous snapshot until the new one is swapped in atomically.

---

## 9. Spec amendments this requires

Two lines in `spec.md` move:

- **Non-goals, "Speech output."** → "Speech output *lives in `loadstar-speech`* (`speech-spec.md`), admitted 2026-10 on the audio crate's precedent: two consumers (wotr, Say the Spire 2) with the same stack."
- **Non-goals, "Strings…"** and **FFI conventions, "No strings anywhere in the ABI."** → unchanged for the core and audio surfaces. The speech surface is the single, explicit exception: text to be spoken goes in, voice and category names come out, by the conventions in §7. Inbound text is NUL-terminated, never length-prefixed. The core still emits numbers only; labels still never enter it.

`§10 Companion layers` drops "Speech helper (Tolk binding)" — the companion's speech helper becomes a thin wrapper over `lv_*`.

---

## 10. What mods build on it

- **wotr's whole `src/Speech`** — `SpeechManager`, `SpeechConfig`, the three handlers, `SpeechOutputs`, the render fallback, the never-interrupt policy — becomes: `LvProfile` structs built straight from the Speech settings tree, three queues (UI, dialogue, events with `max_concurrent = 0`), `lv_speak`/`lv_output` behind the `Tts` facade, `lv_speak_at` for positional events. The settings UI's separate "output" picker (Auto / screen reader / SAPI / clipboard) goes away; "Voice" becomes one tree view over `lv_category_*` / `lv_voice_*`, with built-in category keys mapped to the mod's locale and leaf names passed through. Positional profiles grey out non-`RENDER` leaves, and whether to offer `RENDER_ELSEWHERE` is a visible Enhancements toggle.
- **Say the Spire 2** gets the same by construction, plus positioned speech it never had, plus the gap setting it faked with timers.
- **A new mod** calls `lv_create`, zeroes one profile, and speaks on queue 0. Voice settings are a tree view over the two enumerations. Nothing else to write.

---

## 11. Roadmap

1. ~~This document reviewed.~~ Done.
2. ~~Crate skeleton~~ **Shipped 2026-10-11** with step 3 (SAPI 5, fallback walk, `lv_speak_at` through the audio intake; wotr's `SapiHandler`/`PrismHandler`/`PositionalSpeech` deleted). Measured on the author's machine: `lv_speak` 8–14 µs, NVDA hand-over 21 ms, SAPI render 65 ms for 3 s of audio, resolve 70–150 ns. — Crate skeleton: backend trait, the tree (categories, leaves, autos, refresh), queues and the worker, utterance states, `clipboard`**, with the Prism loader and generated bindings against the pinned header; smoke harness speaks through NVDA via Prism from x64 net472 and checks state degradation.
3. **SAPI 5 native** (speak + render + instance cache), the fallback walk, `lv_speak_at` and the two positional flags against the audio crate; wotr's `SapiHandler` and `PositionalSpeech` retired behind it.
4. **NVDA direct + the 32-bit validation**; Say the Spire 2 moves over.
5. **OneCore**, SSML passthrough.
6. **Streaming positional speech**: a ring-fed source kind in the audio crate so a render plays from its first chunk instead of its last — cuts first-in-burst positional latency from the whole render to one chunk. The largest remaining latency item once render-ahead is in; wants the audio crate's streaming source, which is also what long ambiences need.
7. Later: `lv_submit` batches if a mod hits the ordering race; the plugin ABI for third-party backends (the tree already has room); speech-dispatcher and AVSpeech natively; priorities only if two mods converge on the same scheme.

## 12. Open questions

- Prism's `output` on readers that lack braille: speak only, or `NotSupported`? (Proposed: speak only — `OUTPUT` means "the user's preferred modality set", not "both or fail".)
- `lv_stop` on a queue sharing a reader with another queue cuts the other queue's live utterance (§3). Should the cut utterance report `Cancelled` or `Done`? (Proposed: `Cancelled`, truthfully.)
- Whether `gap_ms` should also apply after an `INTERRUPT`, or only between naturally sequential items. (Proposed: only between sequential items; an interrupt is already a discontinuity.)
- Category display names for third-party categories: English only from the backend, or per-language (the backend supplies a table, the mod asks with the game's language tag)? (Proposed: per-language optional, English required; built-ins need neither since mods localise their keys.)
