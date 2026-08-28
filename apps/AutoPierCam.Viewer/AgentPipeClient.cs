using System.Buffers.Binary;
using System.IO.Pipes;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AutoPierCam.Viewer;

internal sealed class AgentPipeClient : IAsyncDisposable
{
    internal const string DefaultPipeName = "autopiercam-control-v1";
    internal const string UploadsListCapability = "uploads.list";
    internal const string UploadsRequeueCapability = "uploads.requeue";
    private const int ProtocolVersion = 1;
    private const int MaxMessageBytes = 1024 * 1024;
    private static readonly HashSet<string> KnownUploadStates = new(StringComparer.Ordinal)
    {
        "pending",
        "in_progress",
        "retrying",
        "completed",
        "permanently_failed",
    };

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
        AgentStatus status = DeserializeResult<AgentStatus>(result, "status.get");
        if (string.IsNullOrWhiteSpace(status.State))
        {
            throw new AgentProtocolException("status.get returned no state.");
        }

        if (status.Camera is not null && string.IsNullOrWhiteSpace(status.Camera.Name))
        {
            throw new AgentProtocolException("status.get returned a camera without a name.");
        }

        if (status.Capabilities is null ||
            status.Capabilities.Any(string.IsNullOrWhiteSpace))
        {
            throw new AgentProtocolException("status.get returned an invalid capabilities list.");
        }

