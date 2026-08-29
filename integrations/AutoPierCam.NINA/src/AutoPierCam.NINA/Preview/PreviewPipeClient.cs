using System.Buffers.Binary;
using System.IO;
using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AutoPierCam.NINA.Preview;

internal sealed class PreviewPipeClient
{
    internal const string DefaultPipeName = "autopiercam-preview-v1";
    internal const ushort ProtocolVersion = 1;
    internal const int MaxMetadataBytes = 4 * 1024;
    internal const int MaxJpegBytes = 4 * 1024 * 1024;
    internal const uint MaxDimension = 1_280;
    internal const ulong MaxPixels = 1_638_400;

    private static readonly TimeSpan[] ReconnectDelays =
    [
        TimeSpan.FromMilliseconds(250),
        TimeSpan.FromMilliseconds(500),
        TimeSpan.FromSeconds(1),
        TimeSpan.FromSeconds(2),
        TimeSpan.FromSeconds(5),
    ];

    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNameCaseInsensitive = false,
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
    };

    private readonly string pipeName;
    private readonly TimeSpan connectTimeout;
    private readonly TimeSpan assemblyTimeout;
    private int running;

    internal PreviewPipeClient(
        string pipeName = DefaultPipeName,
        TimeSpan? connectTimeout = null,
        TimeSpan? assemblyTimeout = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(pipeName);
        this.pipeName = pipeName;
        this.connectTimeout = connectTimeout ?? TimeSpan.FromSeconds(2);
        this.assemblyTimeout = assemblyTimeout ?? TimeSpan.FromSeconds(5);

        if (this.connectTimeout <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(connectTimeout));
        }

        if (this.assemblyTimeout <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(assemblyTimeout));
        }
    }

    internal async Task RunAsync(
        Func<PreviewFrame, CancellationToken, Task> onFrame,
        Func<PreviewStreamState, CancellationToken, Task> onStateChanged,
        CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(onFrame);
        ArgumentNullException.ThrowIfNull(onStateChanged);

        if (Interlocked.CompareExchange(ref running, 1, 0) != 0)
        {
            throw new InvalidOperationException("The preview client is already running.");
        }

        try
        {
            await RunLoopAsync(onFrame, onStateChanged, cancellationToken).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            // Cancellation is the normal process-wide plugin lifetime boundary.
        }
        finally
        {
            Volatile.Write(ref running, 0);
        }
    }

    private async Task RunLoopAsync(
        Func<PreviewFrame, CancellationToken, Task> onFrame,
        Func<PreviewStreamState, CancellationToken, Task> onStateChanged,
        CancellationToken cancellationToken)
    {
        int reconnectDelayIndex = 0;
        ulong connectionEpoch = 0;
        string? previousFailure = null;

        while (true)
        {
            cancellationToken.ThrowIfCancellationRequested();

            if (previousFailure is not null)
            {
                TimeSpan retryDelay = ReconnectDelays[reconnectDelayIndex];
                reconnectDelayIndex = Math.Min(reconnectDelayIndex + 1, ReconnectDelays.Length - 1);
                await onStateChanged(
                        new PreviewStreamState(
                            PreviewStreamPhase.Reconnecting,
                            connectionEpoch,
                            previousFailure,
                            retryDelay),
                        cancellationToken)
                    .ConfigureAwait(false);
                await Task.Delay(retryDelay, cancellationToken).ConfigureAwait(false);
            }

            connectionEpoch = NextEpoch(connectionEpoch);
            await onStateChanged(
                    new PreviewStreamState(PreviewStreamPhase.Connecting, connectionEpoch),
                    cancellationToken)
                .ConfigureAwait(false);

            await using var pipe = new NamedPipeClientStream(
                ".",
                pipeName,
                PipeDirection.In,
                PipeOptions.Asynchronous);

            string? connectionFailure = await TryConnectAsync(pipe, cancellationToken)
                .ConfigureAwait(false);
            if (connectionFailure is not null)
            {
                previousFailure = connectionFailure;
                continue;
            }

            previousFailure = null;
            await onStateChanged(
                    new PreviewStreamState(PreviewStreamPhase.WaitingForFrame, connectionEpoch),
                    cancellationToken)
                .ConfigureAwait(false);

            ulong? lastSequence = null;
            ulong? lastSessionGeneration = null;
            bool reportedLive = false;

            while (true)
            {
                PreviewFrameData? frameData;
                try
                {
                    frameData = await ReadFrameAsync(pipe, cancellationToken).ConfigureAwait(false);
                    if (frameData is null)
                    {
                        previousFailure = "AutoPierCam closed the preview stream.";
                        break;
                    }

                    ValidateMonotonicOrder(
                        frameData.Metadata,
                        lastSequence,
                        lastSessionGeneration);
                }
                catch (PreviewProtocolException exception)
                {
                    previousFailure = exception.Message;
                    break;
                }
                catch (IOException exception)
                {
                    previousFailure = $"Preview communication failed: {Compact(exception.Message)}";
                    break;
                }
                catch (UnauthorizedAccessException exception)
                {
                    previousFailure = $"Preview pipe access was denied: {Compact(exception.Message)}";
                    break;
                }

                lastSequence = frameData.Metadata.Sequence;
                lastSessionGeneration = frameData.Metadata.SessionGeneration;
                await onFrame(
                        new PreviewFrame(connectionEpoch, frameData.Metadata, frameData.Jpeg),
                        cancellationToken)
                    .ConfigureAwait(false);

                reconnectDelayIndex = 0;
                if (!reportedLive)
                {
                    reportedLive = true;
                    await onStateChanged(
                            new PreviewStreamState(PreviewStreamPhase.Live, connectionEpoch),
                            cancellationToken)
                        .ConfigureAwait(false);
                }
            }
        }
    }

    private async Task<string?> TryConnectAsync(
        NamedPipeClientStream pipe,
        CancellationToken cancellationToken)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(connectTimeout);

        try
        {
            await pipe.ConnectAsync(timeout.Token).ConfigureAwait(false);
            return null;
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            return $"Timed out connecting to AutoPierCam after {connectTimeout.TotalSeconds:0.#} seconds.";
        }
        catch (TimeoutException)
        {
            return $"Timed out connecting to AutoPierCam after {connectTimeout.TotalSeconds:0.#} seconds.";
        }
        catch (IOException exception)
        {
            return $"Could not connect to AutoPierCam: {Compact(exception.Message)}";
        }
        catch (UnauthorizedAccessException exception)
        {
            return $"Access to the AutoPierCam preview was denied: {Compact(exception.Message)}";
        }
    }

    internal async Task<PreviewFrameData?> ReadFrameAsync(
        Stream stream,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(stream);
        byte[] lengthPrefix = new byte[sizeof(uint)];

        // Waiting for a new frame is unbounded. Once its first byte arrives,
        // the rest must arrive promptly so a partial writer cannot strand N.I.N.A.
        int firstByte = await stream
            .ReadAsync(lengthPrefix.AsMemory(0, 1), cancellationToken)
            .ConfigureAwait(false);
        if (firstByte == 0)
        {
            return null;
        }

        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        timeout.CancelAfter(assemblyTimeout);
        CancellationToken assemblyToken = timeout.Token;

        try
        {
            await ReadExactlyAsync(
                    stream,
                    lengthPrefix.AsMemory(1),
                    "preview metadata length",
                    assemblyToken)
                .ConfigureAwait(false);
            int metadataLength = ValidateLength(
                BinaryPrimitives.ReadUInt32LittleEndian(lengthPrefix),
                MaxMetadataBytes,
                "metadata");

            byte[] encodedMetadata = new byte[metadataLength];
            await ReadExactlyAsync(stream, encodedMetadata, "preview metadata", assemblyToken)
                .ConfigureAwait(false);
            PreviewFrameMetadata metadata = ParseMetadata(encodedMetadata);

            await ReadExactlyAsync(stream, lengthPrefix, "preview JPEG length", assemblyToken)
                .ConfigureAwait(false);
            int jpegLength = ValidateLength(
                BinaryPrimitives.ReadUInt32LittleEndian(lengthPrefix),
                MaxJpegBytes,
                "JPEG");

            byte[] jpeg = new byte[jpegLength];
            await ReadExactlyAsync(stream, jpeg, "preview JPEG", assemblyToken)
                .ConfigureAwait(false);
            ValidateJpeg(jpeg);
            ValidateJpegDimensions(jpeg, metadata);
            return new PreviewFrameData(metadata, jpeg);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            throw new PreviewProtocolException(
                $"The preview frame did not finish within {assemblyTimeout.TotalSeconds:0.#} seconds.");
        }
        catch (EndOfStreamException exception)
        {
            throw new PreviewProtocolException(exception.Message, exception);
        }
    }

    private static PreviewFrameMetadata ParseMetadata(ReadOnlySpan<byte> encodedMetadata)
    {
        PreviewFrameMetadata metadata;
        try
        {
            metadata = JsonSerializer.Deserialize<PreviewFrameMetadata>(encodedMetadata, JsonOptions)
                ?? throw new PreviewProtocolException("Preview metadata was JSON null.");
        }
        catch (JsonException exception)
        {
            throw new PreviewProtocolException(
                $"Preview metadata is not valid protocol-v1 JSON: {Compact(exception.Message)}",
                exception);
        }

        metadata.Validate();
        return metadata;
    }

    private static int ValidateLength(uint length, int maximum, string part)
    {
        if (length == 0)
        {
            throw new PreviewProtocolException($"Preview {part} length must not be zero.");
        }

        if (length > maximum)
        {
            throw new PreviewProtocolException(
                $"Preview {part} length {length:N0} exceeds the {maximum:N0}-byte limit.");
        }

        return checked((int)length);
    }

    private static void ValidateJpeg(ReadOnlySpan<byte> jpeg)
    {
        if (jpeg.Length < 4 ||
            jpeg[0] != 0xff || jpeg[1] != 0xd8 ||
            jpeg[^2] != 0xff || jpeg[^1] != 0xd9)
        {
            throw new PreviewProtocolException(
                "Preview payload does not have JPEG start and end markers.");
        }
    }

    internal static void ValidateJpegDimensions(
        ReadOnlySpan<byte> jpeg,
        PreviewFrameMetadata metadata)
    {
        (uint width, uint height) = ReadJpegDimensions(jpeg);
        if (width != metadata.Width || height != metadata.Height)
        {
            throw new PreviewProtocolException(
                $"JPEG dimensions {width:N0}x{height:N0} do not match metadata {metadata.Width:N0}x{metadata.Height:N0}.");
        }

        if (width > MaxDimension || height > MaxDimension || (ulong)width * height > MaxPixels)
        {
            throw new PreviewProtocolException(
                $"JPEG dimensions {width:N0}x{height:N0} exceed the preview limits.");
        }
    }

    private static (uint Width, uint Height) ReadJpegDimensions(ReadOnlySpan<byte> jpeg)
    {
        int offset = 2;
        while (offset < jpeg.Length - 2)
        {
            if (jpeg[offset] != 0xff)
            {
                throw new PreviewProtocolException("JPEG contains data before its first scan header.");
            }

            while (offset < jpeg.Length && jpeg[offset] == 0xff)
            {
                offset++;
            }
            if (offset >= jpeg.Length)
            {
                break;
            }

            byte marker = jpeg[offset++];
            if (marker == 0x00)
            {
                throw new PreviewProtocolException("JPEG contains an escaped marker before scan data.");
            }

            if (marker is 0xd8 or 0xd9 or 0x01 || marker is >= 0xd0 and <= 0xd7)
            {
                continue;
            }

            if (offset + 2 > jpeg.Length)
            {
                break;
            }

            int segmentLength = BinaryPrimitives.ReadUInt16BigEndian(jpeg[offset..]);
            if (segmentLength < 2 || offset + segmentLength > jpeg.Length)
            {
                throw new PreviewProtocolException("JPEG contains an invalid segment length.");
            }

            if (IsStartOfFrame(marker))
            {
                if (segmentLength < 7)
                {
                    throw new PreviewProtocolException("JPEG start-of-frame header is too short.");
                }

                uint height = BinaryPrimitives.ReadUInt16BigEndian(jpeg[(offset + 3)..]);
                uint width = BinaryPrimitives.ReadUInt16BigEndian(jpeg[(offset + 5)..]);
                if (width == 0 || height == 0)
                {
                    throw new PreviewProtocolException("JPEG dimensions must both be greater than zero.");
                }
                return (width, height);
            }

            if (marker == 0xda)
            {
                throw new PreviewProtocolException("JPEG scan data began before a supported frame header.");
            }

            offset += segmentLength;
        }

        throw new PreviewProtocolException("JPEG does not contain a supported frame header.");
    }

    private static bool IsStartOfFrame(byte marker) => marker is
        0xc0 or 0xc1 or 0xc2 or 0xc3 or
        0xc5 or 0xc6 or 0xc7 or
        0xc9 or 0xca or 0xcb or
        0xcd or 0xce or 0xcf;

    private static void ValidateMonotonicOrder(
        PreviewFrameMetadata metadata,
        ulong? previousSequence,
        ulong? previousSessionGeneration)
    {
        if (previousSequence is ulong sequence && metadata.Sequence <= sequence)
        {
            throw new PreviewProtocolException(
                $"Preview sequence {metadata.Sequence:N0} did not advance beyond {sequence:N0}.");
        }

        if (previousSessionGeneration is ulong generation &&
            metadata.SessionGeneration < generation)
        {
            throw new PreviewProtocolException(
                $"Preview camera session regressed from {generation:N0} to {metadata.SessionGeneration:N0}.");
        }
    }

    private static async Task ReadExactlyAsync(
        Stream stream,
        Memory<byte> destination,
        string part,
        CancellationToken cancellationToken)
    {
        int offset = 0;
        while (offset < destination.Length)
        {
            int bytesRead = await stream
                .ReadAsync(destination[offset..], cancellationToken)
                .ConfigureAwait(false);
            if (bytesRead == 0)
            {
                throw new EndOfStreamException(
                    $"The {part} ended after {offset:N0} of {destination.Length:N0} bytes.");
            }

            offset += bytesRead;
        }
    }

    private static ulong NextEpoch(ulong current)
    {
        ulong next = unchecked(current + 1);
        return next == 0 ? 1 : next;
    }

    private static string Compact(string value)
    {
        string compact = string.Join(
            " ",
            value.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return compact.Length <= 500 ? compact : compact[..500] + "…";
    }
}

