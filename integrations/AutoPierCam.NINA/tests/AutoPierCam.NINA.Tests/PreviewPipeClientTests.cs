using System.Buffers.Binary;
using System.Text;
using AutoPierCam.NINA.Preview;

namespace AutoPierCam.NINA.Tests;

public sealed class PreviewPipeClientTests
{
    private static readonly byte[] MinimalJpeg =
    [
        0xff, 0xd8,
        0xff, 0xc0, 0x00, 0x07, 0x08, 0x01, 0xe0, 0x02, 0x80,
        0xff, 0xd9,
    ];

    [Fact]
    public async Task ReadsValidProtocolV1Record()
    {
        var client = new PreviewPipeClient();
        await using var stream = BuildRecord(ValidMetadata(), MinimalJpeg);

        PreviewFrameData? frame = await client.ReadFrameAsync(stream);

        Assert.NotNull(frame);
        Assert.Equal((uint)640, frame.Metadata.Width);
        Assert.Equal((uint)480, frame.Metadata.Height);
        Assert.Equal(12_500, frame.Metadata.ExposureUs);
        Assert.Equal("night", frame.Metadata.Mode);
        Assert.Equal(MinimalJpeg, frame.Jpeg);
    }

    [Fact]
    public async Task RejectsZeroMetadataLengthBeforeReadingPayload()
    {
        await using var stream = Prefix(0);
        var client = new PreviewPipeClient();

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains("must not be zero", error.Message);
    }

    [Fact]
    public async Task RejectsOversizedMetadataBeforeAllocatingPayload()
    {
        await using var stream = Prefix(PreviewPipeClient.MaxMetadataBytes + 1u);
        var client = new PreviewPipeClient();

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains("4,096-byte limit", error.Message);
    }

    [Fact]
    public async Task RejectsOversizedJpegBeforeAllocatingPayload()
    {
        await using var stream = BuildRecordWithJpegLength(
            ValidMetadata(),
            PreviewPipeClient.MaxJpegBytes + 1u);
        var client = new PreviewPipeClient();

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains("4,194,304-byte limit", error.Message);
    }

    [Fact]
    public async Task RejectsUnknownMetadataFields()
    {
        string metadata = ValidMetadata().Replace(
            "\"dropped_frames\":3",
            "\"dropped_frames\":3,\"future_field\":true",
            StringComparison.Ordinal);
        await using var stream = BuildRecord(metadata, MinimalJpeg);
        var client = new PreviewPipeClient();

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains("protocol-v1 JSON", error.Message);
    }

    [Fact]
    public async Task RejectsMissingRequiredMetadataFields()
    {
        string metadata = ValidMetadata().Replace("\"gain\":120,", string.Empty, StringComparison.Ordinal);
        await using var stream = BuildRecord(metadata, MinimalJpeg);
        var client = new PreviewPipeClient();

        await Assert.ThrowsAsync<PreviewProtocolException>(() => client.ReadFrameAsync(stream));
    }

