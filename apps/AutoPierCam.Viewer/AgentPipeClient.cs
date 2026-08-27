using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AutoPierCam.Viewer;

internal sealed class AgentPipeClient : IAsyncDisposable
{
    internal const string DefaultPipeName = "autopiercam-control-v1";
    private const int ProtocolVersion = 1;
    private const int MaxMessageBytes = 1024 * 1024;

    private static readonly JsonSerializerOptions JsonOptions = new()
    {
        PropertyNameCaseInsensitive = false,
    };

    private readonly string _pipeName;
    private readonly TimeSpan _connectTimeout;
    private readonly TimeSpan _requestTimeout;
    private readonly SemaphoreSlim _requestGate = new(1, 1);
    private bool _disposed;

    internal AgentPipeClient(
        string pipeName = DefaultPipeName,
        TimeSpan? connectTimeout = null,
        TimeSpan? requestTimeout = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(pipeName);
        _pipeName = pipeName;
        _connectTimeout = connectTimeout ?? TimeSpan.FromSeconds(2);
        _requestTimeout = requestTimeout ?? TimeSpan.FromSeconds(30);

        if (_connectTimeout <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(connectTimeout));
        }

        if (_requestTimeout <= TimeSpan.Zero)
        {
            throw new ArgumentOutOfRangeException(nameof(requestTimeout));
        }
    }

    internal string PipeName => _pipeName;

    internal async Task<AgentStatus> GetStatusAsync(CancellationToken cancellationToken = default)
    {
        JsonElement result = await RequestAsync("status.get", cancellationToken).ConfigureAwait(false);
        try
        {
            AgentStatus? status = result.Deserialize<AgentStatus>(JsonOptions);
            if (status is null || string.IsNullOrWhiteSpace(status.State))
            {
                throw new AgentProtocolException("status.get returned no state.");
            }

            if (status.Camera is not null && string.IsNullOrWhiteSpace(status.Camera.Name))
            {
                throw new AgentProtocolException("status.get returned a camera without a name.");
            }

            return status;
        }
        catch (JsonException exception)
        {
            throw new AgentProtocolException("status.get returned an invalid result object.", exception);
        }
    }

    internal async Task CaptureNowAsync(CancellationToken cancellationToken = default)
    {
        _ = await RequestAsync("capture.now", cancellationToken).ConfigureAwait(false);
    }

    internal async Task<JsonElement> RequestAsync(
        string method,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(method);
        ThrowIfDisposed();

        await _requestGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            ThrowIfDisposed();
            return await RequestCoreAsync(method, cancellationToken).ConfigureAwait(false);
        }
        finally
        {
            _requestGate.Release();
        }
    }

    private async Task<JsonElement> RequestCoreAsync(
        string method,
        CancellationToken cancellationToken)
    {
        string requestId = Guid.NewGuid().ToString("N");
        byte[] requestBody = JsonSerializer.SerializeToUtf8Bytes(
            new AgentRequest(ProtocolVersion, requestId, method, new EmptyPayload()),
            JsonOptions);
        if (requestBody.Length > MaxMessageBytes)
        {
            throw new AgentProtocolException(
                $"Request is {requestBody.Length:N0} bytes; the protocol limit is {MaxMessageBytes:N0} bytes.");
        }

        await using var pipe = new NamedPipeClientStream(
            ".",
            _pipeName,
            PipeDirection.InOut,
            PipeOptions.Asynchronous);

        using (var connectTimeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken))
        {
            connectTimeout.CancelAfter(_connectTimeout);
            try
            {
                await pipe.ConnectAsync(connectTimeout.Token).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
            {
                throw new AgentUnavailableException(
                    $"Timed out after {_connectTimeout.TotalSeconds:0.#} seconds connecting to pipe '{_pipeName}'.");
            }
            catch (IOException exception)
            {
                throw new AgentUnavailableException(
                    $"Could not connect to pipe '{_pipeName}': {exception.Message}",
                    exception);
            }
            catch (UnauthorizedAccessException exception)
            {
                throw new AgentUnavailableException(
                    $"Access to pipe '{_pipeName}' was denied.",
                    exception);
            }
        }

        using var requestTimeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
        requestTimeout.CancelAfter(_requestTimeout);
        try
        {
            byte[] requestHeader = new byte[sizeof(uint)];
            BinaryPrimitives.WriteUInt32LittleEndian(requestHeader, checked((uint)requestBody.Length));
            await pipe.WriteAsync(requestHeader, requestTimeout.Token).ConfigureAwait(false);
            await pipe.WriteAsync(requestBody, requestTimeout.Token).ConfigureAwait(false);
            await pipe.FlushAsync(requestTimeout.Token).ConfigureAwait(false);

            byte[] responseHeader = new byte[sizeof(uint)];
            await ReadExactlyAsync(pipe, responseHeader, requestTimeout.Token).ConfigureAwait(false);
            uint responseLength = BinaryPrimitives.ReadUInt32LittleEndian(responseHeader);
            if (responseLength == 0 || responseLength > MaxMessageBytes)
            {
                throw new AgentProtocolException(
                    $"Agent response length {responseLength:N0} is outside the allowed 1..={MaxMessageBytes:N0} byte range.");
            }

            byte[] responseBody = new byte[checked((int)responseLength)];
            await ReadExactlyAsync(pipe, responseBody, requestTimeout.Token).ConfigureAwait(false);
            return ParseResponse(responseBody, requestId);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            throw new AgentTimeoutException(
                $"The agent did not answer within {_requestTimeout.TotalSeconds:0.#} seconds; the request may still complete.");
        }
        catch (EndOfStreamException exception)
        {
            throw new AgentUnavailableException(
                "The capture agent disconnected before returning a complete response.",
                exception);
        }
        catch (IOException exception)
        {
            throw new AgentUnavailableException(
                $"Communication with the capture agent failed: {exception.Message}",
                exception);
        }
    }

    private static JsonElement ParseResponse(ReadOnlyMemory<byte> responseBody, string requestId)
    {
        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(responseBody);
        }
        catch (JsonException exception)
        {
            throw new AgentProtocolException("Agent response was not valid UTF-8 JSON.", exception);
        }

        using (document)
        {
            JsonElement root = document.RootElement;
            if (root.ValueKind is not JsonValueKind.Object)
            {
                throw new AgentProtocolException("Agent response must be a JSON object.");
            }

            if (!root.TryGetProperty("version", out JsonElement version) ||
                version.ValueKind is not JsonValueKind.Number ||
                !version.TryGetInt32(out int versionNumber) ||
                versionNumber != ProtocolVersion)
            {
                throw new AgentProtocolException($"Agent response did not use protocol version {ProtocolVersion}.");
            }

            if (!root.TryGetProperty("request_id", out JsonElement responseRequestId) ||
                responseRequestId.ValueKind is not JsonValueKind.String ||
                !string.Equals(responseRequestId.GetString(), requestId, StringComparison.Ordinal))
            {
                throw new AgentProtocolException("Agent response request_id did not match the request.");
            }

            int resultCount = 0;
            int errorCount = 0;
            JsonElement result = default;
            JsonElement error = default;
            foreach (JsonProperty property in root.EnumerateObject())
            {
                if (property.NameEquals("result"))
                {
                    resultCount++;
                    result = property.Value;
                }
                else if (property.NameEquals("error"))
                {
                    errorCount++;
                    error = property.Value;
                }
            }

            if (resultCount + errorCount != 1)
            {
                throw new AgentProtocolException(
                    "Agent response must contain exactly one result or error property.");
            }

            if (errorCount == 1)
            {
                throw AgentRequestException.FromJson(error);
            }

            return result.Clone();
        }
    }

    private static async Task ReadExactlyAsync(
        Stream stream,
        Memory<byte> destination,
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
                throw new EndOfStreamException();
            }

            offset += bytesRead;
        }
    }

    private void ThrowIfDisposed()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
    }

    public async ValueTask DisposeAsync()
    {
        await _requestGate.WaitAsync().ConfigureAwait(false);
        try
        {
            _disposed = true;
        }
        finally
        {
            _requestGate.Release();
        }
    }

    private sealed record EmptyPayload;

    private sealed record AgentRequest(
        [property: JsonPropertyName("version")] int Version,
        [property: JsonPropertyName("request_id")] string RequestId,
        [property: JsonPropertyName("method")] string Method,
        [property: JsonPropertyName("payload")] EmptyPayload Payload);
}

