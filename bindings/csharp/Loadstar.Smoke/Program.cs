using System;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading;
using Loadstar;
using Loadstar.Audio;

// Walking-skeleton smoke test: prove the Rust → C ABI → .NET Framework x86 path end to end.
// Exit code 0 on success; any mismatch or exception is a failure.

Console.WriteLine($"Process is {(Environment.Is64BitProcess ? "64" : "32")}-bit, CLR {Environment.Version}");

Library.CheckAbi();
Console.WriteLine($"ABI version {Library.AbiVersion} OK");

Expect(Marshal.SizeOf(typeof(Vec3)) == 12 && Marshal.SizeOf(typeof(TileEdgeOverride)) == 16
    && Marshal.SizeOf(typeof(LinkDesc)) == 40 && Marshal.SizeOf(typeof(PathLink)) == 16
    && Marshal.SizeOf(typeof(Bounds)) == 40 && Marshal.SizeOf(typeof(EntityDesc)) == 64 && Marshal.SizeOf(typeof(SyncStats)) == 16 && Marshal.SizeOf(typeof(PathOptions)) == 16 && Marshal.SizeOf(typeof(ProfileDesc)) == 16
    && Marshal.SizeOf(typeof(FilterDesc)) == 32 && Marshal.SizeOf(typeof(Selection)) == 16 && Marshal.SizeOf(typeof(EntityVector)) == 28 && Marshal.SizeOf(typeof(PathSegment)) == 24,
    "marshaled struct sizes match the ABI");

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

// --- Blocking groups, goal modes, snapping, reachability on a 5x3 open corridor. ---
{
    const uint NPC = 1u << 0, EXIT = 1u << 1;
    using var ctx = TileContext.FromWalkable(5, 3, 1, Enumerable.Repeat((byte)1, 15).ToArray());
    var npc = ctx.NodeId(2, 1);
    ctx.AddNodeGroups(npc, NPC);
    Expect(ctx.NodeGroups(npc) == NPC, "node group round-trips");

    Expect(ctx.FindPath(Vec3.Cell(0, 1), Vec3.Cell(4, 1))!.Nodes.Length == 5, "no blocking → straight through the NPC");
    var around = ctx.FindPath(Vec3.Cell(0, 1), Vec3.Cell(4, 1), new PathOptions(blockedMask: NPC))!;
    Expect(around.Nodes.Length == 7 && !around.Nodes.Contains(npc), "blocked NPC group → detour");

    var exit = ctx.NodeId(4, 1);
    ctx.AddNodeGroups(exit, EXIT);
    var onto = ctx.FindPath(Vec3.Cell(0, 1), Vec3.Cell(4, 1), new PathOptions(blockedMask: NPC | EXIT))!;
    Expect(onto.Nodes.Last() == exit, "Onto: path ends on the blocked exit itself");

    var talk = ctx.FindPathToNodes(Vec3.Cell(0, 1), new[] { npc }, new PathOptions(blockedMask: NPC, goalMode: GoalMode.Adjacent))!;
    var (lc, lr, _) = ctx.NodeCoords(talk.Nodes.Last());
    Expect(!talk.Nodes.Contains(npc) && Math.Abs((int)lc - 2) + Math.Abs((int)lr - 1) == 1, $"Adjacent: stops next to the NPC at ({lc},{lr})");

    var reach = ctx.ReachableNodes(Vec3.Cell(0, 0), new PathOptions(blockedMask: NPC))!;
    Expect(reach.Length == 15 && !reach[npc] && reach[exit], "reachability excludes the NPC cell, includes the far side");

    ctx.ClearGroups(NPC | EXIT);
    Expect(ctx.NodeGroups(npc) == 0 && ctx.NodeGroups(exit) == 0, "ClearGroups wipes every node");
}

{
    // Start inside a solid cell snaps to the nearest floor when asked.
    var solid = Enumerable.Repeat((byte)1, 15).ToArray();
    solid[7] = 0;
    using var ctx = TileContext.FromWalkable(5, 3, 1, solid);
    Expect(ctx.FindPath(Vec3.Cell(2, 1), Vec3.Cell(4, 1)) == null, "no snapping by default → null");
    var snapped = ctx.FindPath(Vec3.Cell(2, 1), Vec3.Cell(4, 1), new PathOptions(blockedMask: 0, snapRadius: 1))!;
    Expect(snapped.Nodes[0] != 7 && snapped.Nodes.Last() == ctx.NodeId(4, 1), "snapRadius 1 → path starts on the adjacent floor cell");
}

