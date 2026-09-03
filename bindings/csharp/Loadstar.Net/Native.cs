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
/// </summary>
internal static class Native
{
    private const string Lib = "loadstar";

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern uint ls_abi_version();

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern Status ls_vec3_add(Vec3 a, Vec3 b, out Vec3 result);

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)]
    public static extern Status ls_tile_context_create(
        uint width, uint height, uint layers,
        byte[] walkable, UIntPtr walkableLen,
        out ContextHandle context);

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern void ls_context_destroy(IntPtr context);

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern Status ls_node_position(ContextHandle context, uint node, out Vec3 position);

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern Status ls_find_path(ContextHandle context, Vec3 start, Vec3 goal, out PathHandle path);

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern UIntPtr ls_path_len(PathHandle path);

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern Status ls_path_nodes(PathHandle path, uint[] buffer, UIntPtr capacity, out UIntPtr written);

    [DllImport(Lib, CallingConvention = CallingConvention.Cdecl)] public static extern void ls_path_free(IntPtr path);
}
