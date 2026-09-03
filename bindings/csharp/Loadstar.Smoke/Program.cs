using System;
using System.Linq;
using Loadstar;

// Walking-skeleton smoke test: prove the Rust → C ABI → .NET Framework x86 path end to end.
// Exit code 0 on success; any mismatch or exception is a failure.

Console.WriteLine($"Process is {(Environment.Is64BitProcess ? "64" : "32")}-bit, CLR {Environment.Version}");

Library.CheckAbi();
Console.WriteLine($"ABI version {Library.AbiVersion} OK");

var sum = Library.Add(new Vec3(1, 2, 3), new Vec3(10, 20, 30));
Expect(sum.X == 11 && sum.Y == 22 && sum.Z == 33, $"vec3 add by value: {sum}");

// 10x10 grid with a wall down column 5, open only at the bottom row.
const uint W = 10, H = 10;
var cells = Enumerable.Repeat((byte)1, (int)(W * H)).ToArray();
for (int row = 0; row < 9; row++) cells[row * W + 5] = 0;

using var ctx = TileContext.FromWalkable(W, H, 1, cells);

var path = ctx.FindPath(new Vec3(0.5f, 0, 0.5f), new Vec3(9.5f, 0, 0.5f));
Expect(path != null, "path exists around the wall");
// Down 9, across 5, up 9 (or equivalent) = 27 steps → 28 nodes, must pass through the gap at (5, 9).
Expect(path!.Length == 28, $"path length {path.Length} == 28");
var gap = ctx.NodePosition(9 * W + 5);
Expect(path.Any(n => n == 9 * W + 5), $"path passes through gap {gap}");
Console.WriteLine("Path: " + string.Join(" ", path.Select(n => { var p = ctx.NodePosition(n); return $"({(int)p.X},{(int)p.Z})"; })));

Expect(ctx.FindPath(new Vec3(0.5f, 0, 0.5f), new Vec3(5.5f, 0, 0.5f)) == null, "wall cell is unreachable → null");

try
{
    TileContext.FromWalkable(3, 3, 1, new byte[4]);
    Expect(false, "bad grid should throw");
}
catch (LoadstarException e)
{
    Expect(e.Status == Status.InvalidArgument, $"bad grid → {e.Status}");
}

Console.WriteLine("ALL OK");
return 0;

static void Expect(bool ok, string what)
{
    Console.WriteLine($"  [{(ok ? "ok" : "FAIL")}] {what}");
    if (!ok) Environment.Exit(1);
}
