using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace Loadstar;

/// <summary>Mirrors <c>LsVec3</c>. Tile convention: X = column, Z = row, Y = layer.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct Vec3
{
    public float X, Y, Z;

    public Vec3(float x, float y, float z) { X = x; Y = y; Z = z; }

    /// <summary>Center of a tile cell.</summary>
    public static Vec3 Cell(uint col, uint row, uint layer = 0) => new(col + 0.5f, layer, row + 0.5f);

    public override string ToString() => $"({X}, {Y}, {Z})";
}

/// <summary>Mirrors <c>LsStatus</c>.</summary>
public enum Status
{
    Ok = 0,
    NullPointer = 1,
    InvalidArgument = 2,
    BufferTooSmall = 3,
    NoPath = 4,
    Panic = 100,
}

/// <summary>Mirrors <c>LsDir</c> discriminants: the edge-mask bit index.</summary>
public enum Dir : byte
{
    South = 0,
    North = 1,
    East = 2,
    West = 3,
}

/// <summary>Mirrors <c>LsTileEdgeOverride</c>.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct TileEdgeOverride
{
    public uint Node;
    public Dir Dir;
    public uint TargetLayer;
    public float Cost;

    public TileEdgeOverride(uint node, Dir dir, uint targetLayer, float cost)
    {
        Node = node; Dir = dir; TargetLayer = targetLayer; Cost = cost;
    }
}

/// <summary>Mirrors <c>LsLinkFlags</c>.</summary>
[Flags]
public enum LinkFlags : uint
{
    None = 0,
    Bidirectional = 1,
    StartDisabled = 2,
}

/// <summary>Mirrors <c>LsLinkDesc</c>. Endpoints are positions, snapped to their nodes.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct LinkDesc
{
    public ulong Id;
    public Vec3 From;
    public Vec3 To;
    public float Cost;
    public LinkFlags Flags;

    public LinkDesc(ulong id, Vec3 from, Vec3 to, float cost = 1f, LinkFlags flags = LinkFlags.None)
    {
        Id = id; From = from; To = to; Cost = cost; Flags = flags;
    }
}

/// <summary>Mirrors <c>LsPathLink</c>: the node at <see cref="Index"/> was reached via <see cref="LinkId"/>.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct PathLink
{
    public ulong LinkId;
    public uint Index;
    private uint _reserved;
}

/// <summary>Mirrors <c>LsGoalMode</c>.</summary>
public enum GoalMode : uint
{
    /// <summary>Arrive on a target; targets are exempt from blocking.</summary>
    Onto = 0,
    /// <summary>Arrive one ordinary step away from a target; targets stay blocked.</summary>
    Adjacent = 1,
}

/// <summary>Mirrors <c>LsPathOptions</c>. <c>default</c> = nothing blocked, no snapping, Onto.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct PathOptions
{
    /// <summary>Nodes carrying any of these groups are impassable.</summary>
    public uint BlockedMask;
    /// <summary>Ring radius for start snapping when the start has no edges; 0 disables.</summary>
    public uint SnapRadius;
    public GoalMode GoalMode;

    public PathOptions(uint blockedMask, uint snapRadius = 0, GoalMode goalMode = GoalMode.Onto)
    {
        BlockedMask = blockedMask; SnapRadius = snapRadius; GoalMode = goalMode;
    }
}

/// <summary>Mirrors <c>LsBoundsKind</c>.</summary>
public enum BoundsKind : uint
{
    Point = 0,
    TileRect = 1,
    Box = 2,
    Sphere = 3,
}

/// <summary>Mirrors <c>LsBoundsDesc</c> (40 bytes). Build with the static factories.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct Bounds
{
    public const uint AllLayers = uint.MaxValue;

    public BoundsKind Kind;
    public uint Layer;
    public uint RefId;
    private uint _reserved;
    public Vec3 A;
    public Vec3 B;

    public static Bounds Point(Vec3 position) => new() { Kind = BoundsKind.Point, A = position };

    public static Bounds TileRect(uint col, uint row, uint width = 1, uint height = 1, uint layer = 0) => new()
    {
        Kind = BoundsKind.TileRect, Layer = layer, A = new Vec3(col, 0, row), B = new Vec3(width, 0, height),
    };

    public static Bounds Box(Vec3 center, Vec3 halfExtents) => new() { Kind = BoundsKind.Box, A = center, B = halfExtents };

    public static Bounds Sphere(Vec3 center, float radius) => new() { Kind = BoundsKind.Sphere, A = center, B = new Vec3(radius, 0, 0) };
}

/// <summary>Mirrors <c>LsEntityFlags</c>. Other bits are yours to define.</summary>
[Flags]
public enum EntityFlags : uint
{
    None = 0,
    /// <summary>Excluded by the default tracker filter. Convention only.</summary>
    Hidden = 1,
    /// <summary>Paths stop next to this entity instead of on it (NPCs vs. exits).</summary>
    PathfindAdjacent = 2,
}

