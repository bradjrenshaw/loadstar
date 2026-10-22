using System;

namespace Loadstar;

public static class Library
{
    /// <summary>ABI major this companion was written against.</summary>
    public const uint ExpectedAbiVersion = 1;

    public static uint AbiVersion => Native.ls_abi_version();

    /// <summary>Throws if the loaded native library speaks a different ABI.</summary>
    public static void CheckAbi()
    {
        var actual = AbiVersion;
        if (actual != ExpectedAbiVersion)
            throw new InvalidOperationException(
                $"loadstar native ABI {actual} does not match companion ABI {ExpectedAbiVersion}");
    }

    public static Vec3 Add(Vec3 a, Vec3 b)
    {
        Native.ls_vec3_add(a, b, out var r).ThrowIfError(nameof(Native.ls_vec3_add));
        return r;
    }

    internal static void ThrowIfError(this Status status, string call)
    {
        if (status != Status.Ok) throw new LoadstarException(status, call);
    }

    internal static void ThrowIfNot(this Status status, Status a, Status b, string call)
    {
        if (status != a && status != b) throw new LoadstarException(status, call);
    }
}

/// <summary>A path result: node ids, link traversals, and the guidance segments (spec §8).</summary>
public sealed class Path
{
    /// <summary>Includes both endpoints.</summary>
    public uint[] Nodes { get; }

    /// <summary>Sparse: one entry per node that was reached by taking a link.</summary>
    public PathLink[] Links { get; }

    /// <summary>Direction runs broken at link traversals — "north 5, east 2" as data. Empty when already there.</summary>
    public PathSegment[] Segments { get; }

    public float Cost { get; }

    internal Path(uint[] nodes, PathLink[] links, PathSegment[] segments, float cost)
    {
        Nodes = nodes; Links = links; Segments = segments; Cost = cost;
    }
}

/// <summary>
/// A context (spec §4): entities, the player, entity vectors and trackers, plus a map in the
/// <see cref="TileContext"/> subclass. <see cref="Create"/> makes one without a map, for spaces the
/// library doesn't model: everything here works, entities project to no nodes, and nothing is
/// reachable. Dispose releases the native handle.
/// </summary>
public class Context : IDisposable
{
    private protected readonly ContextHandle _handle;

    private protected Context(ContextHandle handle) => _handle = handle;

    /// <summary>A context without a map.</summary>
    public static Context Create()
    {
        Native.ls_context_create(out var h).ThrowIfError(nameof(Native.ls_context_create));
        return new Context(h);
    }

    // --- Entities ---

    /// <summary>Insert or replace one entity by id.</summary>
    public void UpsertEntity(in EntityDesc entity) =>
        Native.ls_entity_upsert(_handle, entity).ThrowIfError(nameof(Native.ls_entity_upsert));

    public void RemoveEntity(ulong id) =>
        Native.ls_entity_remove(_handle, id).ThrowIfError(nameof(Native.ls_entity_remove));

    /// <summary>
    /// Declarative snapshot sync: send everything you see each frame; the library diffs,
    /// re-projects only what changed, and removes entities absent from the snapshot.
    /// </summary>
    public SyncStats SyncEntities(EntityDesc[] entities)
    {
        Native.ls_entities_sync(_handle, entities.Length == 0 ? null : entities, (UIntPtr)entities.Length, out var stats)
            .ThrowIfError(nameof(Native.ls_entities_sync));
        return stats;
    }

    public int EntityCount => (int)(ulong)Native.ls_entity_count(_handle);

    /// <summary>Nodes the entity occupies; empty if it lies off-grid.</summary>
    public uint[] EntityNodes(ulong id)
    {
        Native.ls_entity_nodes(_handle, id, null, UIntPtr.Zero, out var needed)
            .ThrowIfNot(Status.BufferTooSmall, Status.Ok, nameof(Native.ls_entity_nodes));
        var nodes = new uint[(int)(ulong)needed];
        if (nodes.Length > 0)
            Native.ls_entity_nodes(_handle, id, nodes, (UIntPtr)nodes.Length, out _).ThrowIfError(nameof(Native.ls_entity_nodes));
        return nodes;
    }

    /// <summary>Closest point of the entity's bounds to <paramref name="from"/> (pure geometry).</summary>
    public Vec3 EntityNearestPoint(ulong id, Vec3 from)
    {
        Native.ls_entity_nearest_point(_handle, id, from, out var p).ThrowIfError(nameof(Native.ls_entity_nearest_point));
        return p;
    }

