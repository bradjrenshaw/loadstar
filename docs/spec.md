# Loadstar — Design Specification

*A cross-engine navigation and spatial-awareness core for game accessibility mods.*

*Founding design document, distilled from the Wayfinder/CSTW experiment (2025–2026). Status: pre-implementation.*

---

## 1. Purpose and scope

Loadstar gives accessibility mods the computational tools blind players need to navigate game worlds — object tracking, pathfinding, reachability, spatial queries — for games that do not expose their own pathfinding, across engines and languages.

- **Core**: Rust, built as a `cdylib` exposing a flat C ABI.
- **Consumers**: C# (Unity via BepInEx, XNA/MonoGame/.NET Framework games), C++ (Unreal), Lua (UE4SS mods), each through generated bindings plus a thin idiomatic companion layer.
- **Targets** include 32-bit Windows (`i686-pc-windows-msvc`) — many moddable games are x86.

### Explicit non-goals

Learned the hard way from Wayfinder. The core does **not** contain:

- **Input handling or action routing.** The mod detects input and calls functions.
- **Screens, scenes-as-lifecycle, systems, priorities, or any update loop.** The library never acts spontaneously; there is no inversion of control.
- **UI/menu reading.** Game-specific by nature; no abstraction survived contact with real games.
- **Strings, text formatting, or localization.** The core emits numeric/structured data only. Labels never enter the library.
- **Speech output.** Tolk-style output lives in per-language companion layers.
- **Audio.** Deferred by decision (2026-08); revisit with better ideas later. Tone synthesis, wall-tone channel logic, and playback are all out for now. (The cardinal wall-distance *scan* survives as a plain spatial query — see §6.1.)
- **Cross-context routing.** "This dungeon exit leads to overworld (52, 88)" is game knowledge. Mods that need multi-map routing build their own graph over (context, position) pairs. A helper may be added later if multiple mods hand-roll the same thing — not on speculation.

### Feature admission rule

A feature enters the core only if (a) a real mod used it, and (b) a second game plausibly needs it the same way. Everything in this spec passes (a) via the CSTW mod; the first Unity/Unreal consumer tests (b). No speculative pipelines (the voxel lesson).

---

## 2. Design principles

1. **Tools, not a framework.** Each layer is usable without the layers above it. A mod may use only the pathfinder, only the entity registry, or the whole guide layer.
2. **No inversion of control.** No callbacks, no events across the ABI, no update loop. The mod calls in; the library answers and maintains its own caches.
3. **Node sets are the common currency.** Every map/entity interaction reduces to three primitive operations:
   - **Projection**: bounds → set of nav nodes (the only map-type-specific op).
   - **Inverse**: node → position.
   - **Geometry**: nearest-point / bearing / distance (map-independent).

   All higher logic (blocking, goal generation, reachability) is written once against node sets.
4. **No unified map interface.** Tile grids and nav meshes are separate concrete types with separate APIs. Internal code reuse via Rust generics is fine; API-level map polymorphism is not. Contexts are typed by their map family.
5. **Numbers only.** Entity identity is a mod-derived integer. Categories, groups, directions, actions are enums/ints. Paths are node IDs. Formatting is the companion layer's job.
6. **Deterministic and pure.** No clocks, no RNG, no I/O in the core. Same inputs → same outputs. (Also makes the core trivially property-testable.)

---

## 3. Architecture: five layers

Dependency flows downward only. Each layer is independently usable.

| # | Layer | Contents | Depends on |
|---|-------|----------|------------|
| 1 | **Geometry** | Vec3; bounds (point, tile-rect, box, sphere, poly-prism); nearest-point, contains; bearing/elevation/distance utility | nothing |
| 2 | **Graph algorithms** | A* with blocked-group masks; flood fill; goal-candidate generation (onto vs. adjacent); start snapping; path segment compression | node IDs only |
| 3 | **Map types** | `TileGrid` (v1); `MeshWorld` (v2); each provides: the graph, projection, node↔position, clearance data per agent profile | 1, 2 |
| 4 | **Entity registry** | numeric records, snapshot sync with core-side diffing, dirty tracking | 1 |
| 5 | **Guide** | blocked-group maintenance, reachability cache, tracker (browse/announce/pathfind-to) | all |