/// <summary>Mirrors <c>LsEntityDesc</c> (64 bytes). All numeric — keep labels in your own table keyed by <see cref="Id"/>.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct EntityDesc
{
    public ulong Id;
    public ushort Category;
    private ushort _reserved;
    public uint Blocking;
    public EntityFlags Flags;
    public uint SortKey;
    public Bounds Bounds;

    public EntityDesc(ulong id, ushort category, Bounds bounds, uint blocking = 0, EntityFlags flags = EntityFlags.None, uint sortKey = 0)
    {
        Id = id; Category = category; _reserved = 0; Blocking = blocking; Flags = flags; SortKey = sortKey; Bounds = bounds;
    }
}

/// <summary>Mirrors <c>LsSyncStats</c>.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct SyncStats
{
    public uint Added, Updated, Removed;
    private uint _reserved;

    public override string ToString() => $"+{Added} ~{Updated} -{Removed}";
}

public sealed class LoadstarException : Exception
{
    public Status Status { get; }

    public LoadstarException(Status status, string call)
        : base($"{call} returned {status}") => Status = status;
}

/// <summary>Owns an <c>LsContext*</c>.</summary>
public sealed class ContextHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public ContextHandle() : base(ownsHandle: true) { }

    protected override bool ReleaseHandle()
    {
        Native.ls_context_destroy(handle);
        return true;
    }
}

/// <summary>Owns an <c>LsPath*</c>.</summary>
public sealed class PathHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public PathHandle() : base(ownsHandle: true) { }

    protected override bool ReleaseHandle()
    {
        Native.ls_path_free(handle);
        return true;
    }
}

/// <summary>
/// Raw P/Invoke surface, one entry per exported function in include/loadstar.h.
/// Kept mechanical so it can be regenerated; behavior lives in the wrappers.
/// All exports are cdecl; C <c>_Bool</c> parameters are marshaled as one byte.
/// </summary>
internal static class Native
{
    private const string Lib = "loadstar";
    private const CallingConvention Cc = CallingConvention.Cdecl;

    [DllImport(Lib, CallingConvention = Cc)] public static extern uint ls_abi_version();

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_vec3_add(Vec3 a, Vec3 b, out Vec3 result);

    // Contexts
    [DllImport(Lib, CallingConvention = Cc)]
    public static extern Status ls_tile_context_create(
        uint width, uint height, uint layers,
        byte[] walkable, UIntPtr walkableLen,
        out ContextHandle context);

    [DllImport(Lib, CallingConvention = Cc)]
    public static extern Status ls_tile_context_create_edges(
        uint width, uint height, uint layers,
        byte[] masks, UIntPtr masksLen,
        TileEdgeOverride[]? overrides, UIntPtr overridesLen,
        out ContextHandle context);

    [DllImport(Lib, CallingConvention = Cc)] public static extern void ls_context_destroy(IntPtr context);

    // Nodes
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_id(ContextHandle context, uint col, uint row, uint layer, out uint node);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_coords(ContextHandle context, uint node, out uint col, out uint row, out uint layer);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_position(ContextHandle context, uint node, out Vec3 position);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_from_position(ContextHandle context, Vec3 position, out uint node);

    // Edges and links
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_tile_set_edge(ContextHandle context, uint node, Dir dir, [MarshalAs(UnmanagedType.U1)] bool passable);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_tile_set_edge_override(ContextHandle context, uint node, Dir dir, uint targetLayer, float cost);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_tile_clear_edge_override(ContextHandle context, uint node, Dir dir);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_link_add(ContextHandle context, LinkDesc link);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_link_remove(ContextHandle context, ulong id);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_link_set_enabled(ContextHandle context, ulong id, [MarshalAs(UnmanagedType.U1)] bool enabled);

    // Node groups
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_get_groups(ContextHandle context, uint node, out uint mask);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_set_groups(ContextHandle context, uint node, uint mask);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_add_groups(ContextHandle context, uint node, uint mask);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_node_remove_groups(ContextHandle context, uint node, uint mask);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_groups_clear(ContextHandle context, uint mask);

    // Entities
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_entity_upsert(ContextHandle context, in EntityDesc entity);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_entity_remove(ContextHandle context, ulong id);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_entities_sync(ContextHandle context, EntityDesc[]? entities, UIntPtr entitiesLen, out SyncStats stats);
    [DllImport(Lib, CallingConvention = Cc)] public static extern UIntPtr ls_entity_count(ContextHandle context);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_entity_nodes(ContextHandle context, ulong id, [Out] uint[]? buffer, UIntPtr capacity, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_entity_nearest_point(ContextHandle context, ulong id, Vec3 from, out Vec3 point);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_find_path_to_entity(ContextHandle context, Vec3 start, ulong id, PathOptions options, out PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_entity_reachable(ContextHandle context, Vec3 start, ulong id, PathOptions options, [MarshalAs(UnmanagedType.U1)] out bool reachable);

    // Paths and reachability
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_find_path(ContextHandle context, Vec3 start, Vec3 goal, PathOptions options, out PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_find_path_to_nodes(ContextHandle context, Vec3 start, uint[]? targets, UIntPtr targetsLen, PathOptions options, out PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_reachable_nodes(ContextHandle context, Vec3 start, PathOptions options, [Out] byte[]? buffer, UIntPtr capacity, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern UIntPtr ls_path_len(PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern float ls_path_cost(PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_path_nodes(PathHandle path, [Out] uint[] buffer, UIntPtr capacity, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_path_links(PathHandle path, [Out] PathLink[]? buffer, UIntPtr capacity, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern void ls_path_free(IntPtr path);
}
