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

    // Paths
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_find_path(ContextHandle context, Vec3 start, Vec3 goal, out PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern UIntPtr ls_path_len(PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern float ls_path_cost(PathHandle path);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_path_nodes(PathHandle path, [Out] uint[] buffer, UIntPtr capacity, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status ls_path_links(PathHandle path, [Out] PathLink[]? buffer, UIntPtr capacity, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern void ls_path_free(IntPtr path);
}