// --- Entities: snapshot sync, projection into blocking groups, paths and reachability. ---
{
    const uint NPC = 1u << 0, EXIT = 1u << 1;
    const ushort CatNpc = 1, CatExit = 2, CatChest = 3;
    using var ctx = TileContext.FromWalkable(5, 3, 1, Enumerable.Repeat((byte)1, 15).ToArray());

    var npc = new EntityDesc(10, CatNpc, Bounds.TileRect(2, 1), blocking: NPC, flags: EntityFlags.PathfindAdjacent);
    var exit = new EntityDesc(20, CatExit, Bounds.TileRect(4, 0, height: 3), blocking: EXIT);
    var chest = new EntityDesc(30, CatChest, Bounds.Point(Vec3.Cell(0, 2)));
    var stats = ctx.SyncEntities(new[] { npc, exit, chest });
    Expect(stats.Added == 3 && stats.Updated == 0 && stats.Removed == 0 && ctx.EntityCount == 3, $"snapshot sync: {stats}, {ctx.EntityCount} entities");
    Expect(ctx.NodeGroups(ctx.NodeId(2, 1)) == NPC && ctx.NodeGroups(ctx.NodeId(4, 2)) == EXIT, "projections populated the blocking overlay");
    Expect(ctx.EntityNodes(20).Length == 3 && ctx.EntityNodes(30).Length == 1, "1x3 exit → 3 nodes, point chest → 1 node");

    stats = ctx.SyncEntities(new[] { npc, exit, chest });
    Expect(stats.Added == 0 && stats.Updated == 0 && stats.Removed == 0, "unchanged re-send is free");

    var opts = new PathOptions(blockedMask: NPC | EXIT);
    var talk = ctx.FindPathToEntity(Vec3.Cell(0, 1), 10, opts)!;
    Expect(!talk.Nodes.Contains(ctx.NodeId(2, 1)) && talk.Nodes.Length == 2, "PathfindAdjacent entity: stop beside the NPC");
    var leave = ctx.FindPathToEntity(Vec3.Cell(0, 1), 20, opts)!;
    var (ec, _, _) = ctx.NodeCoords(leave.Nodes.Last());
    Expect(ec == 4 && !leave.Nodes.Contains(ctx.NodeId(2, 1)), "walk-onto entity: ends on the exit, detouring the NPC");
    Expect(ctx.EntityNearestPoint(20, Vec3.Cell(0, 1)).X == 4 && ctx.EntityNearestPoint(20, Vec3.Cell(0, 1)).Z == 1.5f, "nearest point on the exit's west face");
    Expect(ctx.EntityReachable(Vec3.Cell(0, 1), 30, opts), "chest is reachable");

    // NPC wanders one tile: its old cell frees up; dropping the chest removes it.
    npc.Bounds = Bounds.TileRect(2, 0);
    stats = ctx.SyncEntities(new[] { npc, exit });
    Expect(stats.Updated == 1 && stats.Removed == 1 && ctx.EntityCount == 2, $"move + drop: {stats}");
    Expect(ctx.NodeGroups(ctx.NodeId(2, 1)) == 0 && ctx.NodeGroups(ctx.NodeId(2, 0)) == NPC, "blocking overlay followed the NPC");
    Expect(ctx.FindPath(Vec3.Cell(0, 1), Vec3.Cell(3, 1), opts)!.Nodes.Length == 4, "old NPC cell is walkable again");

    try { ctx.EntityNodes(30); Expect(false, "removed entity should throw"); }
    catch (LoadstarException e) { Expect(e.Status == Status.InvalidArgument, $"removed entity → {e.Status}"); }
}

// --- Agent profiles: a 2x2 agent on a 4x3 grid with a wall at (1,1). ---
{
    var grid = Enumerable.Repeat((byte)1, 12).ToArray();
    grid[5] = 0;
    using var ctx = TileContext.FromWalkable(4, 3, 1, grid);
    Expect(ctx.NodeClearance(ctx.NodeId(2, 0)) == 2 && ctx.NodeClearance(ctx.NodeId(0, 0)) == 1 && ctx.NodeClearance(ctx.NodeId(1, 1)) == 0,
        "clearance field: 2 at (2,0), 1 at (0,0), 0 at the wall");
    var big = ctx.RegisterProfile(new ProfileDesc(footprint: 2));
    Expect(big == 1, $"registered profile id {big}");
    Expect(ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(2, 1)) != null, "one-cell agent leaves the corner");
    Expect(ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(2, 1), new PathOptions(0, profile: big)) == null, "2x2 agent cannot leave the corner");
    Expect(ctx.FindPath(Vec3.Cell(2, 0), Vec3.Cell(2, 1), new PathOptions(0, profile: big))!.Nodes.Length == 2, "2x2 agent moves between the two anchors that fit");
    try { ctx.RegisterProfile(new ProfileDesc(footprint: 0)); Expect(false, "footprint 0 should throw"); }
    catch (LoadstarException e) { Expect(e.Status == Status.InvalidArgument, $"footprint 0 → {e.Status}"); }
}

