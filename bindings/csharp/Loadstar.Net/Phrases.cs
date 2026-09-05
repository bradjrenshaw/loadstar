using System;
using System.Collections.Generic;
using System.Text;

namespace Loadstar;

/// <summary>
/// Default English phrasing for the structured data the core returns. The core never
/// produces text; this is the companion's opinion, and mods can ignore it entirely or
/// swap it for their own language and style.
/// </summary>
public static class Phrases
{
    /// <summary>Half-width of a cardinal sector, degrees: atan(1/2). Matches the classic
    /// "twice as far along one axis" rule for calling a direction cardinal rather than diagonal.</summary>
    private const double CardinalHalfWidth = 26.565051177;

    public static string DirectionName(Dir dir) => dir switch
    {
        Dir.North => "north",
        Dir.South => "south",
        Dir.East => "east",
        Dir.West => "west",
        _ => dir.ToString().ToLowerInvariant(),
    };

    /// <summary>Eight-way compass word for a bearing in degrees clockwise from north.</summary>
    public static string Compass(float bearingDeg)
    {
        double b = ((bearingDeg % 360) + 360) % 360;
        if (b < CardinalHalfWidth || b > 360 - CardinalHalfWidth) return "north";
        if (Math.Abs(b - 90) < CardinalHalfWidth) return "east";
        if (Math.Abs(b - 180) < CardinalHalfWidth) return "south";
        if (Math.Abs(b - 270) < CardinalHalfWidth) return "west";
        if (b < 90) return "northeast";
        if (b < 180) return "southeast";
        if (b < 270) return "southwest";
        return "northwest";
    }

    /// <summary>"here", or the compass word toward the entity.</summary>
    public static string Direction(in EntityVector v) => v.Steps < 0.5f && v.Distance < 0.5f ? "here" : Compass(v.BearingDeg);

    /// <summary>"1 tile" / "4 tiles", from the grid step count.</summary>
    public static string Tiles(in EntityVector v) => Tiles((int)Math.Round(v.Steps));

    public static string Tiles(int n) => n == 1 ? "1 tile" : $"{n} tiles";

    public static string Objects(uint n) => n == 1 ? "1 object" : $"{n} objects";

    /// <summary>"here" or "east, 4 tiles".</summary>
    public static string Where(in EntityVector v) => Direction(v) == "here" ? "here" : $"{Direction(v)}, {Tiles(v)}";

    /// <summary>The classic browse announcement: "Shopkeeper, NPC, 3 of 7, east, 4 tiles".</summary>
    public static string Announce(string label, string category, in Selection selection, uint total, in EntityVector v) =>
        $"{label}, {category}, {selection.Index + 1} of {total}, {Where(v)}";

    /// <summary>"NPCs, 5 objects" — the category-switch announcement.</summary>
    public static string CategorySwitch(string category, uint count) => $"{category}, {Objects(count)}";

    /// <summary>
    /// Turn-by-turn directions: "north 5, east 2, take the blue portal, east 3".
    /// <paramref name="linkLabel"/> names a link by id; null falls back to "take the link".
    /// </summary>
    public static string Directions(IReadOnlyList<PathSegment> segments, Func<ulong, string?>? linkLabel = null)
    {
        if (segments.Count == 0) return "Already there";
        var sb = new StringBuilder();
        foreach (var s in segments)
        {
            if (sb.Length > 0) sb.Append(", ");
            if (s.Kind == SegmentKind.Link)
                sb.Append("take ").Append(linkLabel?.Invoke(s.LinkId) ?? "the link");
            else
                sb.Append(DirectionName(s.Dir)).Append(' ').Append(s.Count);
        }
        return sb.ToString();
    }
}