The **context** (§4) is the only top-level object; the layers are its internals, individually reachable through it.

---

## 4. Contexts

A **context** bundles exactly the state that must stay mutually consistent:

> one map + one entity registry + agent profiles + all derived caches (clearance fields, group projections, reachability, tracker selection).

- Created typed: `ls_tile_context_create(...)` / `ls_mesh_context_create(...)`. The constructor fixes the map family; tile-only functions exist only on tile contexts.
- Multiple contexts coexist. Example (SMT3 Nocturne): one tile context for the 2D overworld, one 3D context per dungeon, all alive at once. Example (CSTW): one context rebuilt per map transition, or one cached context per map ID (~104 maps fit trivially in memory; revisiting preserves tracker state and skips rebuild).
- **A context is a handle, not a lifecycle.** No active/inactive flag, no modes, no ticking. "Which context is current" = which handle the mod passes. Switching maps = calling into a different handle.
- Destruction frees everything; no unsubscribe choreography.
- **Thread contract**: contexts share nothing. Each context requires external synchronization (one caller at a time), but different contexts may be used from different threads freely — e.g., build the next dungeon's context on a background thread while querying the current one.

---

## 5. Entities (layer 4)

### Record — all numeric, no strings

```
LsEntity {
    id:            u64     // mod-derived unique key; the mod picks the scheme
                           // (e.g. hash of "npc:{map}:{index}"). Identity, forever.
    category:      u16     // mod-defined meaning; core only groups by equality
    bounds:        Bounds  // tagged union — see "Bounds" below
    blocking:      u32     // bitmask of blocking groups (0 = doesn't block)
    flags:         u32     // PATHFIND_ADJACENT | HIDDEN | ...
    sort_key:      u32     // explicit browse order within category
}
```