internal sealed record PreviewFrameData(PreviewFrameMetadata Metadata, byte[] Jpeg);

internal sealed record PreviewFrame(
    ulong ConnectionEpoch,
    PreviewFrameMetadata Metadata,
    byte[] Jpeg);

internal sealed record PreviewFrameMetadata
{
    [JsonPropertyName("version")]
    [JsonRequired]
    public ushort Version { get; init; }

    [JsonPropertyName("session_generation")]
    [JsonRequired]
    public ulong SessionGeneration { get; init; }

    [JsonPropertyName("sequence")]
    [JsonRequired]
    public ulong Sequence { get; init; }

    [JsonPropertyName("captured_at_unix_ms")]
    [JsonRequired]
    public ulong CapturedAtUnixMs { get; init; }

    [JsonPropertyName("width")]
    [JsonRequired]
    public uint Width { get; init; }

    [JsonPropertyName("height")]
    [JsonRequired]
    public uint Height { get; init; }

    [JsonPropertyName("exposure_us")]
    [JsonRequired]
    public long? ExposureUs { get; init; }

    [JsonPropertyName("gain")]
    [JsonRequired]
    public long? Gain { get; init; }

    [JsonPropertyName("content_type")]
    [JsonRequired]
    public string ContentType { get; init; } = null!;

