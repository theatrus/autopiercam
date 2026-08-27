using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AutoPierCam.Viewer;

internal sealed class PreviewPipeClient
{
    internal const string DefaultPipeName = "autopiercam-preview-v1";

    internal const ushort ProtocolVersion = 1;
    private const int MaxMetadataBytes = 4 * 1024;
    private const int MaxJpegBytes = 4 * 1024 * 1024;
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

    private readonly string _pipeName;
    private readonly TimeSpan _connectTimeout;
    private readonly TimeSpan _assemblyTimeout;
    private int _running;

    internal PreviewPipeClient(
        string pipeName = DefaultPipeName,
        TimeSpan? connectTimeout = null,
        TimeSpan? assemblyTimeout = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(pipeName);
        _pipeName = pipeName;
        _connectTimeout = connectTimeout ?? TimeSpan.FromSeconds(2);
        _assemblyTimeout = assemblyTimeout ?? TimeSpan.FromSeconds(5);

        if (_connectTimeout <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(connectTimeout));
        }

        if (_assemblyTimeout <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(assemblyTimeout));
        }
    }

    internal string PipeName => _pipeName;

    /// <summary>
    /// Continuously reads the latest preview stream until cancellation. Expected
    /// connection and protocol failures are reported through <paramref name="onStateChanged"/>
    /// and retried with bounded backoff. Callback invocations never overlap.
    /// </summary>
    internal async Task RunAsync(
        Func<PreviewFrame, CancellationToken, Task> onFrame,
        Func<PreviewStreamState, CancellationToken, Task> onStateChanged,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(onFrame);
        ArgumentNullException.ThrowIfNull(onStateChanged);

        if (Interlocked.CompareExchange(ref _running, 1, 0) != 0)
        {
            throw new InvalidOperationException("The preview client is already running.");
        }

        try
        {
            await RunLoopAsync(onFrame, onStateChanged, cancellationToken).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            // Cancellation is the normal lifetime boundary for the persistent stream.
        }
        finally
        {
            Volatile.Write(ref _running, 0);
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
                reconnectDelayIndex = Math.Min(
                    reconnectDelayIndex + 1,
                    ReconnectDelays.Length - 1);
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
                _pipeName,
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
                        previousFailure = "The preview agent closed the stream.";
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

                // A fully validated frame resets the entire reconnect sequence.
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
        using var connectTimeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        connectTimeout.CancelAfter(_connectTimeout);

        try
        {
            await pipe.ConnectAsync(connectTimeout.Token).ConfigureAwait(false);
            return null;
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            return $"Timed out after {_connectTimeout.TotalSeconds:0.#} seconds connecting to preview pipe '{_pipeName}'.";
        }
        catch (TimeoutException)
        {
            return $"Timed out after {_connectTimeout.TotalSeconds:0.#} seconds connecting to preview pipe '{_pipeName}'.";
        }
        catch (IOException exception)
        {
            return $"Could not connect to preview pipe '{_pipeName}': {Compact(exception.Message)}";
        }
        catch (UnauthorizedAccessException exception)
        {
            return $"Access to preview pipe '{_pipeName}' was denied: {Compact(exception.Message)}";
        }
    }

    private async Task<PreviewFrameData?> ReadFrameAsync(
        Stream stream,
        CancellationToken cancellationToken)
    {
        byte[] lengthPrefix = new byte[sizeof(uint)];

        // Waiting for a frame is intentionally unbounded. Once the first byte
        // arrives, however, the complete record must arrive promptly so a
        // partial writer cannot strand this connection forever.
        int firstByte = await stream
            .ReadAsync(lengthPrefix.AsMemory(0, 1), cancellationToken)
            .ConfigureAwait(false);
        if (firstByte == 0)
        {
            return null;
        }

        using var assemblyTimeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        assemblyTimeout.CancelAfter(_assemblyTimeout);
        CancellationToken assemblyToken = assemblyTimeout.Token;

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
            return new PreviewFrameData(metadata, jpeg);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            throw new PreviewProtocolException(
                $"The preview frame did not finish within {_assemblyTimeout.TotalSeconds:0.#} seconds.");
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
            jpeg[0] != 0xff ||
            jpeg[1] != 0xd8 ||
            jpeg[^2] != 0xff ||
            jpeg[^1] != 0xd9)
        {
            throw new PreviewProtocolException(
                "Preview payload does not have JPEG start and end markers.");
        }
    }

    private static void ValidateMonotonicOrder(
        PreviewFrameMetadata metadata,
        ulong? previousSequence,
        ulong? previousSessionGeneration)
    {
        if (previousSequence is ulong sequence && metadata.Sequence <= sequence)
        {
            throw new PreviewProtocolException(
                $"Preview sequence {metadata.Sequence:N0} did not advance beyond {sequence:N0} within one connection.");
        }

        if (previousSessionGeneration is ulong sessionGeneration &&
            metadata.SessionGeneration < sessionGeneration)
        {
            throw new PreviewProtocolException(
                $"Preview session generation regressed from {sessionGeneration:N0} to {metadata.SessionGeneration:N0} within one connection.");
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

    private sealed record PreviewFrameData(PreviewFrameMetadata Metadata, byte[] Jpeg);
}

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
            throw new PreviewProtocolException(
                "Preview dimensions must both be greater than zero.");
        }

        if (Width > PreviewPipeClient.MaxDimension ||
            Height > PreviewPipeClient.MaxDimension)
        {
            throw new PreviewProtocolException(
                $"Preview dimensions {Width:N0}x{Height:N0} exceed the {PreviewPipeClient.MaxDimension:N0}-pixel edge limit.");
        }

        ulong pixels = ulong.CreateChecked(Width) * ulong.CreateChecked(Height);
        if (pixels > PreviewPipeClient.MaxPixels)
        {
            throw new PreviewProtocolException(
                $"Preview contains {pixels:N0} pixels; maximum is {PreviewPipeClient.MaxPixels:N0}.");
        }

        if (ExposureUs is <= 0)
        {
            throw new PreviewProtocolException(
                "Preview exposure must be positive when present.");
        }

        if (Gain is < 0)
        {
            throw new PreviewProtocolException(
                "Preview gain must not be negative when present.");
        }

        if (!string.Equals(ContentType, "image/jpeg", StringComparison.Ordinal))
        {
            throw new PreviewProtocolException(
                $"Unsupported preview content type '{CompactForMessage(ContentType)}'; expected 'image/jpeg'.");
        }

        if (Mode is not ("unknown" or "day" or "night"))
        {
            throw new PreviewProtocolException(
                $"Unsupported preview mode '{CompactForMessage(Mode)}'.");
        }
    }

    private static string CompactForMessage(string? value)
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
