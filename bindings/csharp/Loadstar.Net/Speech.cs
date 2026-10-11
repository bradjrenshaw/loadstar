using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Text;
using Microsoft.Win32.SafeHandles;

namespace Loadstar.Speech;

// Mirrors the lv_* surface in include/loadstar.h (docs/speech-spec.md). Text crosses as UTF-8:
// inbound strings are marshalled NUL-terminated by the runtime (no byte[] on the GC heap, no
// pinning here); outbound strings use the two-call buffer pattern wrapped in ReadString.

/// <summary>Leaf feature bits (<c>lv_voice_features</c>).</summary>
[Flags]
public enum SpeechFeature : ulong
{
    None = 0,
    Speak = 1UL << 0,
    /// <summary>Speech plus braille where the reader supports both.</summary>
    Output = 1UL << 1,
    Braille = 1UL << 2,
    Stop = 1UL << 3,
    Pause = 1UL << 4,
    /// <summary>The backend can say whether an utterance is still being spoken.</summary>
    IsSpeaking = 1UL << 5,
    SetRate = 1UL << 6,
    SetPitch = 1UL << 7,
    SetVolume = 1UL << 8,
    /// <summary>Speak to memory (render, positional speech).</summary>
    Render = 1UL << 9,
    Ssml = 1UL << 10,
    /// <summary>More than one utterance may be in flight at once on this voice.</summary>
    MultiInstance = 1UL << 11,
    /// <summary>The output is the user's screen reader, not a synthesizer.</summary>
    ScreenReader = 1UL << 12,
}

[Flags]
public enum ProfileFlags : uint
{
    None = 0,
    /// <summary>Fail with <see cref="Status.VoiceUnavailable"/> rather than walk up the tree.</summary>
    NoFallback = 1,
    /// <summary>Trim leading/trailing silence from rendered speech.</summary>
    TrimSilence = 2,
}

[Flags]
public enum SpeakFlags : uint
{
    None = 0,
    /// <summary>Stop what this queue is speaking, drop what it has pending, then speak this.</summary>
    Interrupt = 1,
    /// <summary>Go to the head of the queue without cutting anything.</summary>
    Front = 2,
    /// <summary>The text is SSML.</summary>
    Ssml = 4,
    /// <summary>Speech plus braille where the voice supports it.</summary>
    BrailleToo = 8,
    /// <summary>Positional only: render through the first synthesizer category's auto when the voice cannot.</summary>
    RenderElsewhere = 16,
    /// <summary>Positional only: speak unpositioned when the voice cannot render; the utterance reads as flat.</summary>
    SpeakFlat = 32,
}

public enum UtteranceState : uint
{
    Pending = 0,
    Speaking = 1,
    Done = 2,
    Cancelled = 3,
    /// <summary>Not issued by this context, or older than the state table remembers.</summary>
    Unknown = 4,
}

public enum RenderJobState : uint { Pending = 0, Done = 1, Failed = 2, Unknown = 3 }

/// <summary>Mirrors <c>LsSpeechProfile</c> (24 bytes): flat, caller-owned, passed with every call.
/// <see cref="Voice"/> 0 = the root auto (the running screen reader, else the default synthesizer voice).</summary>
[StructLayout(LayoutKind.Sequential)]
public struct Profile
{
    public ulong Voice;
    /// <summary>0..1; 0.5 = the voice's default.</summary>
    public float Rate;
    public float Pitch;
    /// <summary>0..1.</summary>
    public float Volume;
    public ProfileFlags Flags;

    public static Profile Default => new() { Voice = 0, Rate = 0.5f, Pitch = 0.5f, Volume = 1f };
    public static Profile For(ulong voice) { var p = Default; p.Voice = voice; return p; }
}

/// <summary>Mirrors <c>LsSpeechQueueOptions</c>.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct QueueOptions
{
    /// <summary>1 = sequential; 0 = unlimited overlap (synthesizer voices only — a screen reader is one channel).</summary>
    public uint MaxConcurrent;
    public uint GapMs;

    public static QueueOptions Sequential(uint gapMs = 0) => new() { MaxConcurrent = 1, GapMs = gapMs };
    public static QueueOptions Unlimited => new() { MaxConcurrent = 0, GapMs = 0 };
}