    [Theory]
    [InlineData("\"width\":640", "\"width\":1281", "edge limit")]
    [InlineData("\"mode\":\"night\"", "\"mode\":\"twilight\"", "Unsupported preview mode")]
    [InlineData("\"exposure_us\":12500", "\"exposure_us\":0", "exposure must be positive")]
    [InlineData("\"gain\":120", "\"gain\":-1", "gain must not be negative")]
    [InlineData("\"content_type\":\"image/jpeg\"", "\"content_type\":\"image/png\"", "content type")]
    public async Task RejectsInvalidMetadataValues(
        string original,
        string replacement,
        string expectedMessage)
    {
        string metadata = ValidMetadata().Replace(original, replacement, StringComparison.Ordinal);
        await using var stream = BuildRecord(metadata, MinimalJpeg);
        var client = new PreviewPipeClient();

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains(expectedMessage, error.Message, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public async Task RejectsPayloadWithoutJpegMarkers()
    {
        await using var stream = BuildRecord(ValidMetadata(), [1, 2, 3, 4]);
        var client = new PreviewPipeClient();

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains("JPEG start and end markers", error.Message);
    }

    [Fact]
    public async Task RejectsJpegDimensionsThatDisagreeWithMetadata()
    {
        byte[] jpeg =
        [
            0xff, 0xd8,
            0xff, 0xc0, 0x00, 0x07, 0x08, 0x01, 0xe0, 0x02, 0x81,
            0xff, 0xd9,
        ];
        await using var stream = BuildRecord(ValidMetadata(), jpeg);
        var client = new PreviewPipeClient();

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains("do not match metadata", error.Message);
    }

    [Fact]
    public async Task TimesOutARecordThatStallsAfterItsFirstByte()
    {
        await using var stream = new FirstByteThenWaitStream();
        var client = new PreviewPipeClient(assemblyTimeout: TimeSpan.FromMilliseconds(100));

        PreviewProtocolException error = await Assert.ThrowsAsync<PreviewProtocolException>(
            () => client.ReadFrameAsync(stream));

        Assert.Contains("did not finish", error.Message);
    }

    [Theory]
    [InlineData(850, "850 µs")]
    [InlineData(12_500, "12.5 ms")]
    [InlineData(2_500_000, "2.5 s")]
    public void FormatsExposureForOperators(long exposureUs, string expected)
    {
        Assert.Equal(expected, PierCameraPreviewRuntime.FormatExposure(exposureUs));
    }

    private static string ValidMetadata() =>
        """
        {"version":1,"session_generation":4,"sequence":128,"captured_at_unix_ms":1725000000123,"width":640,"height":480,"exposure_us":12500,"gain":120,"content_type":"image/jpeg","mode":"night","dropped_frames":3}
        """;

    private static MemoryStream Prefix(uint length)
    {
        byte[] bytes = new byte[sizeof(uint)];
        BinaryPrimitives.WriteUInt32LittleEndian(bytes, length);
        return new MemoryStream(bytes, writable: false);
    }

    private static MemoryStream BuildRecord(string metadata, byte[] jpeg)
    {
        byte[] encodedMetadata = Encoding.UTF8.GetBytes(metadata);
        var stream = new MemoryStream();
        WritePrefix(stream, checked((uint)encodedMetadata.Length));
        stream.Write(encodedMetadata);
        WritePrefix(stream, checked((uint)jpeg.Length));
        stream.Write(jpeg);
        stream.Position = 0;
        return stream;
    }

    private static MemoryStream BuildRecordWithJpegLength(string metadata, uint jpegLength)
    {
        byte[] encodedMetadata = Encoding.UTF8.GetBytes(metadata);
        var stream = new MemoryStream();
        WritePrefix(stream, checked((uint)encodedMetadata.Length));
        stream.Write(encodedMetadata);
        WritePrefix(stream, jpegLength);
        stream.Position = 0;
        return stream;
    }

    private static void WritePrefix(Stream stream, uint value)
    {
        Span<byte> prefix = stackalloc byte[sizeof(uint)];
        BinaryPrimitives.WriteUInt32LittleEndian(prefix, value);
        stream.Write(prefix);
    }

    private sealed class FirstByteThenWaitStream : Stream
    {
        private int delivered;

        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position
        {
            get => throw new NotSupportedException();
            set => throw new NotSupportedException();
        }

        public override int Read(byte[] buffer, int offset, int count) =>
            throw new NotSupportedException();

        public override async ValueTask<int> ReadAsync(
            Memory<byte> buffer,
            CancellationToken cancellationToken = default)
        {
            if (Interlocked.Exchange(ref delivered, 1) == 0)
            {
                buffer.Span[0] = 1;
                return 1;
            }

            await Task.Delay(Timeout.InfiniteTimeSpan, cancellationToken);
            return 0;
        }

        public override void Flush() => throw new NotSupportedException();
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    }
}