    // --- Player, announcement data, trackers ---

    /// <summary>Update the player's position (each frame). A real move invalidates cached reachability.</summary>
    public void SetPlayer(Vec3 position) =>
        Native.ls_player_set(_handle, position).ThrowIfError(nameof(Native.ls_player_set));

    /// <summary>Bearing, elevation, distance and step count from the player to the entity's nearest point.</summary>
    public EntityVector EntityVector(ulong id)
    {
        Native.ls_entity_vector(_handle, id, out var v).ThrowIfError(nameof(Native.ls_entity_vector));
        return v;
    }

    /// <summary>Create a tracker (a selection state machine) with a declarative, AND-combined filter list.
    /// The core has no built-in visibility rules — even hiding is just a <see cref="FilterDesc.Flags"/> filter.</summary>
    public uint CreateTracker(FilterDesc[]? filters = null, bool includeAll = true)
    {
        var count = filters?.Length ?? 0;
        Native.ls_tracker_create(_handle, count == 0 ? null : filters, (UIntPtr)count, includeAll, out var id)
            .ThrowIfError(nameof(Native.ls_tracker_create));
        return id;
    }

    public void DestroyTracker(uint tracker) =>
        Native.ls_tracker_destroy(_handle, tracker).ThrowIfError(nameof(Native.ls_tracker_destroy));

    public void SetTrackerFilters(uint tracker, FilterDesc[]? filters)
    {
        var count = filters?.Length ?? 0;
        Native.ls_tracker_set_filters(_handle, tracker, count == 0 ? null : filters, (UIntPtr)count)
            .ThrowIfError(nameof(Native.ls_tracker_set_filters));
    }

    /// <summary>Enable or disable one filter by index — the hotkey-toggle primitive.</summary>
    public void SetTrackerFilterEnabled(uint tracker, int index, bool enabled) =>
        Native.ls_tracker_set_filter_enabled(_handle, tracker, (UIntPtr)index, enabled).ThrowIfError(nameof(Native.ls_tracker_set_filter_enabled));

    /// <summary>Set the tracker's cycle order (<see cref="TrackerOrder.Distance"/> = nearest first, the scanner order).</summary>
    public void SetTrackerOrder(uint tracker, TrackerOrder order) =>
        Native.ls_tracker_set_order(_handle, tracker, order).ThrowIfError(nameof(Native.ls_tracker_set_order));

    /// <summary>Jump the tracker to a category (<see cref="Selection.AllCategories"/> = the all-categories view)
    /// without cycling; clears the selection. A category with nothing visible just lists nothing.</summary>
    public void SetTrackerCategory(uint tracker, uint category) =>
        Native.ls_tracker_set_category(_handle, tracker, category).ThrowIfError(nameof(Native.ls_tracker_set_category));

    public Selection TrackerAct(uint tracker, TrackerAction action)
    {
        Native.ls_tracker_act(_handle, tracker, action, out var s).ThrowIfError(nameof(Native.ls_tracker_act));
        return s;
    }

    /// <summary>Size of the selected entity's group in the tracker's current view; 0 when nothing is selected.</summary>
    public uint TrackerGroupCount(uint tracker)
    {
        Native.ls_tracker_group_count(_handle, tracker, out var n).ThrowIfError(nameof(Native.ls_tracker_group_count));
        return n;
    }

    /// <summary>Visible entity count under the tracker's filters, in one category or all.</summary>
    public uint TrackerCount(uint tracker, uint category = Selection.AllCategories)
    {
        Native.ls_tracker_count(_handle, tracker, category, out var n).ThrowIfError(nameof(Native.ls_tracker_count));
        return n;
    }

    public void Dispose() => _handle.Dispose();
}

/// <summary>A tile-grid context (spec §4): a <see cref="Context"/> with a grid map, so entities project into
/// nodes and paths and reachability work.</summary>
public sealed class TileContext : Context
{
    private TileContext(ContextHandle handle) : base(handle) { }

    /// <summary>
    /// Build from per-cell walkability: <c>width * height * layers</c> bytes, layer-major then
    /// row-major (<c>index = layer * width * height + row * width + col</c>); nonzero = walkable.
    /// </summary>
    public static TileContext FromWalkable(uint width, uint height, uint layers, byte[] walkable)
    {
        Native.ls_tile_context_create(width, height, layers, walkable, (UIntPtr)walkable.Length, out var h)
            .ThrowIfError(nameof(Native.ls_tile_context_create));
        return new TileContext(h);
    }