[StructLayout(LayoutKind.Sequential)]
internal struct NativeSpeechConfig
{
    public IntPtr PrismPath;
    public uint LoadPrism;
    public uint UtteranceHistory;
}

/// <summary>A category of the voice tree: a fixed key the mod localizes, a default English name, a rank.</summary>
public sealed class SpeechCategory
{
    public ulong Id { get; internal set; }
    public string Key { get; internal set; } = "";
    public string Name { get; internal set; } = "";
    public int Rank { get; internal set; }
    /// <summary>The voice id meaning "this category's best".</summary>
    public ulong Auto { get; internal set; }
    public List<SpeechVoice> Voices { get; } = new();
}

/// <summary>A leaf of the voice tree: a screen reader or an installed synthesizer voice.</summary>
public sealed class SpeechVoice
{
    public ulong Id { get; internal set; }
    public string Name { get; internal set; } = "";
    /// <summary>BCP-47 where known, else empty.</summary>
    public string Language { get; internal set; } = "";
    public ulong Category { get; internal set; }
    public SpeechFeature Features { get; internal set; }
    public bool Available { get; internal set; }
    public bool Has(SpeechFeature f) => (Features & f) == f;
}

public sealed class SpeechHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public SpeechHandle() : base(ownsHandle: true) { }
    protected override bool ReleaseHandle() { NativeSpeech.lv_destroy(handle); return true; }
}

