using System;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace Loadstar.Audio;

// Mirrors the la_* surface in include/loadstar.h (docs/audio-spec.md). All records are flat and
// pinned to their native sizes; see the smoke harness for the size assertions.

public enum SpaceMode : uint { Planar = 0, Spatial = 1 }

public enum SourceKind : byte { Buffer = 0, Oscillator = 1, Noise = 2 }

public enum Waveform : byte { Sine = 0, Triangle = 1, Square = 2, Saw = 3 }

public enum NoiseColor : byte { White = 0, Pink = 1, Brown = 2 }

[Flags]
public enum SourceFlags : uint
{
    None = 0,
    /// <summary>Desired state, level-triggered. A finished sequence stays finished while the record is unchanged.</summary>
    Playing = 1,
    /// <summary>Edge-triggered: start the sequence again this frame. Cleared by the library.</summary>
    Restart = 2,
    /// <summary>Listener-relative: no geometry, <see cref="SourceDesc.PanWidth"/> is the pan (−1..1).</summary>
    Relative = 4,
    /// <summary>Remove the source from the space when its sequence finishes.</summary>
    AutoRemove = 8,
}

public enum EffectKind : uint
{
    Gain = 0, LowPass = 1, HighPass = 2, LowShelf = 3, HighShelf = 4, Peak = 5, Delay = 6, Limiter = 7,
    /// <summary>Configures the voice's distance model; not processed in the chain.</summary>
    Distance = 8,
    /// <summary>Configures the head model for this source; not processed in the chain.</summary>
    Spatializer = 9,
}

[Flags]
public enum EffectFlags : uint
{
    Enabled = 1,
    // Spatializer cue toggles:
    Itd = 2,
    HeadShadow = 4,
    RearCue = 8,
    Elevation = 16,
}

/// <summary><see cref="None"/>: gain 1 at every distance — the mod owns distance → gain (max still culls when set).</summary>
public enum DistanceModel : uint { Inverse = 0, Linear = 1, Reference = 2, None = 3 }

public enum ChainKind : uint { Master = 0, Space = 1, Bus = 2, SourcePre = 3, SourcePost = 4 }

[StructLayout(LayoutKind.Sequential)]
public struct DeviceConfig
{
    /// <summary>0 = the device's mix rate.</summary>
    public uint SampleRate;
    /// <summary>0 = the smallest period the driver offers.</summary>
    public uint PeriodFrames;
    public float MasterGain;
    /// <summary>0 = default (64).</summary>
    public uint VoiceCap;

    public static DeviceConfig Default => new() { MasterGain = 1f };
}

[StructLayout(LayoutKind.Sequential)]
public struct StreamInfo
{
    public uint SampleRate, PeriodFrames, Channels;
    private uint _lowLatency;

    /// <summary>True when the IAudioClient3 low-latency path is active.</summary>
    public bool LowLatency => _lowLatency != 0;
    public float PeriodMs => PeriodFrames * 1000f / Math.Max(1, SampleRate);

    public override string ToString() => $"{SampleRate} Hz, {PeriodFrames} frames ({PeriodMs:F2} ms), low-latency {LowLatency}";
}