    /// <summary>
    /// Build from explicit directed edge masks (same layout as <see cref="FromWalkable"/>; bit index =
    /// <see cref="Dir"/>) plus the edges that change layer or cost.
    /// </summary>
    public static TileContext FromEdges(uint width, uint height, uint layers, byte[] masks, TileEdgeOverride[]? overrides = null)
    {
        var count = overrides?.Length ?? 0;
        Native.ls_tile_context_create_edges(width, height, layers, masks, (UIntPtr)masks.Length,
                count == 0 ? null : overrides, (UIntPtr)count, out var h)
            .ThrowIfError(nameof(Native.ls_tile_context_create_edges));
        return new TileContext(h);
    }

    // --- Nodes ---

    public uint NodeId(uint col, uint row, uint layer = 0)
    {
        Native.ls_node_id(_handle, col, row, layer, out var id).ThrowIfError(nameof(Native.ls_node_id));
        return id;
    }

    public (uint Col, uint Row, uint Layer) NodeCoords(uint node)
    {
        Native.ls_node_coords(_handle, node, out var c, out var r, out var l).ThrowIfError(nameof(Native.ls_node_coords));
        return (c, r, l);
    }

    public Vec3 NodePosition(uint node)
    {
        Native.ls_node_position(_handle, node, out var p).ThrowIfError(nameof(Native.ls_node_position));
        return p;
    }

    public uint NodeFromPosition(Vec3 position)
    {
        Native.ls_node_from_position(_handle, position, out var id).ThrowIfError(nameof(Native.ls_node_from_position));
        return id;
    }

    // --- Edges and links ---

    /// <summary>Open or close one directed edge; the reverse edge is untouched.</summary>
    public void SetEdge(uint node, Dir dir, bool passable) =>
        Native.ls_tile_set_edge(_handle, node, dir, passable).ThrowIfError(nameof(Native.ls_tile_set_edge));

    /// <summary>Make one directed edge change layer and/or cost (ramps, terrain).</summary>
    public void SetEdgeOverride(uint node, Dir dir, uint targetLayer, float cost = 1f) =>
        Native.ls_tile_set_edge_override(_handle, node, dir, targetLayer, cost).ThrowIfError(nameof(Native.ls_tile_set_edge_override));

    public void ClearEdgeOverride(uint node, Dir dir) =>
        Native.ls_tile_clear_edge_override(_handle, node, dir).ThrowIfError(nameof(Native.ls_tile_clear_edge_override));

    /// <summary>Add a link, or replace the one with the same id.</summary>
    public void AddLink(LinkDesc link) =>
        Native.ls_link_add(_handle, link).ThrowIfError(nameof(Native.ls_link_add));

    public void RemoveLink(ulong id) =>
        Native.ls_link_remove(_handle, id).ThrowIfError(nameof(Native.ls_link_remove));

    public void SetLinkEnabled(ulong id, bool enabled) =>
        Native.ls_link_set_enabled(_handle, id, enabled).ThrowIfError(nameof(Native.ls_link_set_enabled));

    // --- Profiles ---

    /// <summary>Register an agent size; use the returned id in <see cref="PathOptions.Profile"/>.</summary>
    public uint RegisterProfile(ProfileDesc profile)
    {
        Native.ls_profile_register(_handle, profile, out var id).ThrowIfError(nameof(Native.ls_profile_register));
        return id;
    }

    /// <summary>Largest square footprint that fits anchored at the node (0 = wall).</summary>
    public uint NodeClearance(uint node)
    {
        Native.ls_node_clearance(_handle, node, out var c).ThrowIfError(nameof(Native.ls_node_clearance));
        return c;
    }

    // --- Node groups (blocking overlay) ---

    public uint NodeGroups(uint node)
    {
        Native.ls_node_get_groups(_handle, node, out var mask).ThrowIfError(nameof(Native.ls_node_get_groups));
        return mask;
    }

    public void SetNodeGroups(uint node, uint mask) =>
        Native.ls_node_set_groups(_handle, node, mask).ThrowIfError(nameof(Native.ls_node_set_groups));

    public void AddNodeGroups(uint node, uint mask) =>
        Native.ls_node_add_groups(_handle, node, mask).ThrowIfError(nameof(Native.ls_node_add_groups));

    public void RemoveNodeGroups(uint node, uint mask) =>
        Native.ls_node_remove_groups(_handle, node, mask).ThrowIfError(nameof(Native.ls_node_remove_groups));

