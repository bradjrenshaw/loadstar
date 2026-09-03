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

/// <summary>A path result: node ids plus which arrivals happened via a link (spec §8).</summary>
public sealed class Path
{
    /// <summary>Includes both endpoints.</summary>
    public uint[] Nodes { get; }

    /// <summary>Sparse: one entry per node that was reached by taking a link.</summary>
    public PathLink[] Links { get; }

    public float Cost { get; }

    internal Path(uint[] nodes, PathLink[] links, float cost) { Nodes = nodes; Links = links; Cost = cost; }
}

/// <summary>A tile-grid context (spec §4). Dispose releases the native handle.</summary>
public sealed class TileContext : IDisposable
{
    private readonly ContextHandle _handle;

    private TileContext(ContextHandle handle) => _handle = handle;

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

    // --- Paths and reachability ---

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

    private static Path? ReadPath(Status status, PathHandle path, string call)
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

            return new Path(nodes, links, Native.ls_path_cost(path));
        }
    }

    public void Dispose() => _handle.Dispose();
}