/// <summary>Mirrors <c>LsAudioSourceDesc</c> (96 bytes). Build with the factories, refine with the fluent methods.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct SourceDesc
{
    public ulong Id;
    public SourceKind Kind;
    /// <summary>Oscillator waveform or noise colour.</summary>
    public byte Waveform;
    public byte Priority;
    private byte _reserved0;
    /// <summary>Plays per trigger; 0 = forever.</summary>
    public ushort RepeatCount;
    private ushort _reserved1;
    public uint Buffer;
    public uint Bus;
    public SourceFlags Flags;
    public Bounds Bounds;
    public float Gain;
    /// <summary>Buffer: playback rate. Oscillator: frequency in Hz.</summary>
    public float Pitch;
    /// <summary>Lateral crossover in the space's units; for Relative sources, the pan (−1..1).</summary>
    public float PanWidth;
    public float RepeatGapMs;
    private uint _reserved2, _reserved3, _reserved4;

    private static SourceDesc Base(ulong id, SourceKind kind, Bounds bounds) => new()
    {
        Id = id, Kind = kind, RepeatCount = 1, Flags = SourceFlags.Playing, Bounds = bounds, Gain = 1f, Pitch = 1f, PanWidth = 1f,
    };

    public static SourceDesc Buffer_(ulong id, uint buffer, Bounds bounds) { var d = Base(id, SourceKind.Buffer, bounds); d.Buffer = buffer; return d; }
    public static SourceDesc Oscillator(ulong id, float hz, Waveform waveform, Bounds bounds) { var d = Base(id, SourceKind.Oscillator, bounds); d.Pitch = hz; d.Waveform = (byte)waveform; d.RepeatCount = 0; return d; }
    public static SourceDesc Noise(ulong id, NoiseColor color, Bounds bounds) { var d = Base(id, SourceKind.Noise, bounds); d.Waveform = (byte)color; d.RepeatCount = 0; return d; }

    public SourceDesc Looping() { RepeatCount = 0; return this; }
    public SourceDesc Repeats(ushort count, float gapMs) { RepeatCount = count; RepeatGapMs = gapMs; return this; }
    public SourceDesc Relative(float pan) { Flags |= SourceFlags.Relative; PanWidth = pan; return this; }
    public SourceDesc OnBus(uint bus) { Bus = bus; return this; }
    public SourceDesc WithGain(float gain) { Gain = gain; return this; }
    public SourceDesc WithPriority(byte priority) { Priority = priority; return this; }
    public SourceDesc WithFlags(SourceFlags set, SourceFlags clear = SourceFlags.None) { Flags = (Flags | set) & ~clear; return this; }
}

/// <summary>Mirrors <c>LsAudioEffectDesc</c> (40 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
public struct EffectDesc
{
    public EffectKind Kind;
    public EffectFlags Flags;
    public float P0, P1, P2, P3, P4, P5, P6, P7;

    private static EffectDesc Make(EffectKind kind, float p0 = 0, float p1 = 0, float p2 = 0, float p3 = 0, EffectFlags extra = 0) =>
        new() { Kind = kind, Flags = EffectFlags.Enabled | extra, P0 = p0, P1 = p1, P2 = p2, P3 = p3 };

    public static EffectDesc Gain(float gain) => Make(EffectKind.Gain, gain);
    public static EffectDesc LowPass(float hz, float q = 0.707f) => Make(EffectKind.LowPass, hz, q);
    public static EffectDesc HighPass(float hz, float q = 0.707f) => Make(EffectKind.HighPass, hz, q);
    public static EffectDesc LowShelf(float hz, float db) => Make(EffectKind.LowShelf, hz, db);
    public static EffectDesc HighShelf(float hz, float db) => Make(EffectKind.HighShelf, hz, db);
    public static EffectDesc Peak(float hz, float db, float q = 1f) => Make(EffectKind.Peak, hz, db, q);
    public static EffectDesc Delay(float ms, float feedback, float mix) => Make(EffectKind.Delay, ms, feedback, mix);
    public static EffectDesc Limiter(float thresholdDb, float releaseMs) => Make(EffectKind.Limiter, thresholdDb, releaseMs);
    public static EffectDesc Distance(float min, float max, float rolloff, DistanceModel model) => Make(EffectKind.Distance, min, max, rolloff, (float)model);

    /// <summary>Head-model overrides for one source; 0 keeps the default for that parameter.</summary>
    public static EffectDesc Spatializer(EffectFlags cues = EffectFlags.Itd | EffectFlags.HeadShadow | EffectFlags.RearCue,
        float ildCapDb = 0, float panExponent = 0, float itdMaxMs = 0, float shadowCornerHz = 0, float shadowMaxDb = 0,
        float rearCornerHz = 0, float rearMaxDb = 0, float elevationMaxDb = 0) => new()
    {
        Kind = EffectKind.Spatializer, Flags = EffectFlags.Enabled | cues,
        P0 = ildCapDb, P1 = panExponent, P2 = itdMaxMs, P3 = shadowCornerHz, P4 = shadowMaxDb, P5 = rearCornerHz, P6 = rearMaxDb, P7 = elevationMaxDb,
    };