        return status;
    }

    internal async Task<UploadListResult> ListUploadsAsync(
        IReadOnlyList<string> states,
        ushort pageSize,
        string? cursor = null,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(states);
        if (pageSize is < 1 or > 100)
        {
            throw new ArgumentOutOfRangeException(
                nameof(pageSize),
                "Upload page size must be between 1 and 100.");
        }

        if (states.Count == 0 ||
            states.Any(state => !KnownUploadStates.Contains(state)) ||
            states.Distinct(StringComparer.Ordinal).Count() != states.Count)
        {
            throw new ArgumentException(
                "At least one non-empty upload state is required.",
                nameof(states));
        }

        if (cursor is not null && string.IsNullOrWhiteSpace(cursor))
        {
            throw new ArgumentException("Upload cursor cannot be empty.", nameof(cursor));
        }

        JsonElement result = await RequestAsync(
                UploadsListCapability,
                new UploadListRequest(states, pageSize, cursor),
                cancellationToken)
            .ConfigureAwait(false);
        UploadListResult page = DeserializeResult<UploadListResult>(result, UploadsListCapability);
        page.Validate();
        return page;
    }

    internal async Task<UploadRequeueResult> RequeueUploadAsync(
        string ledgerId,
        ulong jobId,
        ulong expectedJobRevision,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(ledgerId);
        if (jobId == 0)
        {
            throw new ArgumentOutOfRangeException(nameof(jobId));
        }

        if (expectedJobRevision == 0)
        {
            throw new ArgumentOutOfRangeException(nameof(expectedJobRevision));
        }

        JsonElement result = await RequestAsync(
                UploadsRequeueCapability,
                new UploadRequeueRequest(ledgerId, jobId, expectedJobRevision),
                cancellationToken)
            .ConfigureAwait(false);
        UploadRequeueResult requeue =
            DeserializeResult<UploadRequeueResult>(result, UploadsRequeueCapability);
        requeue.Validate();
        if (requeue.Job.JobId != jobId ||
            requeue.Job.JobRevision <= expectedJobRevision ||
            !string.Equals(requeue.Job.State, "pending", StringComparison.Ordinal) ||
            requeue.Job.RequeueEligible)
        {
            throw new AgentProtocolException(
                "uploads.requeue returned a job that does not match the accepted requeue transition.");
        }
        return requeue;
    }

    internal async Task CaptureNowAsync(CancellationToken cancellationToken = default)
    {
        _ = await RequestAsync("capture.now", cancellationToken).ConfigureAwait(false);
    }

    internal async Task<AgentConfigurationSnapshot> GetConfigurationAsync(
        CancellationToken cancellationToken = default)
    {
        JsonElement result = await RequestAsync("config.get", cancellationToken).ConfigureAwait(false);
        AgentConfigurationSnapshot snapshot =
            DeserializeResult<AgentConfigurationSnapshot>(result, "config.get");
        snapshot.Config.Validate("config.get");
        return snapshot;
    }

    internal async Task<AgentConfigurationReplaceResult> ReplaceConfigurationAsync(
        ulong expectedRevision,
        AgentConfiguration configuration,
        CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(configuration);
        configuration.Validate("config.replace");
        JsonElement result = await RequestAsync(
                "config.replace",
                new AgentConfigurationReplacePayload(expectedRevision, configuration),
                cancellationToken)
            .ConfigureAwait(false);
        AgentConfigurationReplaceResult replaceResult =
            DeserializeResult<AgentConfigurationReplaceResult>(result, "config.replace");
        if (!replaceResult.Saved || !replaceResult.RestartScheduled)
        {
            throw new AgentProtocolException(
                "config.replace returned success without confirming both the save and scheduled restart.");
        }

        return replaceResult;
    }

    internal async Task<JsonElement> RequestAsync(
        string method,
        CancellationToken cancellationToken = default)
    {
        return await RequestAsync(method, new EmptyPayload(), cancellationToken).ConfigureAwait(false);
    }

    internal async Task<JsonElement> RequestAsync(
        string method,
        object payload,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(method);
        ArgumentNullException.ThrowIfNull(payload);
        ThrowIfDisposed();

        await _requestGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            ThrowIfDisposed();
            return await RequestCoreAsync(method, payload, cancellationToken).ConfigureAwait(false);
        }
        finally
        {
            _requestGate.Release();
        }
    }

    private async Task<JsonElement> RequestCoreAsync(
        string method,
        object payload,
        CancellationToken cancellationToken)
    {
        string requestId = Guid.NewGuid().ToString("N");
        byte[] requestBody = JsonSerializer.SerializeToUtf8Bytes(
            new AgentRequest(ProtocolVersion, requestId, method, payload),
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

    private static T DeserializeResult<T>(JsonElement result, string method)
    {
        try
        {
            T? value = result.Deserialize<T>(JsonOptions);
            return value ?? throw new AgentProtocolException(
                $"{method} returned a null result instead of {typeof(T).Name}.");
        }
        catch (JsonException exception)
        {
            throw new AgentProtocolException(
                $"{method} returned an invalid {typeof(T).Name} result object.",
                exception);
        }
    }

    internal static bool IsKnownUploadState(string state)
    {
        return KnownUploadStates.Contains(state);
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
        [property: JsonPropertyName("payload")] object Payload);
}

internal sealed record AgentConfigurationSnapshot
{
    [JsonPropertyName("revision")]
    [JsonRequired]
    public ulong Revision { get; init; }

    [JsonPropertyName("config")]
    [JsonRequired]
    public AgentConfiguration Config { get; init; } = null!;
}

internal sealed record AgentConfigurationReplacePayload(
    [property: JsonPropertyName("expected_revision")] ulong ExpectedRevision,
    [property: JsonPropertyName("config")] AgentConfiguration Config);

internal sealed record AgentConfigurationReplaceResult
{
    [JsonPropertyName("revision")]
    [JsonRequired]
    public ulong Revision { get; init; }

    [JsonPropertyName("saved")]
    [JsonRequired]
    public bool Saved { get; init; }

    [JsonPropertyName("restart_scheduled")]
    [JsonRequired]
    public bool RestartScheduled { get; init; }
}

internal sealed record UploadListRequest(
    [property: JsonPropertyName("states")] IReadOnlyList<string> States,
    [property: JsonPropertyName("page_size")] ushort PageSize,
    [property: JsonPropertyName("cursor"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
        string? Cursor);

internal sealed record UploadListResult
{
    [JsonPropertyName("ledger_id")]
    [JsonRequired]
    public string LedgerId { get; init; } = string.Empty;

    [JsonPropertyName("jobs")]
    [JsonRequired]
    public IReadOnlyList<UploadJobSummary> Jobs { get; init; } = null!;

    [JsonPropertyName("next_cursor")]
    public string? NextCursor { get; init; }

    internal void Validate()
    {
        if (string.IsNullOrWhiteSpace(LedgerId) || Jobs is null)
        {
            throw new AgentProtocolException(
                "uploads.list returned an invalid ledger identifier or jobs list.");
        }

        if (NextCursor is not null && string.IsNullOrWhiteSpace(NextCursor))
        {
            throw new AgentProtocolException("uploads.list returned an empty continuation cursor.");
        }


        if (Jobs.Count > 100 || Jobs.Select(job => job.JobId).Distinct().Count() != Jobs.Count)
        {
            throw new AgentProtocolException(
                "uploads.list returned too many jobs or duplicate job identifiers.");
        }

        foreach (UploadJobSummary job in Jobs)
        {
            job.Validate("uploads.list");
        }
    }
}

internal sealed record UploadRequeueRequest(
    [property: JsonPropertyName("ledger_id")] string LedgerId,
    [property: JsonPropertyName("job_id")] ulong JobId,
    [property: JsonPropertyName("expected_job_revision")] ulong ExpectedJobRevision);

internal sealed record UploadRequeueResult
{
    [JsonPropertyName("job")]
    [JsonRequired]
    public UploadJobSummary Job { get; init; } = null!;

    [JsonPropertyName("worker_notified")]
    [JsonRequired]
    public bool WorkerNotified { get; init; }

    internal void Validate()
    {
        if (Job is null)
        {
            throw new AgentProtocolException("uploads.requeue returned no upload job.");
        }

        Job.Validate("uploads.requeue");
    }
}

internal sealed record UploadJobSummary
{
    [JsonPropertyName("job_id")]
    [JsonRequired]
    public ulong JobId { get; init; }

    [JsonPropertyName("job_revision")]
    [JsonRequired]
    public ulong JobRevision { get; init; }

    [JsonPropertyName("filename")]
    [JsonRequired]
    public string Filename { get; init; } = string.Empty;

    [JsonPropertyName("state")]
    [JsonRequired]
    public string State { get; init; } = string.Empty;

    [JsonPropertyName("file_size_bytes")]
    [JsonRequired]
    public ulong FileSizeBytes { get; init; }

    [JsonPropertyName("attempt_count")]
    [JsonRequired]
    public ulong AttemptCount { get; init; }

    [JsonPropertyName("requeue_count")]
    [JsonRequired]
    public ulong RequeueCount { get; init; }

    [JsonPropertyName("created_at_unix_ms")]
    [JsonRequired]
    public ulong CreatedAtUnixMs { get; init; }

    [JsonPropertyName("updated_at_unix_ms")]
    [JsonRequired]
    public ulong UpdatedAtUnixMs { get; init; }

    [JsonPropertyName("next_attempt_at_unix_ms")]
    public ulong? NextAttemptAtUnixMs { get; init; }

    [JsonPropertyName("completed_at_unix_ms")]
    public ulong? CompletedAtUnixMs { get; init; }

    [JsonPropertyName("last_failure_at_unix_ms")]
    public ulong? LastFailureAtUnixMs { get; init; }

    [JsonPropertyName("last_requeued_at_unix_ms")]
    public ulong? LastRequeuedAtUnixMs { get; init; }

    [JsonPropertyName("last_http_status")]
    public ushort? LastHttpStatus { get; init; }

    [JsonPropertyName("last_error")]
    public string? LastError { get; init; }

    [JsonPropertyName("requeue_eligible")]
    [JsonRequired]
    public bool RequeueEligible { get; init; }

    internal void Validate(string method)
    {
        bool knownState = AgentPipeClient.IsKnownUploadState(State);
        bool historyTimestampsValid =
            UpdatedAtUnixMs >= CreatedAtUnixMs &&
            IsHistoryTimestampValid(CompletedAtUnixMs) &&
            IsHistoryTimestampValid(LastFailureAtUnixMs) &&
            IsHistoryTimestampValid(LastRequeuedAtUnixMs);
        if (JobId == 0 ||
            JobRevision == 0 ||
            string.IsNullOrWhiteSpace(Filename) ||
            Filename.IndexOfAny(['/', '\\']) >= 0 ||
            !knownState ||
            !historyTimestampsValid ||
            (State == "retrying") != NextAttemptAtUnixMs.HasValue ||
            NextAttemptAtUnixMs is ulong nextAttempt && nextAttempt < UpdatedAtUnixMs ||
            (State == "completed") != CompletedAtUnixMs.HasValue ||
            (RequeueCount > 0) != LastRequeuedAtUnixMs.HasValue ||
            RequeueEligible && State != "permanently_failed" ||
            (LastHttpStatus is ushort httpStatus &&
             (httpStatus < 100 || httpStatus > 599)))
        {
            throw new AgentProtocolException($"{method} returned an invalid upload job.");
        }

        return;

        bool IsHistoryTimestampValid(ulong? timestamp)
        {
            return timestamp is null ||
                   timestamp >= CreatedAtUnixMs && timestamp <= UpdatedAtUnixMs;
        }
    }
}

internal sealed record AgentConfiguration
{
    [JsonPropertyName("camera")]
    [JsonRequired]
    public AgentCameraConfiguration Camera { get; init; } = null!;

    [JsonPropertyName("capture")]
    [JsonRequired]
    public AgentCaptureConfiguration Capture { get; init; } = null!;

    [JsonPropertyName("upload")]
    [JsonRequired]
    public AgentUploadConfiguration Upload { get; init; } = null!;

    [JsonPropertyName("video")]
    [JsonRequired]
    public AgentVideoConfiguration Video { get; init; } = null!;

    [JsonPropertyName("api")]
    [JsonRequired]
    public AgentApiConfiguration Api { get; init; } = null!;

    internal void Validate(string method)
    {
        if (Camera is null || Capture is null || Upload is null || Video is null || Api is null)
        {
            throw new AgentProtocolException($"{method} did not contain every configuration section.");
        }

        if (string.IsNullOrWhiteSpace(Capture.Directory))
        {
            throw new AgentProtocolException($"{method} returned an empty capture directory.");
        }

        if (string.IsNullOrWhiteSpace(Api.Listen))
        {
            throw new AgentProtocolException($"{method} returned an empty API listen address.");
        }

        if (Camera.MaxExposureUs < Camera.MinExposureUs)
        {
            throw new AgentProtocolException(
                $"{method} returned max_exposure_us below min_exposure_us.");
        }

        if (Camera.MinExposureUs < 0 ||
            Camera.MaxExposureUs <= 0 ||
            Camera.MaxGain < 0)
        {
            throw new AgentProtocolException(
                $"{method} returned negative camera limits or a non-positive maximum exposure.");
        }

        if (Capture.IntervalMs == 0 ||
            Capture.JpegQuality is < 1 or > 100 ||
            Capture.WriterQueueCapacity == 0)
        {
            throw new AgentProtocolException(
                $"{method} returned invalid capture timing, JPEG quality, or writer capacity.");
        }

        if (Capture.RetentionMaxBytes == 0 || Capture.RetentionMinFreeBytes == 0)
        {
            throw new AgentProtocolException(
                $"{method} returned a zero retention byte limit; use null to disable a limit.");
        }

        if (Upload.QueueCapacity == 0)
        {
            throw new AgentProtocolException(
                $"{method} returned a zero upload queue capacity.");
        }

        Uri? uploadEndpoint = null;
        if (Upload.Endpoint is string endpoint)
        {
            if (string.IsNullOrWhiteSpace(endpoint) ||
                endpoint.Trim() != endpoint ||
                !Uri.TryCreate(endpoint, UriKind.Absolute, out uploadEndpoint) ||
                (uploadEndpoint.Scheme != Uri.UriSchemeHttp &&
                 uploadEndpoint.Scheme != Uri.UriSchemeHttps) ||
                string.IsNullOrWhiteSpace(uploadEndpoint.Host) ||
                uploadEndpoint.UserInfo.Length != 0 ||
                uploadEndpoint.Fragment.Length != 0)
            {
                throw new AgentProtocolException(
                    $"{method} returned an invalid upload endpoint.");
            }
        }

        if (Upload.Enabled && uploadEndpoint is null)
        {
            throw new AgentProtocolException(
                $"{method} enabled upload without an endpoint.");
        }

        if (Upload.BearerTokenEnvironment is string bearerEnvironment)
        {
            if (string.IsNullOrWhiteSpace(bearerEnvironment) ||
                bearerEnvironment.Trim() != bearerEnvironment ||
                bearerEnvironment.Contains('=') ||
                bearerEnvironment.Contains('\0') ||
                bearerEnvironment.Any(char.IsControl))
            {
                throw new AgentProtocolException(
                    $"{method} returned an invalid bearer-token environment reference.");
            }

            if (uploadEndpoint is not null && uploadEndpoint.Scheme != Uri.UriSchemeHttps)
            {
                throw new AgentProtocolException(
                    $"{method} configured bearer authentication over a non-HTTPS upload endpoint.");
            }
        }
    }
}

internal sealed record AgentCameraConfiguration
{
    [JsonPropertyName("camera_id")]
    public int? CameraId { get; init; }

    [JsonPropertyName("name_contains")]
    public string? NameContains { get; init; }

    [JsonPropertyName("width")]
    public uint? Width { get; init; }

    [JsonPropertyName("height")]
    public uint? Height { get; init; }

    [JsonPropertyName("bin")]
    [JsonRequired]
    public int Bin { get; init; }

    [JsonPropertyName("min_exposure_us")]
    [JsonRequired]
    public long MinExposureUs { get; init; }

    [JsonPropertyName("max_exposure_us")]
    [JsonRequired]
    public long MaxExposureUs { get; init; }

    [JsonPropertyName("max_gain")]
    [JsonRequired]
    public long MaxGain { get; init; }

    [JsonPropertyName("target_brightness")]
    [JsonRequired]
    public long TargetBrightness { get; init; }

    [JsonPropertyName("settle_frames")]
    [JsonRequired]
    public uint SettleFrames { get; init; }
}

internal sealed record AgentCaptureConfiguration
{
    [JsonPropertyName("directory")]
    [JsonRequired]
    public string Directory { get; init; } = string.Empty;

    [JsonPropertyName("interval_ms")]
    [JsonRequired]
    public ulong IntervalMs { get; init; }

    [JsonPropertyName("jpeg_quality")]
    [JsonRequired]
    public byte JpegQuality { get; init; }

    [JsonPropertyName("writer_queue_capacity")]
    [JsonRequired]
    public ulong WriterQueueCapacity { get; init; }

    [JsonPropertyName("keep_latest")]
    [JsonRequired]
    public bool KeepLatest { get; init; }

    [JsonPropertyName("retention_days")]
    [JsonRequired]
    public uint RetentionDays { get; init; }

    [JsonPropertyName("retention_max_bytes")]
    public ulong? RetentionMaxBytes { get; init; }

    [JsonPropertyName("retention_min_free_bytes")]
    public ulong? RetentionMinFreeBytes { get; init; }
}

internal sealed record AgentUploadConfiguration
{
    [JsonPropertyName("enabled")]
    [JsonRequired]
    public bool Enabled { get; init; }

    [JsonPropertyName("endpoint")]
    public string? Endpoint { get; init; }

    [JsonPropertyName("bearer_token_env")]
    public string? BearerTokenEnvironment { get; init; }

    [JsonPropertyName("queue_capacity")]
    [JsonRequired]
    public ulong QueueCapacity { get; init; }
}

internal sealed record AgentVideoConfiguration
{
    [JsonPropertyName("enabled")]
    [JsonRequired]
    public bool Enabled { get; init; }

    [JsonPropertyName("segment_seconds")]
    [JsonRequired]
    public uint SegmentSeconds { get; init; }

    [JsonPropertyName("frames_per_second")]
    [JsonRequired]
    public uint FramesPerSecond { get; init; }
}

internal sealed record AgentApiConfiguration
{
    [JsonPropertyName("listen")]
    [JsonRequired]
    public string Listen { get; init; } = string.Empty;
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

    [JsonPropertyName("upload")]
    public AgentUploadStatus? Upload { get; init; }

    [JsonPropertyName("capabilities")]
    public IReadOnlyList<string> Capabilities { get; init; } = Array.Empty<string>();

    internal bool HasCapability(string capability)
    {
        return Capabilities.Contains(capability, StringComparer.Ordinal);
    }
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

internal sealed record AgentUploadStatus
{
    [JsonPropertyName("pending")]
    [JsonRequired]
    public ulong Pending { get; init; }

    [JsonPropertyName("active")]
    [JsonRequired]
    public ulong Active { get; init; }

    [JsonPropertyName("retrying")]
    [JsonRequired]
    public ulong Retrying { get; init; }

    [JsonPropertyName("completed")]
    [JsonRequired]
    public ulong Completed { get; init; }

    [JsonPropertyName("permanently_failed")]
    [JsonRequired]
    public ulong PermanentlyFailed { get; init; }

    [JsonPropertyName("last_success_unix_ms")]
    public ulong? LastSuccessUnixMs { get; init; }

    [JsonPropertyName("last_failure_unix_ms")]
    public ulong? LastFailureUnixMs { get; init; }

    [JsonPropertyName("last_error")]
    public string? LastError { get; init; }
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
    private AgentRequestException(
        string code,
        string message,
        JsonElement? details,
        ulong? currentRevision)
        : base(message)
    {
        Code = code;
        Details = details;
        CurrentRevision = currentRevision;
    }

    internal string Code { get; }

    internal JsonElement? Details { get; }

    internal ulong? CurrentRevision { get; }

    internal bool IsRevisionConflict =>
        string.Equals(Code, "revision_conflict", StringComparison.Ordinal);

    internal bool IsSavedWithoutRestart =>
        string.Equals(Code, "config_saved_agent_stopped", StringComparison.Ordinal);

    internal static AgentRequestException FromJson(JsonElement error)
    {
        if (error.ValueKind is JsonValueKind.String)
        {
            return new AgentRequestException(
                "agent_error",
                error.GetString() ?? "The capture agent rejected the request.",
                null,
                null);
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
            JsonElement? details = error.TryGetProperty("details", out JsonElement detailsValue) &&
                                   detailsValue.ValueKind is not JsonValueKind.Null
                ? detailsValue.Clone()
                : null;
            ulong? currentRevision = TryReadCurrentRevision(details) ??
                                     TryReadCurrentRevision(error);

            return new AgentRequestException(
                string.IsNullOrWhiteSpace(code) ? "agent_error" : code,
                string.IsNullOrWhiteSpace(message)
                    ? "The capture agent rejected the request."
                    : message,
                details,
                currentRevision);
        }

        string rawError = error.GetRawText();
        if (rawError.Length > 500)
        {
            rawError = rawError[..500] + "…";
        }

        return new AgentRequestException(
            "agent_error",
            $"The capture agent rejected the request: {rawError}",
            null,
            null);
    }

    private static ulong? TryReadCurrentRevision(JsonElement? value)
    {
        if (value is not { ValueKind: JsonValueKind.Object } objectValue ||
            !objectValue.TryGetProperty("current_revision", out JsonElement revision) ||
            revision.ValueKind is not JsonValueKind.Number ||
            !revision.TryGetUInt64(out ulong currentRevision))
        {
            return null;
        }

        return currentRevision;
    }
}