// --- Trackers: filters as data, cycling, counts, announcement geometry. ---
{
    const uint NPC = 1u << 0;
    const ushort CatNpc = 1, CatChest = 3;
    using var ctx = TileContext.FromWalkable(5, 3, 1, Enumerable.Repeat((byte)1, 15).ToArray());
    ctx.SyncEntities(new[]
    {
        new EntityDesc(10, CatNpc, Bounds.TileRect(2, 0), blocking: NPC, flags: EntityFlags.PathfindAdjacent),
        new EntityDesc(11, CatNpc, Bounds.TileRect(2, 1), blocking: NPC, flags: EntityFlags.PathfindAdjacent),
        new EntityDesc(12, CatNpc, Bounds.TileRect(2, 2), blocking: NPC, flags: EntityFlags.PathfindAdjacent),
        new EntityDesc(30, CatChest, Bounds.TileRect(4, 0), sortKey: 1),
        new EntityDesc(31, CatChest, Bounds.TileRect(1, 2), sortKey: 0),
        new EntityDesc(32, CatChest, Bounds.TileRect(4, 2), flags: EntityFlags.Hidden),
    });
    ctx.SetPlayer(Vec3.Cell(0, 1));

    var everything = ctx.CreateTracker();
    Expect(ctx.TrackerCount(everything) == 6, "no filters → zero built-in rules, hidden chest included");

    var tracker = ctx.CreateTracker(new[]
    {
        FilterDesc.Flags(require: EntityFlags.None, forbid: EntityFlags.Hidden),
        FilterDesc.Reachable(profile: 0, blockedMask: NPC, enabled: false),
    });
    Expect(ctx.TrackerCount(tracker) == 5 && ctx.TrackerCount(tracker, CatChest) == 2, "hidden filtered; counts per category");

    var s = ctx.TrackerAct(tracker, TrackerAction.Next);
    Expect(s.EntityId == 10 && s.Category == CatNpc && s.Index == 0, $"first in ALL view: {s}");
    ctx.TrackerAct(tracker, TrackerAction.Next); ctx.TrackerAct(tracker, TrackerAction.Next);
    s = ctx.TrackerAct(tracker, TrackerAction.Next);
    Expect(s.EntityId == 31, $"chests follow NPCs, sort_key 0 first: {s}");
    Expect(ctx.TrackerAct(tracker, TrackerAction.Prev).EntityId == 12, "Prev goes back");

    s = ctx.TrackerAct(tracker, TrackerAction.CategoryNext);
    Expect(s.IsNone && s.Category == CatNpc, $"category cycle clears the selection: {s}");
    s = ctx.TrackerAct(tracker, TrackerAction.CategoryNext);
    Expect(s.Category == CatChest && ctx.TrackerAct(tracker, TrackerAction.Next).EntityId == 31, "second category is chests");

    ctx.SetTrackerFilterEnabled(tracker, 1, true);
    Expect(ctx.TrackerCount(tracker, CatChest) == 1, "reachability filter on: chest 30 behind the NPC wall drops out");
    Expect(!ctx.EntityReachableFromPlayer(30, 0, NPC) && ctx.EntityReachableFromPlayer(11, 0, NPC), "cached reachability agrees: chest 30 no, NPC 11 yes (adjacent)");

    var v = ctx.EntityVector(30);
    Expect(v.NearestPoint.X == 4 && v.NearestPoint.Z == 1 && v.Steps == 4 && v.BearingDeg > 0 && v.BearingDeg < 90, $"vector to chest 30: bearing {v.BearingDeg:F1}°, {v.Steps} steps");

    var nearby = ctx.CreateTracker(new[] { FilterDesc.Range(1.5f) });
    Expect(ctx.TrackerCount(nearby) == 2, "range 1.5: NPC 11 and chest 31 only");
    var chestsOnly = ctx.CreateTracker(new[] { FilterDesc.Categories(1ul << CatChest) }, includeAll: false);
    Expect(ctx.TrackerAct(chestsOnly, TrackerAction.Next).Category == CatChest, "category-restricted tracker starts in its category");

    ctx.DestroyTracker(tracker);
    try { ctx.TrackerAct(tracker, TrackerAction.Next); Expect(false, "destroyed tracker should throw"); }
    catch (LoadstarException e) { Expect(e.Status == Status.InvalidArgument, $"destroyed tracker → {e.Status}"); }
}