    public EffectDesc Disabled() { Flags &= ~EffectFlags.Enabled; return this; }
}

/// <summary>Mirrors <c>LsAudioChainTarget</c> (24 bytes).</summary>
[StructLayout(LayoutKind.Sequential)]
public struct ChainTarget
{
    public ChainKind Kind;
    public uint Space;
    public uint Bus;
    private uint _reserved;
    public ulong SourceId;

    public static ChainTarget Master => new() { Kind = ChainKind.Master };
    public static ChainTarget ForSpace(uint space) => new() { Kind = ChainKind.Space, Space = space };
    public static ChainTarget ForBus(uint space, uint bus) => new() { Kind = ChainKind.Bus, Space = space, Bus = bus };
    public static ChainTarget SourcePre(uint space, ulong sourceId) => new() { Kind = ChainKind.SourcePre, Space = space, SourceId = sourceId };
    public static ChainTarget SourcePost(uint space, ulong sourceId) => new() { Kind = ChainKind.SourcePost, Space = space, SourceId = sourceId };
}

/// <summary>Mirrors <c>LsAudioFinished</c>: a sequence that ended.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct Finished
{
    public uint Space;
    private uint _reserved;
    public ulong Id;
}

public sealed class AudioDeviceHandle : SafeHandleZeroOrMinusOneIsInvalid
{
    public AudioDeviceHandle() : base(ownsHandle: true) { }

    protected override bool ReleaseHandle()
    {
        NativeAudio.la_device_close(handle);
        return true;
    }
}

internal static class NativeAudio
{
    private const string Lib = "loadstar";
    private const CallingConvention Cc = CallingConvention.Cdecl;

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_device_open(DeviceConfig cfg, out AudioDeviceHandle device);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_device_open_null(DeviceConfig cfg, uint sampleRate, uint periodFrames, out AudioDeviceHandle device);
    [DllImport(Lib, CallingConvention = Cc)] public static extern void la_device_close(IntPtr device);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_device_info(AudioDeviceHandle device, out StreamInfo info);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_device_master_gain(AudioDeviceHandle device, float gain);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_device_render(AudioDeviceHandle device, [Out] float[] buffer, UIntPtr frames);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_device_finished(AudioDeviceHandle device, [Out] Finished[]? buffer, UIntPtr capacity, out UIntPtr written);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_space_create(AudioDeviceHandle device, SpaceMode mode, float gain, float unitScale, out uint space);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_space_destroy(AudioDeviceHandle device, uint space);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_space_gain(AudioDeviceHandle device, uint space, float gain);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_listener_set(AudioDeviceHandle device, uint space, Vec3 position, Vec3 forward, Vec3 up);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_space_update(AudioDeviceHandle device, uint space, float dt);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_bus_create(AudioDeviceHandle device, uint space, float gain, out uint bus);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_bus_gain(AudioDeviceHandle device, uint space, uint bus, float gain);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_buffer_create(AudioDeviceHandle device, float[] samples, UIntPtr samplesLen, uint channels, uint sampleRate, out uint buffer);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_buffer_click(AudioDeviceHandle device, float ms, out uint buffer);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_buffer_tone(AudioDeviceHandle device, float hz, float ms, float attackMs, float releaseMs, out uint buffer);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_buffer_free(AudioDeviceHandle device, uint buffer);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_space_sync(AudioDeviceHandle device, uint space, SourceDesc[]? descs, UIntPtr descsLen);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_source_upsert(AudioDeviceHandle device, uint space, in SourceDesc desc);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_source_remove(AudioDeviceHandle device, uint space, ulong id);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_space_fire(AudioDeviceHandle device, uint space, in SourceDesc desc);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_source_stop(AudioDeviceHandle device, uint space, ulong id);

    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_chain_set(AudioDeviceHandle device, ChainTarget target, EffectDesc[]? nodes, UIntPtr nodesLen);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_effect_set_param(AudioDeviceHandle device, ChainTarget target, uint index, uint param, float value);
    [DllImport(Lib, CallingConvention = Cc)] public static extern Status la_effect_set_enabled(AudioDeviceHandle device, ChainTarget target, uint index, [MarshalAs(UnmanagedType.U1)] bool enabled);
}