    /// <summary>Clear <paramref name="mask"/> bits from every node.</summary>
    public void ClearGroups(uint mask) =>
        Native.ls_groups_clear(_handle, mask).ThrowIfError(nameof(Native.ls_groups_clear));

    // --- Paths and reachability ---

    /// <summary>Path to an entity; goal mode comes from its <see cref="EntityFlags.PathfindAdjacent"/> flag.
    /// Null if unreachable.</summary>
    public Path? FindPathToEntity(Vec3 start, ulong id, PathOptions options = default)
    {
        var status = Native.ls_find_path_to_entity(_handle, start, id, options, out var path);
        return ReadPath(status, path, nameof(Native.ls_find_path_to_entity));
    }

    /// <summary>Whether any of the entity's nodes, or any node one step from them, is reachable
    /// from <paramref name="start"/>. Runs a fresh flood fill each call.</summary>
    public bool EntityReachable(Vec3 start, ulong id, PathOptions options = default)
    {
        var status = Native.ls_entity_reachable(_handle, start, id, options, out var reachable);
        if (status == Status.NoPath) return false;
        status.ThrowIfError(nameof(Native.ls_entity_reachable));
        return reachable;
    }

    /// <summary>Cached reachability of an entity from the player — cheap to call per announcement.</summary>
    public bool EntityReachableFromPlayer(ulong id, uint profile = 0, uint blockedMask = 0)
    {
        var status = Native.ls_entity_reachable_from_player(_handle, id, profile, blockedMask, out var reachable);
        if (status == Status.NoPath) return false;
        status.ThrowIfError(nameof(Native.ls_entity_reachable_from_player));
        return reachable;
    }

    /// <summary>Path from the node containing <paramref name="start"/> to the one containing
    /// <paramref name="goal"/>, or null if no route exists.</summary>
    public Path? FindPath(Vec3 start, Vec3 goal, PathOptions options = default)
    {
        var status = Native.ls_find_path(_handle, start, goal, options, out var path);
        return ReadPath(status, path, nameof(Native.ls_find_path));
    }

    /// <summary>Path to the cheapest of <paramref name="targets"/> (node ids), interpreted per
    /// <see cref="PathOptions.GoalMode"/>; null if none is reachable.</summary>
    public Path? FindPathToNodes(Vec3 start, uint[] targets, PathOptions options = default)
    {
        var status = Native.ls_find_path_to_nodes(_handle, start, targets, (UIntPtr)targets.Length, options, out var path);
        return ReadPath(status, path, nameof(Native.ls_find_path_to_nodes));
    }

    /// <summary>Reachability from <paramref name="start"/>, indexed by node id; null if the start
    /// is off-grid and cannot be snapped.</summary>
    public bool[]? ReachableNodes(Vec3 start, PathOptions options = default)
    {
        var status = Native.ls_reachable_nodes(_handle, start, options, null, UIntPtr.Zero, out var needed);
        if (status == Status.NoPath) return null;
        var bytes = new byte[(int)(ulong)needed];
        Native.ls_reachable_nodes(_handle, start, options, bytes, (UIntPtr)bytes.Length, out _)
            .ThrowIfError(nameof(Native.ls_reachable_nodes));
        var result = new bool[bytes.Length];
        for (int i = 0; i < bytes.Length; i++) result[i] = bytes[i] != 0;
        return result;
    }

    private Path? ReadPath(Status status, PathHandle path, string call)
    {
        if (status == Status.NoPath) return null;
        status.ThrowIfError(call);

        using (path)
        {
            var nodes = new uint[(int)(ulong)Native.ls_path_len(path)];
            Native.ls_path_nodes(path, nodes, (UIntPtr)nodes.Length, out _)
                .ThrowIfError(nameof(Native.ls_path_nodes));

            Native.ls_path_links(path, null, UIntPtr.Zero, out var needed);
            var links = new PathLink[(int)(ulong)needed];
            if (links.Length > 0)
                Native.ls_path_links(path, links, (UIntPtr)links.Length, out _)
                    .ThrowIfError(nameof(Native.ls_path_links));

            Native.ls_path_segments(_handle, path, null, UIntPtr.Zero, out needed);
            var segments = new PathSegment[(int)(ulong)needed];
            if (segments.Length > 0)
                Native.ls_path_segments(_handle, path, segments, (UIntPtr)segments.Length, out _)
                    .ThrowIfError(nameof(Native.ls_path_segments));

            return new Path(nodes, links, segments, Native.ls_path_cost(path));
        }
    }

}
