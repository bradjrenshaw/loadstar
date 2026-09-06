using System;
using System.IO;

namespace Loadstar.Audio;

/// <summary>
/// Minimal RIFF/WAVE reader for cue material: PCM 8/16/24/32-bit and 32-bit float, any channel
/// count and rate, returned as interleaved floats ready for <see cref="AudioDevice.CreateBuffer"/>.
/// Not a general decoder — no compressed formats, no cue/loop chunks.
/// </summary>
public static class WavFile
{
    public readonly struct Pcm
    {
        public readonly float[] Samples;
        public readonly uint Channels;
        public readonly uint SampleRate;

        public Pcm(float[] samples, uint channels, uint sampleRate)
        {
            Samples = samples; Channels = channels; SampleRate = sampleRate;
        }

        public float Seconds => Samples.Length / (float)(Channels * SampleRate);
    }

    public static Pcm Load(string path)
    {
        using var stream = File.OpenRead(path);
        return Load(stream);
    }

    public static Pcm Load(Stream stream)
    {
        using var r = new BinaryReader(stream);
        if (ReadTag(r) != "RIFF") throw new InvalidDataException("not a RIFF file");
        r.ReadUInt32();
        if (ReadTag(r) != "WAVE") throw new InvalidDataException("not a WAVE file");

        ushort format = 0, channels = 0, bits = 0;
        uint rate = 0;
        byte[]? data = null;

        while (stream.Position + 8 <= stream.Length)
        {
            string tag = ReadTag(r);
            uint size = r.ReadUInt32();
            long next = stream.Position + size + (size & 1);
            switch (tag)
            {
                case "fmt ":
                    format = r.ReadUInt16();
                    channels = r.ReadUInt16();
                    rate = r.ReadUInt32();
                    r.ReadUInt32(); // byte rate
                    r.ReadUInt16(); // block align
                    bits = r.ReadUInt16();
                    if (format == 0xFFFE && size >= 40)
                    {
                        r.ReadUInt16(); // cbSize
                        r.ReadUInt16(); // valid bits
                        r.ReadUInt32(); // channel mask
                        format = r.ReadUInt16(); // first two bytes of the sub-format GUID: 1 = PCM, 3 = float
                    }
                    break;
                case "data":
                    data = r.ReadBytes((int)Math.Min(size, stream.Length - stream.Position));
                    break;
            }
            if (next > stream.Length) break;
            stream.Position = next;
        }

        if (data == null || channels == 0 || rate == 0) throw new InvalidDataException("WAVE file has no fmt/data chunks");
        int bytesPer = bits / 8;
        if (bytesPer == 0) throw new InvalidDataException("WAVE file has zero bits per sample");
        int count = data.Length / bytesPer;
        var samples = new float[count];

        switch ((format, bits))
        {
            case (1, 8):
                for (int i = 0; i < count; i++) samples[i] = (data[i] - 128) / 128f;
                break;
            case (1, 16):
                for (int i = 0; i < count; i++) samples[i] = BitConverter.ToInt16(data, i * 2) / 32768f;
                break;
            case (1, 24):
                for (int i = 0; i < count; i++)
                {
                    int v = data[i * 3] | (data[i * 3 + 1] << 8) | (data[i * 3 + 2] << 16);
                    if ((v & 0x800000) != 0) v |= unchecked((int)0xFF000000);
                    samples[i] = v / 8388608f;
                }
                break;
            case (1, 32):
                for (int i = 0; i < count; i++) samples[i] = BitConverter.ToInt32(data, i * 4) / 2147483648f;
                break;
            case (3, 32):
                for (int i = 0; i < count; i++) samples[i] = BitConverter.ToSingle(data, i * 4);
                break;
            default:
                throw new InvalidDataException($"unsupported WAVE format {format} at {bits} bits");
        }
        return new Pcm(samples, channels, rate);
    }

    private static string ReadTag(BinaryReader r)
    {
        var b = r.ReadBytes(4);
        return b.Length == 4 ? System.Text.Encoding.ASCII.GetString(b) : "";
    }
}