/// <summary>
/// An open audio device: spaces, buses, buffers, sources and effect chains (audio spec §3).
/// One per process. Every call is a command to the audio thread; nothing here blocks on it.
/// </summary>
public sealed class AudioDevice : IDisposable
{
    private readonly AudioDeviceHandle _h;

    private AudioDevice(AudioDeviceHandle h) => _h = h;

    /// <summary>Open the default output (WASAPI shared mode). Throws <see cref="LoadstarException"/>
    /// with <see cref="Status.DeviceUnavailable"/> if there is none.</summary>
    public static AudioDevice Open(DeviceConfig? config = null)
    {
        NativeAudio.la_device_open(config ?? DeviceConfig.Default, out var h).ThrowIfError(nameof(NativeAudio.la_device_open));
        return new AudioDevice(h);
    }

    /// <summary>A device with no hardware; render periods by hand with <see cref="Render"/>.</summary>
    public static AudioDevice OpenNull(uint sampleRate, uint periodFrames, DeviceConfig? config = null)
    {
        NativeAudio.la_device_open_null(config ?? DeviceConfig.Default, sampleRate, periodFrames, out var h).ThrowIfError(nameof(NativeAudio.la_device_open_null));
        return new AudioDevice(h);
    }

    public StreamInfo Info
    {
        get
        {
            NativeAudio.la_device_info(_h, out var info).ThrowIfError(nameof(NativeAudio.la_device_info));
            return info;
        }
    }

    public void MasterGain(float gain) => NativeAudio.la_device_master_gain(_h, gain).ThrowIfError(nameof(NativeAudio.la_device_master_gain));

    /// <summary>Null devices only: render <c>buffer.Length / 2</c> frames of interleaved stereo.</summary>
    public void Render(float[] interleavedStereo) =>
        NativeAudio.la_device_render(_h, interleavedStereo, (UIntPtr)(interleavedStereo.Length / 2)).ThrowIfError(nameof(NativeAudio.la_device_render));

    /// <summary>Sequences that ended since the last call.</summary>
    public Finished[] TakeFinished()
    {
        var status = NativeAudio.la_device_finished(_h, null, UIntPtr.Zero, out var needed);
        if (status == Status.Ok) return Array.Empty<Finished>();
        if (status != Status.BufferTooSmall) throw new LoadstarException(status, nameof(NativeAudio.la_device_finished));
        var items = new Finished[(int)(ulong)needed];
        NativeAudio.la_device_finished(_h, items, (UIntPtr)items.Length, out _).ThrowIfError(nameof(NativeAudio.la_device_finished));
        return items;
    }

    // --- Spaces ---

    public uint CreateSpace(SpaceMode mode, float gain = 1f, float unitScale = 1f)
    {
        NativeAudio.la_space_create(_h, mode, gain, unitScale, out var id).ThrowIfError(nameof(NativeAudio.la_space_create));
        return id;
    }

    public void DestroySpace(uint space) => NativeAudio.la_space_destroy(_h, space).ThrowIfError(nameof(NativeAudio.la_space_destroy));
    public void SpaceGain(uint space, float gain) => NativeAudio.la_space_gain(_h, space, gain).ThrowIfError(nameof(NativeAudio.la_space_gain));

