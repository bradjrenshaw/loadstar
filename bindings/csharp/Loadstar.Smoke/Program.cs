using System;
using System.Linq;
using System.Runtime.InteropServices;
using Loadstar;

// Walking-skeleton smoke test: prove the Rust → C ABI → .NET Framework x86 path end to end.
// Exit code 0 on success; any mismatch or exception is a failure.

Console.WriteLine($"Process is {(Environment.Is64BitProcess ? "64" : "32")}-bit, CLR {Environment.Version}");

Library.CheckAbi();
Console.WriteLine($"ABI version {Library.AbiVersion} OK");

Expect(Marshal.SizeOf(typeof(Vec3)) == 12 && Marshal.SizeOf(typeof(TileEdgeOverride)) == 16
    && Marshal.SizeOf(typeof(LinkDesc)) == 40 && Marshal.SizeOf(typeof(PathLink)) == 16,
    "marshaled struct sizes match the ABI (12/16/40/16)");

var sum = Library.Add(new Vec3(1, 2, 3), new Vec3(10, 20, 30));
Expect(sum.X == 11 && sum.Y == 22 && sum.Z == 33, $"vec3 add by value: {sum}");

// --- Tier 1: walkability. 10x10 with a wall down column 5, open only at the bottom row. ---
const uint W = 10, H = 10;
var cells = Enumerable.Repeat((byte)1, (int)(W * H)).ToArray();
for (int row = 0; row < 9; row++) cells[row * W + 5] = 0;

using (var ctx = TileContext.FromWalkable(W, H, 1, cells))
{
    var path = ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(9, 0));
    Expect(path != null, "path exists around the wall");
    Expect(path!.Nodes.Length == 28 && path.Cost == 27, $"path length {path.Nodes.Length} == 28, cost {path.Cost} == 27");
    Expect(path.Nodes.Contains(ctx.NodeId(5, 9)), "path passes through the gap at (5, 9)");
    Expect(path.Links.Length == 0, "no link traversals");
    Console.WriteLine("  " + string.Join(" ", path.Nodes.Select(n => { var (c, r, _) = ctx.NodeCoords(n); return $"({c},{r})"; })));

    Expect(ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(5, 0)) == null, "wall cell is unreachable → null");

    // One-way ledge: close the edge back up from (4,9) to (4,8).
    ctx.SetEdge(ctx.NodeId(4, 9), Dir.North, false);
    Expect(ctx.FindPath(Vec3.Cell(4, 9), Vec3.Cell(4, 8))!.Nodes.Length > 2, "ledge forces a detour going back north");
    Expect(ctx.FindPath(Vec3.Cell(4, 8), Vec3.Cell(4, 9))!.Nodes.Length == 2, "ledge is still passable southward");

    // Teleporter across the wall, toggleable.
    ctx.AddLink(new LinkDesc(id: 7, Vec3.Cell(1, 0), Vec3.Cell(8, 0), cost: 1, LinkFlags.Bidirectional | LinkFlags.StartDisabled));
    Expect(ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(9, 0))!.Nodes.Length == 28, "disabled teleporter changes nothing");
    ctx.SetLinkEnabled(7, true);
    var tp = ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(9, 0))!;
    Expect(tp.Nodes.Length == 4 && tp.Cost == 3, $"teleporter path: {tp.Nodes.Length} nodes, cost {tp.Cost}");
    Expect(tp.Links.Length == 1 && tp.Links[0].LinkId == 7 && tp.Links[0].Index == 2,
        $"link traversal recorded: link {tp.Links[0].LinkId} at index {tp.Links[0].Index}");
    var back = ctx.FindPath(Vec3.Cell(9, 0), Vec3.Cell(0, 0))!;
    Expect(back.Links.Length == 1, "bidirectional link works in reverse");
    ctx.RemoveLink(7);
    Expect(ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(9, 0))!.Nodes.Length == 28, "removed teleporter changes nothing");
}

// --- Tier 2: explicit masks + a ramp override between two layers. ---
{
    const uint W2 = 4, L = 2;
    byte open = (byte)((1 << (int)Dir.East) | (1 << (int)Dir.West));
    var masks = Enumerable.Repeat(open, (int)(W2 * L)).ToArray();
    var ramp = new TileEdgeOverride(node: 1, Dir.East, targetLayer: 1, cost: 1);

    using var ctx = TileContext.FromEdges(W2, 1, L, masks, new[] { ramp });
    var up = ctx.FindPath(Vec3.Cell(0, 0, layer: 0), Vec3.Cell(3, 0, layer: 1));
    Expect(up != null && up.Nodes.Length == 4, "ramp carries the path from layer 0 to layer 1");
    var (_, _, lastLayer) = ctx.NodeCoords(up!.Nodes.Last());
    Expect(lastLayer == 1, $"final node is on layer {lastLayer}");
    Expect(ctx.FindPath(Vec3.Cell(3, 0, layer: 1), Vec3.Cell(0, 0, layer: 0)) == null, "ramp is one-way: no path back down");

    ctx.ClearEdgeOverride(1, Dir.East);
    Expect(ctx.FindPath(Vec3.Cell(0, 0, layer: 0), Vec3.Cell(3, 0, layer: 1)) == null, "without the ramp the layers are disconnected");

    ctx.SetEdgeOverride(0, Dir.East, targetLayer: 0, cost: 50);
    Expect(ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(1, 0))!.Cost == 50, "edge cost override is reflected in path cost");
}

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