internal static class NativeSpeech
{
    private const string Lib = "loadstar";
    private const CallingConvention Cc = CallingConvention.Cdecl;
    // UnmanagedType.LPUTF8Str (48) is absent from the netstandard2.0 reference assembly but marshalled by every
    // runtime this targets (.NET Framework 4.7+, Mono 5+, .NET Core): a NUL-terminated UTF-8 copy per call, no GC-heap array.
    private const UnmanagedType Utf8 = (UnmanagedType)48;

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_create(in NativeSpeechConfig cfg, out SpeechHandle ctx);
    [DllImport(Lib, CallingConvention = Cc)] public static extern void lv_destroy(IntPtr ctx);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_prism_version(SpeechHandle ctx, [Out] byte[]? buf, UIntPtr cap, out UIntPtr written);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_tree_refresh(SpeechHandle ctx, [MarshalAs(UnmanagedType.U1)] out bool changed);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_tree_generation(SpeechHandle ctx, out ulong generation);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_category_count(SpeechHandle ctx, out UIntPtr count);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_category_at(SpeechHandle ctx, UIntPtr index, out ulong id);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_category_key(SpeechHandle ctx, ulong id, [Out] byte[]? buf, UIntPtr cap, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_category_name(SpeechHandle ctx, ulong id, [Out] byte[]? buf, UIntPtr cap, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_category_rank(SpeechHandle ctx, ulong id, out int rank);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_category_auto(SpeechHandle ctx, ulong id, out ulong voice);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_voice_count(SpeechHandle ctx, ulong category, out UIntPtr count);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_voice_at(SpeechHandle ctx, ulong category, UIntPtr index, out ulong id);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_voice_name(SpeechHandle ctx, ulong id, [Out] byte[]? buf, UIntPtr cap, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_voice_language(SpeechHandle ctx, ulong id, [Out] byte[]? buf, UIntPtr cap, out UIntPtr written);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_voice_category(SpeechHandle ctx, ulong id, out ulong category);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_voice_features(SpeechHandle ctx, ulong id, out ulong bits);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_voice_available(SpeechHandle ctx, ulong id, [MarshalAs(UnmanagedType.U1)] out bool available);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_resolve(SpeechHandle ctx, in Profile profile, ulong required, out ulong voice, [MarshalAs(UnmanagedType.U1)] out bool fellBack, out ulong missing);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_queue_create(SpeechHandle ctx, in QueueOptions options, out uint queue);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_queue_set(SpeechHandle ctx, uint queue, in QueueOptions options);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_queue_destroy(SpeechHandle ctx, uint queue);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_speak(SpeechHandle ctx, uint queue, in Profile profile, [MarshalAs(Utf8)] string text, uint flags, out ulong utterance);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_braille(SpeechHandle ctx, uint queue, in Profile profile, [MarshalAs(Utf8)] string text, out ulong utterance);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_gap(SpeechHandle ctx, uint queue, uint ms, out ulong utterance);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_stop(SpeechHandle ctx, uint queue);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_flush(SpeechHandle ctx, uint queue);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_pause(SpeechHandle ctx, uint queue);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_resume(SpeechHandle ctx, uint queue);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_cancel(SpeechHandle ctx, ulong utterance);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_stop_all(SpeechHandle ctx);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_utterance_state(SpeechHandle ctx, ulong utterance, out uint state);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_utterance_voice(SpeechHandle ctx, ulong utterance, out ulong voice);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_render(SpeechHandle ctx, in Profile profile, [MarshalAs(Utf8)] string text, uint flags, out ulong job);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_render_poll(SpeechHandle ctx, ulong job, out uint state);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_render_take(SpeechHandle ctx, ulong job, out IntPtr samples, out UIntPtr frames, out uint channels, out uint rate);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_render_free(SpeechHandle ctx, ulong job);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status lv_speak_at(SpeechHandle ctx, uint queue, in Profile profile, [MarshalAs(Utf8)] string text, uint flags,
        Loadstar.Audio.AudioDeviceHandle device, uint space, in Loadstar.Audio.SourceDesc desc, out ulong utterance, out ulong source);
}

/// <summary>
/// A speech context (speech spec §2–§7): the voice tree, queues, utterances, rendering and
/// positioned speech. One per process. Speaking verbs return a <see cref="Status"/> rather than
/// throwing, since a refused utterance is an ordinary runtime outcome on a per-frame path.
/// </summary>
public sealed class SpeechContext : IDisposable
{
    private readonly SpeechHandle _h;
    private const uint FlatBit = 0x10;

    private SpeechContext(SpeechHandle h) => _h = h;

    /// <summary>Open a context. Cheap; backends probe on the library's worker and the first
    /// utterance resolves once they have (see <see cref="Generation"/>).</summary>
    public static SpeechContext Open(string? prismPath = null, bool loadPrism = true, uint utteranceHistory = 0)
    {
        var path = IntPtr.Zero;
        try
        {
            if (prismPath != null)
            {
                var bytes = Encoding.UTF8.GetBytes(prismPath + "\0");
                path = Marshal.AllocHGlobal(bytes.Length);
                Marshal.Copy(bytes, 0, path, bytes.Length);
            }
            var cfg = new NativeSpeechConfig { PrismPath = path, LoadPrism = loadPrism ? 1u : 0u, UtteranceHistory = utteranceHistory };
            NativeSpeech.lv_create(cfg, out var h).ThrowIfError(nameof(NativeSpeech.lv_create));
            return new SpeechContext(h);
        }
        finally
        {
            if (path != IntPtr.Zero) Marshal.FreeHGlobal(path);
        }
    }

    /// <summary>The Prism tag this library was validated against and where prism.dll was loaded from; empty if not loaded.</summary>
    public string PrismVersion => ReadString((buf, cap, out n) => NativeSpeech.lv_prism_version(_h, buf, cap, out n));

    // --- tree ---

    /// <summary>Snapshot generation; 0 until the first probe has published.</summary>
    public ulong Generation
    {
        get { NativeSpeech.lv_tree_generation(_h, out var g).ThrowIfError(nameof(NativeSpeech.lv_tree_generation)); return g; }
    }

    /// <summary>Ask for a re-probe; returns whether the tree changed shape since the last call
    /// (the re-probe itself lands asynchronously — ask again next frame).</summary>
    public bool Refresh()
    {
        NativeSpeech.lv_tree_refresh(_h, out var changed).ThrowIfError(nameof(NativeSpeech.lv_tree_refresh));
        return changed;
    }

    /// <summary>The whole tree, categories in rank order with their voices.</summary>
    public List<SpeechCategory> GetTree()
    {
        var result = new List<SpeechCategory>();
        NativeSpeech.lv_category_count(_h, out var count).ThrowIfError(nameof(NativeSpeech.lv_category_count));
        for (ulong i = 0; i < (ulong)count; i++)
        {
            NativeSpeech.lv_category_at(_h, (UIntPtr)i, out var id).ThrowIfError(nameof(NativeSpeech.lv_category_at));
            var cat = new SpeechCategory { Id = id };
            cat.Key = ReadString((buf, cap, out n) => NativeSpeech.lv_category_key(_h, id, buf, cap, out n));
            cat.Name = ReadString((buf, cap, out n) => NativeSpeech.lv_category_name(_h, id, buf, cap, out n));
            NativeSpeech.lv_category_rank(_h, id, out var rank).ThrowIfError(nameof(NativeSpeech.lv_category_rank));
            cat.Rank = rank;
            NativeSpeech.lv_category_auto(_h, id, out var auto).ThrowIfError(nameof(NativeSpeech.lv_category_auto));
            cat.Auto = auto;
            NativeSpeech.lv_voice_count(_h, id, out var vc).ThrowIfError(nameof(NativeSpeech.lv_voice_count));
            for (ulong j = 0; j < (ulong)vc; j++)
            {
                NativeSpeech.lv_voice_at(_h, id, (UIntPtr)j, out var vid).ThrowIfError(nameof(NativeSpeech.lv_voice_at));
                cat.Voices.Add(GetVoice(vid)!);
            }
            result.Add(cat);
        }
        return result;
    }

    /// <summary>One voice by id, or null if the tree has no such leaf right now.</summary>
    public SpeechVoice? GetVoice(ulong id)
    {
        if (NativeSpeech.lv_voice_category(_h, id, out var cat) != Status.Ok) return null;
        var v = new SpeechVoice { Id = id, Category = cat };
        v.Name = ReadString((buf, cap, out n) => NativeSpeech.lv_voice_name(_h, id, buf, cap, out n));
        v.Language = ReadString((buf, cap, out n) => NativeSpeech.lv_voice_language(_h, id, buf, cap, out n));
        NativeSpeech.lv_voice_features(_h, id, out var bits);
        v.Features = (SpeechFeature)bits;
        NativeSpeech.lv_voice_available(_h, id, out var avail);
        v.Available = avail;
        return v;
    }

    /// <summary>What <paramref name="profile"/> would use right now for a call needing <paramref name="required"/>.
    /// <see cref="Status.Ok"/>, <see cref="Status.VoiceUnavailable"/> (voice 0) or <see cref="Status.NotSupported"/> (see <paramref name="missing"/>).
    /// Before the first probe, Ok with voice 0.</summary>
    public Status Resolve(in Profile profile, SpeechFeature required, out ulong voice, out bool fellBack, out SpeechFeature missing)
    {
        var st = NativeSpeech.lv_resolve(_h, profile, (ulong)required, out voice, out fellBack, out var m);
        missing = (SpeechFeature)m;
        return st;
    }

    // --- queues ---

    public uint CreateQueue(QueueOptions options)
    {
        NativeSpeech.lv_queue_create(_h, options, out var q).ThrowIfError(nameof(NativeSpeech.lv_queue_create));
        return q;
    }

    public void SetQueue(uint queue, QueueOptions options) => NativeSpeech.lv_queue_set(_h, queue, options).ThrowIfError(nameof(NativeSpeech.lv_queue_set));
    public void DestroyQueue(uint queue) => NativeSpeech.lv_queue_destroy(_h, queue).ThrowIfError(nameof(NativeSpeech.lv_queue_destroy));

    // --- speaking ---

    /// <summary>Speak on <paramref name="queue"/>. Empty text is an already-done utterance.</summary>
    public Status Speak(uint queue, in Profile profile, string text, SpeakFlags flags, out ulong utterance)
        => NativeSpeech.lv_speak(_h, queue, profile, text ?? "", (uint)flags, out utterance);

    /// <summary>Speech plus braille where the voice supports both.</summary>
    public Status Output(uint queue, in Profile profile, string text, SpeakFlags flags, out ulong utterance)
        => Speak(queue, profile, text, flags | SpeakFlags.BrailleToo, out utterance);

    public Status Braille(uint queue, in Profile profile, string text, out ulong utterance)
        => NativeSpeech.lv_braille(_h, queue, profile, text ?? "", out utterance);

    /// <summary>Silence as a queue item.</summary>
    public Status Gap(uint queue, uint ms, out ulong utterance) => NativeSpeech.lv_gap(_h, queue, ms, out utterance);

    /// <summary>Cut what this queue is speaking; keep what is pending.</summary>
    public void Stop(uint queue) => NativeSpeech.lv_stop(_h, queue);
    /// <summary>Drop everything pending on this queue; keep what is speaking.</summary>
    public void Flush(uint queue) => NativeSpeech.lv_flush(_h, queue);
    public void Pause(uint queue) => NativeSpeech.lv_pause(_h, queue);
    public void Resume(uint queue) => NativeSpeech.lv_resume(_h, queue);
    public void Cancel(ulong utterance) => NativeSpeech.lv_cancel(_h, utterance);
    public void StopAll() => NativeSpeech.lv_stop_all(_h);

    /// <summary>An utterance's state; <paramref name="flat"/> is set when a positional utterance was spoken unpositioned.</summary>
    public UtteranceState State(ulong utterance, out bool flat)
    {
        NativeSpeech.lv_utterance_state(_h, utterance, out var raw);
        flat = (raw & FlatBit) != 0;
        return (UtteranceState)(raw & ~FlatBit);
    }

    public UtteranceState State(ulong utterance) => State(utterance, out _);

    /// <summary>The voice that spoke (or will speak) the utterance after any fallback; 0 if not yet known.</summary>
    public ulong UtteranceVoice(ulong utterance)
    {
        NativeSpeech.lv_utterance_voice(_h, utterance, out var v);
        return v;
    }

    // --- rendering ---

    /// <summary>Start a render job. <see cref="Status.NotSupported"/> synchronously if the voice cannot render.</summary>
    public Status Render(in Profile profile, string text, bool ssml, out ulong job)
        => NativeSpeech.lv_render(_h, profile, text ?? "", ssml ? (uint)SpeakFlags.Ssml : 0u, out job);

    public RenderJobState RenderPoll(ulong job)
    {
        NativeSpeech.lv_render_poll(_h, job, out var s);
        return (RenderJobState)s;
    }

    /// <summary>Copy out a finished render (interleaved f32) and free the job. Null while not done.</summary>
    public float[]? RenderTake(ulong job, out uint channels, out uint rate)
    {
        channels = 0; rate = 0;
        if (NativeSpeech.lv_render_take(_h, job, out var ptr, out var frames, out channels, out rate) != Status.Ok) return null;
        var n = (int)((ulong)frames * channels);
        var samples = new float[n];
        if (n > 0) Marshal.Copy(ptr, samples, 0, n);
        NativeSpeech.lv_render_free(_h, job);
        return samples;
    }

    public void RenderFree(ulong job) => NativeSpeech.lv_render_free(_h, job);

    /// <summary>Positional speech: rendered off the game thread and fired into <paramref name="space"/>
    /// as a one-shot source built from <paramref name="template"/> (your bus, bounds, gain, pan width,
    /// priority; the library sets the id, buffer and play flags), honouring the queue like any utterance.
    /// <paramref name="source"/> is the mod's handle for the audio device's stop/upsert from the moment
    /// this returns.</summary>
    public Status SpeakAt(uint queue, in Profile profile, string text, SpeakFlags flags, Loadstar.Audio.AudioDevice device, uint space, in Loadstar.Audio.SourceDesc template,
        out ulong utterance, out ulong source)
        => NativeSpeech.lv_speak_at(_h, queue, profile, text ?? "", (uint)flags, device.Handle, space, template, out utterance, out source);

    public void Dispose() => _h.Dispose();

    // --- helpers ---

    private delegate Status StringCall(byte[]? buf, UIntPtr cap, out UIntPtr written);

    private static string ReadString(StringCall call)
    {
        var st = call(null, UIntPtr.Zero, out var needed);
        if (st == Status.Ok) return "";
        if (st != Status.BufferTooSmall) throw new LoadstarException(st, "string");
        var buf = new byte[(int)(ulong)needed];
        call(buf, (UIntPtr)buf.Length, out var len).ThrowIfError("string");
        return Encoding.UTF8.GetString(buf, 0, (int)(ulong)len);
    }
}