- **Labels, custom names, descriptions live mod-side**, keyed by `id`. They are display state, can change per frame with game logic, and never participate in identity or diffing.
- Category *meanings*, default flags per category, etc. are mod/companion conventions applied when constructing records — the core has no category registration.
- `flags` bits are **mod-defined** (bit 0 = `HIDDEN` by convention only). Trackers select on them via `FLAGS` filters (§8); the core attaches no semantics. Wayfinder's `IFilter` chain is replaced by mod-computed bits + declarative selection; the one filter needing core data — reachability — is a built-in filter type backed by the context's flood-fill cache.
- **Target override** (user redirects an object's pathfinding target — wayfinder called this "linking") is data: an optional per-entity override position, passed in like any other field. Not to be confused with connectivity **links** (§6.4).

### Bounds

One tagged union; **every variant is defined in 3D space** and "2D" is a constrained interpretation (tile convention: x=col, z=row, y=layer). No parallel 2D/3D variant families.

```
Bounds = POINT      { pos: Vec3 }
       | TILE_RECT  { col, row, w, h, layer | ALL_LAYERS }   // grid-native
       | BOX        { center: Vec3, extents: Vec3 }          // AABB
       | SPHERE     { center: Vec3, radius }                 // circle in 2D use
       | POLY_PRISM { poly_id: u32, offset: Vec3 }
```

- **POLY_PRISM** is the polygon story for both worlds: a 2D vertex loop in the XZ plane extruded over a vertical range — which is what zone/trigger volumes actually are in Unity and Unreal. In a tile context the vertical range collapses to a layer (or all-layers); in a mesh context it's a real prism. A rotated rectangle (OBB) is a 4-vertex prism, so no OBB variant exists. True convex-hull/mesh bounds are deferred under the feature-admission rule.
- **Polygons are registered, not inlined**: `ls_register_polygon(ctx, verts_xz[], count, y_min, y_max) → poly_id`. Keeps the entity record fixed-size POD (no pointers in snapshot arrays), dedupes shared shapes, and `offset` makes moving polygon entities a cheap numeric update. Registered polygons live and die with the context.
- **Per-map support is honest**: TileGrid projects POINT, TILE_RECT, POLY_PRISM (scanline rasterization), BOX/SPHERE by footprint; TILE_RECT in a mesh context is a type error, not a guess.
- Every variant implements the layer-1 contract: `center`, `nearest_point(from)`, `contains(point)` — pure math (prism nearest-point = 2D polygon nearest-point + y clamp). Projection remains the map's job.

### Sync — the core does the diffing

Two modes; mods pick per situation:

- **`ls_sync(ctx, entities[], count)`** — declarative snapshot. The mod re-scans game state each frame and sends everything it sees. The core compares each record's numeric fields against its store, marks only real changes dirty, and removes IDs absent from the snapshot. Immediate-mode/reconciliation model: maximum work taken off the mod dev, no missed-removal bugs. Cost is one flat memcpy (50 entities ≈ 2 KB; even 1,000 ≈ 100 KB/frame is microseconds).
- **`ls_upsert(ctx, entities[], count)` / `ls_remove(ctx, ids[], count)`** — explicit deltas for mods that hook spawn/despawn events.

Plus `ls_set_player(ctx, pos)` every frame.

### Dirty-cost rules

- Non-blocking entity moved → nothing invalidated (its projection is computed at query time).
- Blocking entity moved → re-project that one entity's node set.
- Tile/edge data changed, or a blocking entity moved → reachability cache invalidated (recomputed lazily at next query).

---

## 6. Map types (layer 3)

### 6.1 TileGrid (v1 — CSTW-validated)

**Node** = (col, row, layer), packed into a `u32` node ID.

**Canonical connectivity is the directed edge**, not cell walkability. For each (cell, layer, direction): blocked, or (target cell, target layer, cost). This natively expresses:

- one-way edges (Pokémon ledges),
- pairwise terrain-transition rules (CSTW: terrain classes 3–6 can't cross into a different class; 7↔8 never crosses),
- layer-changing edges (CSTW ramps: obstruction 3–6 tiles change floor only when exited in one specific direction; adjacent-cell stairs are just edges to another layer),
- per-edge costs (swamp), default 1,
- world-map wrapping (CSTW 200×180 torus) via a wrap flag.

Memory: bitmask (4 bits/cell/layer) + exception table for the rare edges with layer changes or non-unit costs. CSTW world map = 288k directed edges; negligible.

**Why baking works**: connectivity rules are pure functions of static tile data (proven by `CSTWTileConnectivity` — it *felt* dynamic only because Wayfinder called it as an in-process interface during search). The mod evaluates its game's rules once per map load — enumerate every (cell, direction), ask the game's logic, bake the answer. A few hundred thousand evaluations, single-digit milliseconds, once per map transition. Chatty per-edge callbacks across FFI are thereby designed out.

**Upload tiers** (easy case stays easy):

1. `tile_build_walkable(ctx, w, h, layers, bitmask)` — core derives symmetric edges.
2. `tile_build_edges(ctx, w, h, layers, edge_masks, exceptions[], n)` — full directed control.
3. *(possible later convenience)* per-cell terrain class + (class, class, direction) rule matrix, expanded by the core into edges. A builder atop tier 2, not a different representation.

**Patch ops**: `tile_set_cells(...)`, `tile_set_edges(...)` for doors, switches, drained water. A handful of edges change; reachability invalidates; next query re-floods. No in-place resizing — a map with new dimensions is a new map (or new context).

**Grid-specific queries**: cardinal ray scan — distance to first blocked edge in each of 4 directions from a cell (the computational half of wall proximity audio; what the mod does with the distances is its business). Low priority but validated.

### 6.2 MeshWorld (v2 — first 3D consumer drives it)

**Node** = polygon/triangle index.

- v2 ingests **one mesh per agent profile**, matching where the data actually comes from: Unity and Unreal bake agent-specific (radius-eroded) navmeshes the mod can extract. Any path on such a mesh is valid for agents at or below its baked radius — query-time size checks are free.
- Query-time **portal-width filtering** (annotate triangle crossings with portal width; A* rejects portals narrower than agent diameter; funnel inflates corners by radius) is the later addition for one-mesh-many-sizes situations.
- String-pulling/funnel is a **path utility** (nodes → smooth waypoints), not part of search.

### 6.3 Links (off-grid / off-mesh connections)

Connectivity has two stores feeding the same neighbor function:

- **Base edges** — the dense per-cell directional table (grids) or polygon adjacency (meshes). Direction-indexed; only reaches adjacent nodes.
- **Links** — a sparse list of arbitrary directed connections for everything that isn't cell-to-adjacent-cell: teleporters, ladders, distant stairs, elevators, cave entrances. The Recast/Unity "off-mesh link" concept, shared by both map families.

```
LsLink { link_id: u64 (mod-assigned), from: Vec3, to: Vec3, cost: f32,
          flags: BIDIRECTIONAL | ... , enabled: bool }

ls_add_link(ctx, link)          // endpoints snapped to nearest node/polygon
ls_remove_link(ctx, link_id)
ls_set_link_enabled(ctx, link_id, bool)   // unpowered teleporter, locked door
```

- To layer 2, an enabled link is just one more neighbor entry — A*, flood-fill, and goal candidates traverse links with no special machinery, so reachability through teleporters is automatic. Adding/removing/toggling a link invalidates the reachability cache like any edge change.
- **Links break guidance segments** (§8): compressing cardinal steps across a teleport would produce nonsense, so path results mark link traversals and the segment list carries a `LINK { link_id }` variant. Mod-assigned IDs exist so the mod can look up its own label ("blue portal") when formatting.
- **Profile vs. link**: systematic geometry quirks (uniform step heights à la SMT3) belong in the agent profile (`step_height`) or the baked edge rules; links are the manual patch for one-off spots where geometry defies the profile. If the same link pattern is being created many times, the profile or bake should have handled it.
- Possible later field: a profile mask (climb-links only for capable agents) — deferred under the admission rule.

### 6.4 Voxel maps

Not designed until a target game (SMT3-style) demands them. The Wayfinder voxel pipeline is a cautionary tale, not a porting source.

---

## 7. Agent profiles

```
LsProfile { radius, height, step_height, footprint_cells }   // fields per map family
```

- Registered **per context** (`ls_register_profile(ctx, ...) → profile_id`), because a profile's baked consequences live in the map data: clearance field on a grid, per-profile mesh in 3D. Profile and map must share an owner.
- **Profile 0 = point/one-tile agent**, the default; single-profile mods (the dominant accessibility case — the agent is the player) never think about profiles at all.
- **TileGrid**: per-profile clearance field (distance transform: largest footprint that fits anchored at each node), computed at build, patched locally on cell/edge changes. A* skips nodes with clearance below the profile's footprint. Composes with directed edges: clearance answers "does the agent fit," edges answer "may it move" — both must pass. For profile 0 the check compiles to nothing.
- **Threads through every query** — this is the correctness-critical part. `find_path`, reachability flood-fill, goal-candidate generation (object bounds inflated by agent radius when computing "adjacent" stands), and start snapping all take the profile ID. Otherwise the tracker announces "reachable" and pathfinding then fails — exactly the contradiction the library exists to prevent. Reachability cache is keyed by (profile, blocked-mask).

---

## 8. Navigation queries (layers 2 + 5)

### Pathfinding

```
ls_find_path(ctx, start_pos, goal, profile_id, blocked_mask, opts)
    → path_id  (then: ls_path_nodes(path_id) → u32[])
```

- **The fundamental result is a node-ID array.** Tiles → (col,row,layer); mesh → triangle indices. Mods doing their own post-processing (custom funnel, room detection, annotation) consume this directly.
- **Derived views, called only if wanted**:
  - `ls_node_position(ctx, node) → Vec3` (tile center / triangle centroid), and a batch nodes→waypoints call;
  - `ls_path_segments(path_id) → Segment[]` where `Segment = MOVE { direction: enum, count: u32 } | LINK { link_id: u64 }` — cardinal compression for grids, broken at link traversals ("north 5, take teleporter, east 3" *as data*; words are the companion's job). Raw node paths carry the same link-transition flags;
  - funnel/string-pulling for meshes (v2).
- **Goal semantics per entity**: `PATHFIND_ADJACENT` set (NPCs) → goal candidates = `neighbors(projected set) \ projected set`, with the entity's own blocking groups still blocked and the candidate-goal fallback choosing the cheapest reachable candidate. Clear (exits) → goal = the projected set itself, with the entity's own groups dropped from the blocked mask (walk-onto). Both behaviors and the start-snapping/exact-waypoint details are ports of hard-won Wayfinder fixes — encoded once, in node-space, map-type-agnostic.

### Reachability

- Lazy flood fill from the player's node, cached per (profile, blocked-mask), invalidated per §5 rules.
- Entity test: projected node set **∪ its neighbors** intersects the reachable set (the neighbor union is why an NPC standing on blocked tiles still counts as reachable).
- API: `ls_entity_reachable(ctx, id, profile) → bool`, plus bulk form.

### Trackers

A tracker is a selection state machine created on a context. **Several may coexist**, each with its own declarative filter set and its own selection state. Trackers survive map switches to *other* contexts (overworld selection intact after a dungeon trip) and die with their context.

```
ls_tracker_create(ctx, filters[], n) → tracker
ls_tracker_set_filters(tracker, filters[], n)        // replace wholesale
ls_tracker_set_filter_enabled(tracker, i, bool)      // hotkey toggles
ls_tracker_destroy(tracker)

Filter = REACHABLE  { profile, blocked_mask }   // uses the context's shared flood-fill cache
       | RANGE      { max_distance }            // nearest-point distance to player
       | CATEGORIES { mask }                    // subset scanner ("quest items only")
       | FLAGS      { require, forbid }         // mod-defined entity flag bits

ls_tracker_act(tracker, action: enum) → { entity_id: u64, category: u16, index: u32 }
// actions: NEXT, PREV, CATEGORY_NEXT, CATEGORY_PREV, CURRENT
// entity_id 0 = empty list. The tracker computes no spatial data and
// presumes no announcement policy.
```

- Filters AND together, evaluated lazily at browse time. The core has **zero built-in visibility rules** — visibility is exactly the filter list. Even hiding is convention: `HIDDEN` is flag bit 0, and the companion's default tracker forbids it; a "show hidden objects" scanner is a tracker that doesn't.
- **`FLAGS` is the no-callback escape hatch** for game-specific predicates: the mod computes any condition (quest state, line of sight, faction), stamps it as a flag bit during sync, and selects on it declaratively. Wayfinder's `IFilter` callback chain becomes mod-computed bits + data-side selection.
- **Multi-mode scanning** (the FF1–6 pattern — hotkey between "reachable items" and "all items"): either two trackers with different filters, each mode keeping its own selection, or one tracker whose filter is toggled. Both are one call per press.

Announcement data is **à-la-carte** — the mod calls only what it will speak:

```
ls_entity_vector(ctx, entity_id)
    → { nearest_point: Vec3, bearing_deg, elevation_deg, distance: f32 }
    // trivial geometry vs. current player position

ls_entity_reachable(ctx, entity_id, profile) → bool
    // may trigger the (cached) flood-fill — only pay if you announce it;
    // mods that let players browse not-yet-reachable objects never call it

ls_tracker_count(tracker, category | ALL) → u32
    // count under that tracker's filters; used for "3 of 7" and
    // category-switch announcements ("NPCs, 5 objects")
```

Composability is safe because the library never mutates between the mod's calls: a follow-up query is guaranteed consistent with the selection it follows. The full wayfinder-style announcement is three or four calls; "Shopkeeper" alone is one.

- Categories cycled = distinct category values present among that tracker's visible entities, ascending; a virtual ALL category optional via tracker config. Entities ordered by (sort_key, id).
- The mod looks up its own label table by `entity_id`, formats, and speaks. The core never says a word.

---

## 9. FFI conventions

- **Opaque handles** (`LsContext*`, path IDs). Every function's first parameter is the context (except geometry utilities, which are free functions).
- **Flat data only**: ints, floats, packed structs, caller-visible arrays. No callbacks. No strings anywhere in the ABI.
- **Buffers**: two-call pattern (call with null buffer → required count) or caller-provided buffer + capacity → written count. Library-owned results (paths) are handle-addressed and freed via `ls_path_free` / freed wholesale with the context.
- **Errors**: every function returns a status code; results via out-params. No panics across the boundary (catch_unwind at the ABI edge).
- **Versioning**: `ls_abi_version()` checked by companions at load.
- **Bindings**: `cbindgen` → C header (Unreal/C++, LuaJIT FFI); `csbindgen` or `interoptopus` → C# P/Invoke. IL2CPP note: no callbacks in the ABI means no `[MonoPInvokeCallback]` hazards at all.
- **Builds**: `x86_64-pc-windows-msvc` and `i686-pc-windows-msvc` from day one (CSTW is a 32-bit .NET Framework 4.7.2 process); Linux targets when a consumer needs them.
- **Lessons from the walking skeleton (2026-09)**: all exports are cdecl, and on 32-bit hosts bindings must declare it (`CallingConvention.Cdecl`; .NET defaults to stdcall on x86). The native library is `loadstar.dll`; no managed assembly may be named `Loadstar.dll` — Windows filenames are case-insensitive and the two clobber each other. The .NET companion is therefore `Loadstar.Net.dll`.
- **Output buffers in .NET must be marked `[Out]`**: the Framework marshaler copies struct arrays in but not back out unless told to, so a filled `LsPathLink[]` silently came back zeroed. Every caller-provided output array in `Loadstar.Net` carries `[Out]`, and the smoke harness pins `Marshal.SizeOf` of every ABI struct against the Rust `size_of` test.

---

## 10. Companion layers (per language, thin, optional)

Pure-C# first (validated against CSTW), pure-C++ later:

- Idiomatic wrapper over the P/Invoke surface (safe handles, spans).
- **Default English formatters**: direction enum → "east"; segments → "north 5, east 2"; `TrackerResult` + mod's label table → "Shopkeeper, NPC, 3 of 7, east, 4 tiles". Overridable for other languages/phrasings; the core is never involved.
- JSON persistence store for labels/custom names/hidden/links (the `CustomNameStore` pattern), keyed by entity ID.
- Speech helper (Tolk binding) — desktop Windows only, optional.
- Convenience builders: category-default application, entity-record construction, terrain-rule → edge-table baking helpers.

---

## 11. Validation roadmap

1. **Phase 1 — core + CSTW.** Implement geometry, TileGrid, entities, queries, tracker in Rust with full property tests (port the useful Wayfinder test intent, not its API). Generate C# bindings; port the CSTW mod onto them. CSTW is the hostile end of the compatibility spectrum (32-bit, .NET Framework, XNA) and its mod code should get *shorter* — the smell test that the boundary is right. Wayfinder's C# source is retired from the mod at the end of this phase.
2. **Phase 2 — second game, second language surface.** A Unity/BepInEx game exercising snapshot sync at larger entity counts; ideally one with an extractable navmesh to drive MeshWorld ingestion. Every feature both games use identically is confirmed; features only CSTW uses go on watch.
3. **Phase 3 — C++/Lua surface** against an Unreal/UE4SS target; portal-width filtering and funnel utilities if the game demands them.

Wayfinder itself is not ported wholesale. It is the prototype that discovered the requirements; this spec is its distillation. Its algorithmic fixes (candidate goals, start snapping, adjacency semantics, reachability neighbor test, path compression) are the primary assets to carry over — as behavior, not as code.

## 12. Open questions

- Whether tier-3 rule-matrix grid building earns its place, or stays a companion-side helper.
- Multi-map routing helper — deferred until ≥2 mods hand-roll it.
- Audio's eventual shape — deliberately unspecified (owner has better ideas brewing).
- MeshWorld projection details (box → triangle set with vertical tolerance) — specified when phase 2's game is chosen.