// --- A context without a map: a scanner over world coordinates, nearest first. ---
{
    const ushort CatChest = 3, CatNpc = 1;
    using var ctx = Context.Create();
    ctx.SyncEntities(new[]
    {
        new EntityDesc(1, CatChest, Bounds.Point(new Vec3(-20, 0, -10))),
        new EntityDesc(2, CatNpc, Bounds.Box(new Vec3(-12, 0, -10), new Vec3(0.5f, 0, 0.5f)), blocking: 1),
        new EntityDesc(3, CatChest, Bounds.Point(new Vec3(-10, 0, -4))),
    });
    ctx.SetPlayer(new Vec3(-10, 0, -10));
    Expect(ctx.EntityNodes(2).Length == 0, "no map: entities project to nothing");

    var scan = ctx.CreateTracker();
    ctx.SetTrackerOrder(scan, TrackerOrder.Distance);
    var order = new[] { 0, 1, 2 }.Select(_ => ctx.TrackerAct(scan, TrackerAction.Next).EntityId).ToArray();
    Expect(order.SequenceEqual(new ulong[] { 2, 3, 1 }), "distance order: " + string.Join(", ", order));
    ctx.SetPlayer(new Vec3(-20, 0, -10));
    ctx.SetTrackerCategory(scan, Selection.AllCategories);
    Expect(ctx.TrackerAct(scan, TrackerAction.Next).EntityId == 1, "re-sorted after the player moves");
    ctx.SetTrackerCategory(scan, CatChest);
    var chests = new[] { 0, 1 }.Select(_ => ctx.TrackerAct(scan, TrackerAction.Next).EntityId).ToArray();
    Expect(chests.SequenceEqual(new ulong[] { 1, 3 }), "category jump keeps distance order: " + string.Join(", ", chests));

    var v = ctx.EntityVector(1);
    Expect(v.Distance == 0, $"standing on chest 1: {v.Distance}");

    // Group browsing: the two chests are one group, the NPC its own.
    ctx.SyncEntities(new[]
    {
        new EntityDesc(1, CatChest, Bounds.Point(new Vec3(-20, 0, -10)), group: 4),
        new EntityDesc(2, CatNpc, Bounds.Box(new Vec3(-12, 0, -10), new Vec3(0.5f, 0, 0.5f)), blocking: 1),
        new EntityDesc(3, CatChest, Bounds.Point(new Vec3(-10, 0, -4)), group: 4),
    });
    ctx.SetTrackerCategory(scan, Selection.AllCategories);
    var g = ctx.TrackerAct(scan, TrackerAction.GroupNext);
    Expect(g.EntityId == 1 && ctx.TrackerGroupCount(scan) == 2, $"nearest group first, on its nearest member: {g}");
    g = ctx.TrackerAct(scan, TrackerAction.InGroupNext);
    Expect(g.EntityId == 3 && g.Index == 1, $"within the group: {g}");
    Expect(ctx.TrackerAct(scan, TrackerAction.GroupNext).EntityId == 2 && ctx.TrackerGroupCount(scan) == 1, "ungrouped NPC is a group of one");
    try { ctx.TrackerCount(ctx.CreateTracker(new[] { FilterDesc.Reachable(0, 0) })); }
    catch (LoadstarException) { Expect(false, "reachability filter must not throw on a context without a map"); }
}