    /// <summary>Listener pose, each frame. Facing rotates the ears even in Planar mode.</summary>
    public void SetListener(uint space, Vec3 position, Vec3 forward, Vec3 up) =>
        NativeAudio.la_listener_set(_h, space, position, forward, up).ThrowIfError(nameof(NativeAudio.la_listener_set));

    /// <summary>Per-frame hook for simulating renderers; a no-op for the head model.</summary>
    public void Update(uint space, float dt) => NativeAudio.la_space_update(_h, space, dt).ThrowIfError(nameof(NativeAudio.la_space_update));

    public uint CreateBus(uint space, float gain = 1f)
    {
        NativeAudio.la_bus_create(_h, space, gain, out var bus).ThrowIfError(nameof(NativeAudio.la_bus_create));
        return bus;
    }

    public void BusGain(uint space, uint bus, float gain) => NativeAudio.la_bus_gain(_h, space, bus, gain).ThrowIfError(nameof(NativeAudio.la_bus_gain));

    // --- Buffers ---

    /// <summary>Upload interleaved PCM; copied and converted to mono at the mixer rate.</summary>
    public uint CreateBuffer(float[] samples, uint channels, uint sampleRate)
    {
        NativeAudio.la_buffer_create(_h, samples, (UIntPtr)samples.Length, channels, sampleRate, out var id).ThrowIfError(nameof(NativeAudio.la_buffer_create));
        return id;
    }

    public uint Click(float ms = 8f)
    {
        NativeAudio.la_buffer_click(_h, ms, out var id).ThrowIfError(nameof(NativeAudio.la_buffer_click));
        return id;
    }

    public uint Tone(float hz, float ms, float attackMs = 5f, float releaseMs = 20f)
    {
        NativeAudio.la_buffer_tone(_h, hz, ms, attackMs, releaseMs, out var id).ThrowIfError(nameof(NativeAudio.la_buffer_tone));
        return id;
    }

    public void FreeBuffer(uint buffer) => NativeAudio.la_buffer_free(_h, buffer).ThrowIfError(nameof(NativeAudio.la_buffer_free));

    // --- Sources ---

    /// <summary>Declare the space's soundscape; the library diffs and removes what is absent.</summary>
    public void Sync(uint space, SourceDesc[] sources) =>
        NativeAudio.la_space_sync(_h, space, sources.Length == 0 ? null : sources, (UIntPtr)sources.Length).ThrowIfError(nameof(NativeAudio.la_space_sync));

    public void Upsert(uint space, in SourceDesc source) => NativeAudio.la_source_upsert(_h, space, source).ThrowIfError(nameof(NativeAudio.la_source_upsert));
    public void Remove(uint space, ulong id) => NativeAudio.la_source_remove(_h, space, id).ThrowIfError(nameof(NativeAudio.la_source_remove));

    /// <summary>Play a sequence and forget it; the source removes itself when done.</summary>
    public void Fire(uint space, in SourceDesc source) => NativeAudio.la_space_fire(_h, space, source).ThrowIfError(nameof(NativeAudio.la_space_fire));
    public void Stop(uint space, ulong id) => NativeAudio.la_source_stop(_h, space, id).ThrowIfError(nameof(NativeAudio.la_source_stop));

    // --- Effects ---

    public void SetChain(ChainTarget target, EffectDesc[] nodes) =>
        NativeAudio.la_chain_set(_h, target, nodes.Length == 0 ? null : nodes, (UIntPtr)nodes.Length).ThrowIfError(nameof(NativeAudio.la_chain_set));

    public void SetEffectParam(ChainTarget target, int index, int param, float value) =>
        NativeAudio.la_effect_set_param(_h, target, (uint)index, (uint)param, value).ThrowIfError(nameof(NativeAudio.la_effect_set_param));

    public void SetEffectEnabled(ChainTarget target, int index, bool enabled) =>
        NativeAudio.la_effect_set_enabled(_h, target, (uint)index, enabled).ThrowIfError(nameof(NativeAudio.la_effect_set_enabled));

    public void Dispose() => _h.Dispose();
}