    [JsonPropertyName("mode")]
    [JsonRequired]
    public string Mode { get; init; } = null!;

    [JsonPropertyName("dropped_frames")]
    [JsonRequired]
    public ulong DroppedFrames { get; init; }

    internal void Validate()
    {
        if (Version != PreviewPipeClient.ProtocolVersion)
        {
            throw new PreviewProtocolException(
                $"Unsupported preview protocol version {Version}; expected {PreviewPipeClient.ProtocolVersion}.");
        }

        if (Width == 0 || Height == 0)
        {
            throw new PreviewProtocolException("Preview dimensions must both be greater than zero.");
        }

        if (Width > PreviewPipeClient.MaxDimension || Height > PreviewPipeClient.MaxDimension)
        {
            throw new PreviewProtocolException(
                $"Preview dimensions {Width:N0}x{Height:N0} exceed the {PreviewPipeClient.MaxDimension:N0}-pixel edge limit.");
        }

        ulong pixels = (ulong)Width * Height;
        if (pixels > PreviewPipeClient.MaxPixels)
        {
            throw new PreviewProtocolException(
                $"Preview contains {pixels:N0} pixels; maximum is {PreviewPipeClient.MaxPixels:N0}.");
        }

        if (ExposureUs is <= 0)
        {
            throw new PreviewProtocolException("Preview exposure must be positive when present.");
        }

        if (Gain is < 0)
        {
            throw new PreviewProtocolException("Preview gain must not be negative when present.");
        }

        if (!string.Equals(ContentType, "image/jpeg", StringComparison.Ordinal))
        {
            throw new PreviewProtocolException(
                $"Unsupported preview content type '{CompactValue(ContentType)}'; expected 'image/jpeg'.");
        }

        if (Mode is not ("unknown" or "day" or "night"))
        {
            throw new PreviewProtocolException(
                $"Unsupported preview mode '{CompactValue(Mode)}'.");
        }

        if (CapturedAtUnixMs > long.MaxValue)
        {
            throw new PreviewProtocolException("Preview capture timestamp is out of range.");
        }

        try
        {
            _ = DateTimeOffset.FromUnixTimeMilliseconds((long)CapturedAtUnixMs);
        }
        catch (ArgumentOutOfRangeException exception)
        {
            throw new PreviewProtocolException("Preview capture timestamp is out of range.", exception);
        }
    }

    internal DateTimeOffset CapturedAt =>
        DateTimeOffset.FromUnixTimeMilliseconds(checked((long)CapturedAtUnixMs));

    private static string CompactValue(string? value)
    {
        if (value is null)
        {
            return "null";
        }

        string compact = string.Join(
            " ",
            value.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return compact.Length <= 100 ? compact : compact[..100] + "…";
    }
}

internal enum PreviewStreamPhase
{
    Connecting,
    WaitingForFrame,
    Live,
    Reconnecting,
}

internal sealed record PreviewStreamState(
    PreviewStreamPhase Phase,
    ulong ConnectionEpoch,
    string? Detail = null,
    TimeSpan? RetryDelay = null);

internal sealed class PreviewProtocolException : Exception
{
    internal PreviewProtocolException(string message)
        : base(message)
    {
    }

    internal PreviewProtocolException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}
