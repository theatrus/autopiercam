using System;
using System.Buffers.Binary;
using System.Diagnostics;
using System.IO;
using System.IO.Pipes;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace AutoPierCam.Preview;

// Deliberately read-only: N.I.N.A. observes the existing camera owner.
internal sealed class ExposureProgressClient
{
    internal const string DefaultPipeName = "autopiercam-control-v1";
    internal const int MaxMessageBytes = 1024 * 1024;
    private readonly string pipeName;
    private readonly TimeSpan connectTimeout;
    private readonly TimeSpan responseTimeout;
    private readonly TimeSpan pollInterval;
    private int running;

    internal ExposureProgressClient(
        string pipeName = DefaultPipeName,
        TimeSpan? connectTimeout = null,
        TimeSpan? responseTimeout = null,
        TimeSpan? pollInterval = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(pipeName);
        this.pipeName = pipeName;
        this.connectTimeout = Positive(connectTimeout ?? TimeSpan.FromSeconds(2), nameof(connectTimeout));
        this.responseTimeout = Positive(responseTimeout ?? TimeSpan.FromSeconds(2), nameof(responseTimeout));
        this.pollInterval = Positive(pollInterval ?? TimeSpan.FromSeconds(2), nameof(pollInterval));
    }

    internal async Task RunAsync(
        Func<ExposureProgressObservation?, CancellationToken, Task> onStatus,
        CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(onStatus);
        if (Interlocked.CompareExchange(ref running, 1, 0) != 0)
        {
            throw new InvalidOperationException("The exposure progress client is already running.");
        }

        try
        {
            while (!cancellationToken.IsCancellationRequested)
            {
                ExposureProgressObservation? observation = null;
                try
                {
                    ExposureProgressStatus status = await GetAsync(cancellationToken).ConfigureAwait(false);
                    observation = new ExposureProgressObservation(status, Stopwatch.GetTimestamp());
                }
                catch (Exception exception) when (exception is IOException or UnauthorizedAccessException or TimeoutException)
                {
                    // Clear old status immediately; the preview stream still reconnects independently.
                }

                await onStatus(observation, cancellationToken).ConfigureAwait(false);
                await Task.Delay(pollInterval, cancellationToken).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
        }
        finally
        {
            Volatile.Write(ref running, 0);
        }
    }

    internal async Task<ExposureProgressStatus> GetAsync(CancellationToken cancellationToken = default)
    {
        await using var pipe = new NamedPipeClientStream(".", pipeName, PipeDirection.InOut, PipeOptions.Asynchronous);
        using (var connect = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
        {
            connect.CancelAfter(connectTimeout);
            try
            {
                await pipe.ConnectAsync(connect.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
            {
                throw new TimeoutException("Timed out connecting to AutoPierCam exposure status.");
            }
        }

        using var response = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        response.CancelAfter(responseTimeout);
        string requestId = Guid.NewGuid().ToString("N");
        byte[] body = JsonSerializer.SerializeToUtf8Bytes(new
        {
            version = 1,
            request_id = requestId,
            method = "status.get",
            payload = new { },
        });
        byte[] prefix = new byte[sizeof(uint)];
        BinaryPrimitives.WriteUInt32LittleEndian(prefix, checked((uint)body.Length));
        try
        {
            await pipe.WriteAsync(prefix, response.Token).ConfigureAwait(false);
            await pipe.WriteAsync(body, response.Token).ConfigureAwait(false);
            await pipe.FlushAsync(response.Token).ConfigureAwait(false);
            return await ReadResponseAsync(pipe, requestId, response.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            throw new TimeoutException("Timed out waiting for AutoPierCam exposure status.");
        }
    }

    internal static async Task<ExposureProgressStatus> ReadResponseAsync(
        Stream stream,
        string requestId,
        CancellationToken cancellationToken = default)
    {
        byte[] prefix = new byte[sizeof(uint)];
        await stream.ReadExactlyAsync(prefix, cancellationToken).ConfigureAwait(false);
        uint length = BinaryPrimitives.ReadUInt32LittleEndian(prefix);
        if (length == 0 || length > MaxMessageBytes)
        {
            throw new ExposureProgressProtocolException("Exposure status response exceeds the 1 MiB protocol limit or is empty.");
        }

        byte[] body = new byte[checked((int)length)];
        await stream.ReadExactlyAsync(body, cancellationToken).ConfigureAwait(false);
        return ParseResponse(body, requestId);
    }

    internal static ExposureProgressStatus ParseResponse(ReadOnlyMemory<byte> body, string requestId)
    {
        try
        {
            using JsonDocument document = JsonDocument.Parse(body);
            JsonElement root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                throw new ExposureProgressProtocolException("Exposure status response must be an object.");
            }

            JsonElement result = default;
            int versions = 0, ids = 0, results = 0, errors = 0;
            foreach (JsonProperty property in root.EnumerateObject())
            {
                if (property.NameEquals("version"))
                {
                    versions++;
                    if (property.Value.ValueKind != JsonValueKind.Number ||
                        !property.Value.TryGetInt32(out int version) || version != 1)
                    {
                        throw new ExposureProgressProtocolException("Exposure status response has an unsupported version.");
                    }
                }
                else if (property.NameEquals("request_id"))
                {
                    ids++;
                    if (property.Value.ValueKind != JsonValueKind.String || property.Value.GetString() != requestId)
                    {
                        throw new ExposureProgressProtocolException("Exposure status request_id does not match.");
                    }
                }
                else if (property.NameEquals("result"))
                {
                    results++;
                    result = property.Value;
                }
                else if (property.NameEquals("error"))
                {
                    errors++;
                }
            }

            if (versions != 1 || ids != 1 || results != 1 || errors != 0)
            {
                throw new ExposureProgressProtocolException("Exposure status response did not contain one successful result.");
            }

            return ExposureProgressStatus.ParseStatus(result);
        }
        catch (JsonException exception)
        {
            throw new ExposureProgressProtocolException("Exposure status response is not valid JSON.", exception);
        }
    }

    private static TimeSpan Positive(TimeSpan value, string parameter) =>
        value > TimeSpan.Zero ? value : throw new ArgumentOutOfRangeException(parameter);
}

internal sealed class ExposureProgressProtocolException : IOException
{
    internal ExposureProgressProtocolException(string message, Exception? inner = null) : base(message, inner)
    {
    }
}
