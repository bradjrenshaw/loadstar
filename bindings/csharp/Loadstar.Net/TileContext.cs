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

    /// <summary>Node ids from the node containing <paramref name="start"/> to the one containing
    /// <paramref name="goal"/>, or null if no route exists.</summary>
    public uint[]? FindPath(Vec3 start, Vec3 goal)
    {
        var status = Native.ls_find_path(_handle, start, goal, out var path);
        if (status == Status.NoPath) return null;
        status.ThrowIfError(nameof(Native.ls_find_path));

        using (path)
        {
            var nodes = new uint[(int)(ulong)Native.ls_path_len(path)];
            Native.ls_path_nodes(path, nodes, (UIntPtr)nodes.Length, out _)
                .ThrowIfError(nameof(Native.ls_path_nodes));
            return nodes;
        }
    }

    public Vec3 NodePosition(uint node)
    {
        Native.ls_node_position(_handle, node, out var p).ThrowIfError(nameof(Native.ls_node_position));
        return p;
    }

    public void Dispose() => _handle.Dispose();
}