internal sealed record AgentStatus
{
    [JsonPropertyName("state")]
    [JsonRequired]
    public string State { get; init; } = string.Empty;

    [JsonPropertyName("camera")]
    public AgentCameraStatus? Camera { get; init; }

    [JsonPropertyName("frames_captured")]
    [JsonRequired]
    public ulong FramesCaptured { get; init; }

    [JsonPropertyName("frames_saved")]
    [JsonRequired]
    public ulong FramesSaved { get; init; }

    [JsonPropertyName("last_artifact")]
    public string? LastArtifact { get; init; }

    [JsonPropertyName("last_error")]
    public string? LastError { get; init; }
}

internal sealed record AgentCameraStatus
{
    [JsonPropertyName("id")]
    [JsonRequired]
    public int Id { get; init; }

    [JsonPropertyName("name")]
    [JsonRequired]
    public string Name { get; init; } = string.Empty;
}

internal abstract class AgentClientException : Exception
{
    protected AgentClientException(string message)
        : base(message)
    {
    }

    protected AgentClientException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}

internal sealed class AgentUnavailableException : AgentClientException
{
    internal AgentUnavailableException(string message)
        : base(message)
    {
    }

    internal AgentUnavailableException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}

internal sealed class AgentTimeoutException : AgentClientException
{
    internal AgentTimeoutException(string message)
        : base(message)
    {
    }
}

internal sealed class AgentProtocolException : AgentClientException
{
    internal AgentProtocolException(string message)
        : base(message)
    {
    }

    internal AgentProtocolException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}

internal sealed class AgentRequestException : AgentClientException
{
    private AgentRequestException(string message)
        : base(message)
    {
    }

    internal static AgentRequestException FromJson(JsonElement error)
    {
        if (error.ValueKind is JsonValueKind.String)
        {
            return new AgentRequestException(error.GetString() ?? "The capture agent rejected the request.");
        }

        if (error.ValueKind is JsonValueKind.Object)
        {
            string? code = error.TryGetProperty("code", out JsonElement codeValue) &&
                           codeValue.ValueKind is JsonValueKind.String
                ? codeValue.GetString()
                : null;
            string? message = error.TryGetProperty("message", out JsonElement messageValue) &&
                              messageValue.ValueKind is JsonValueKind.String
                ? messageValue.GetString()
                : null;

            if (!string.IsNullOrWhiteSpace(code) && !string.IsNullOrWhiteSpace(message))
            {
                return new AgentRequestException($"{code}: {message}");
            }

            if (!string.IsNullOrWhiteSpace(message))
            {
                return new AgentRequestException(message);
            }

            if (!string.IsNullOrWhiteSpace(code))
            {
                return new AgentRequestException(code);
            }
        }

        string rawError = error.GetRawText();
        if (rawError.Length > 500)
        {
            rawError = rawError[..500] + "…";
        }

        return new AgentRequestException($"The capture agent rejected the request: {rawError}");
    }
}