// --- Segments and the companion's default phrasing. ---
{
    var line = Enumerable.Repeat((byte)1, 10).ToArray();
    line[5] = 0;
    using var ctx = TileContext.FromWalkable(10, 1, 1, line);
    ctx.AddLink(new LinkDesc(id: 42, Vec3.Cell(2, 0), Vec3.Cell(7, 0)));
    var p = ctx.FindPath(Vec3.Cell(0, 0), Vec3.Cell(9, 0))!;
    Expect(p.Segments.Length == 3 && p.Segments[1].Kind == SegmentKind.Link && p.Segments[1].LinkId == 42 && p.Segments[2].Count == 2,
        "segments: " + string.Join(" | ", p.Segments));
    var words = Phrases.Directions(p.Segments, id => id == 42 ? "the blue portal" : null);
    Expect(words == "east 2, take the blue portal, east 2", $"directions: \"{words}\"");
    Expect(Phrases.Directions(ctx.FindPath(Vec3.Cell(3, 0), Vec3.Cell(3, 0))!.Segments) == "Already there", "zero-length path phrase");

    ctx.SyncEntities(new[] { new EntityDesc(1, 1, Bounds.TileRect(4, 0)) });
    ctx.SetPlayer(Vec3.Cell(0, 0));
    var v = ctx.EntityVector(1);
    var announce = Phrases.Announce("Chest", "Chest", new Selection { EntityId = 1, Category = 1, Index = 2 }, 7, v);
    Expect(announce == "Chest, Chest, 3 of 7, east, 4 tiles", $"announcement: \"{announce}\"");
    Expect(Phrases.Compass(20) == "north" && Phrases.Compass(45) == "northeast" && Phrases.Compass(120) == "southeast" && Phrases.Compass(300) == "northwest" && Phrases.Compass(350) == "north",
        "compass sectors use the 2:1 cardinal rule");
    Expect(Phrases.CategorySwitch("NPCs", 1) == "NPCs, 1 object", "category switch phrase");
}

// --- Audio through the ABI: null-device math, then a short real WASAPI ping. ---
{
    Expect(Marshal.SizeOf(typeof(Loadstar.Audio.SourceDesc)) == 96 && Marshal.SizeOf(typeof(EffectDesc)) == 40
        && Marshal.SizeOf(typeof(ChainTarget)) == 24 && Marshal.SizeOf(typeof(DeviceConfig)) == 16
        && Marshal.SizeOf(typeof(StreamInfo)) == 16 && Marshal.SizeOf(typeof(Finished)) == 16, "audio struct sizes match the ABI");

    using (var dev = AudioDevice.OpenNull(48000, 256))
    {
        Expect(dev.Info.SampleRate == 48000 && dev.Info.PeriodFrames == 256 && !dev.Info.LowLatency, $"null device: {dev.Info}");
        var space = dev.CreateSpace(SpaceMode.Planar);
        var tone = dev.Tone(440, 500);
        dev.SetListener(space, default, new Vec3(0, 0, -1), new Vec3(0, 1, 0));
        dev.Sync(space, new[] { Loadstar.Audio.SourceDesc.Buffer_(1, tone, Bounds.Point(new Vec3(4, 0, 0))).Looping() });
        var buf = new float[512];
        float l = 0, r = 0;
        for (int i = 0; i < 6; i++)
        {
            dev.Render(buf);
            for (int k = 0; k < buf.Length; k += 2) { l += buf[k] * buf[k]; r += buf[k + 1] * buf[k + 1]; }
        }
        Expect(r > l * 2, $"null device: a source to the east is heard on the right (L {l:F1}, R {r:F1})");

        var walls = dev.CreateBus(space, 0.5f);
        dev.SetChain(ChainTarget.ForBus(space, walls), new[] { EffectDesc.LowPass(200) });
        dev.SetEffectEnabled(ChainTarget.ForBus(space, walls), 0, false);
        dev.Fire(space, Loadstar.Audio.SourceDesc.Buffer_(77, tone, Bounds.Point(new Vec3(0, 0, -1))).Repeats(1, 0));
        for (int i = 0; i < 120; i++) dev.Render(buf);
        var done = dev.TakeFinished();
        Expect(done.Length == 1 && done[0].Id == 77 && done[0].Space == space, "fired one-shot reported finished");
        Expect(dev.TakeFinished().Length == 0, "finished list drains");
    }

    try
    {
        using var dev = AudioDevice.Open();
        Console.WriteLine($"  WASAPI: {dev.Info}");
        var space = dev.CreateSpace(SpaceMode.Planar);
        var ping = dev.Tone(880, 120, 5, 40);
        dev.SetListener(space, default, new Vec3(0, 0, -1), new Vec3(0, 1, 0));
        dev.Fire(space, Loadstar.Audio.SourceDesc.Buffer_(5, ping, Bounds.Point(new Vec3(3, 0, 0))).Repeats(3, 150).WithGain(0.4f));
        Thread.Sleep(900);
        Expect(Array.Exists(dev.TakeFinished(), f => f.Id == 5), "WASAPI: three pings on the right played and finished");
    }
    catch (LoadstarException e) when (e.Status == Status.DeviceUnavailable)
    {
        Console.WriteLine("  (no audio device: WASAPI check skipped)");
    }
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
