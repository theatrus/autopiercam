using System;
using System.Diagnostics;
using System.Linq;
using System.Text.Json;
using System.Text.Json.Serialization;

namespace AutoPierCam.Preview;

// Kept outside preview-v1 metadata so older strict preview readers remain compatible.
internal sealed record ExposureProgress
{
    [JsonPropertyName("session_generation"), JsonRequired]
    public ulong SessionGeneration { get; init; }

    [JsonPropertyName("settling"), JsonRequired]
    public bool Settling { get; init; }

    [JsonPropertyName("exposure_us"), JsonRequired]
    public long ExposureUs { get; init; }

    [JsonPropertyName("gain"), JsonRequired]
    public long Gain { get; init; }

    [JsonPropertyName("max_exposure_us"), JsonRequired]
    public long MaxExposureUs { get; init; }

    [JsonPropertyName("settling_frames"), JsonRequired]
    public uint SettlingFrames { get; init; }

    [JsonPropertyName("settling_min_frames"), JsonRequired]
    public uint SettlingMinFrames { get; init; }

    [JsonPropertyName("wait_elapsed_ms"), JsonRequired]
    public ulong WaitElapsedMs { get; init; }

    [JsonPropertyName("frame_timeout_ms"), JsonRequired]
    public ulong FrameTimeoutMs { get; init; }

    internal bool IsValid => ExposureUs > 0 && Gain >= 0 && MaxExposureUs > 0 &&
        FrameTimeoutMs > 0 && (!Settling || SettlingMinFrames > 0);
}

internal sealed record ExposureProgressStatus(string State, string? LastError, ExposureProgress? Exposure)
{
    internal bool IsActive => State is "starting" or "capturing" or "paused";

    internal static ExposureProgressStatus ParseStatus(JsonElement status)
    {
        if (status.ValueKind != JsonValueKind.Object ||
            !status.TryGetProperty("state", out JsonElement stateValue) ||
            stateValue.ValueKind != JsonValueKind.String ||
            string.IsNullOrWhiteSpace(stateValue.GetString()))
        {
            throw new ExposureProgressProtocolException("The agent status has no state.");
        }

        string state = stateValue.GetString()!;
        string? lastError = status.TryGetProperty("last_error", out JsonElement error) &&
            error.ValueKind == JsonValueKind.String ? Compact(error.GetString()!) : null;
        ExposureProgress? exposure = null;
        bool supported = status.TryGetProperty("capabilities", out JsonElement capabilities) &&
            capabilities.ValueKind == JsonValueKind.Array &&
            capabilities.EnumerateArray().Any(capability =>
                capability.ValueKind == JsonValueKind.String &&
                capability.GetString() == "exposure.progress");
        if (supported && status.TryGetProperty("exposure", out JsonElement value) &&
            value.ValueKind == JsonValueKind.Object)
        {
            try
            {
                exposure = value.Deserialize<ExposureProgress>();
                if (exposure?.IsValid != true)
                {
                    exposure = null;
                }
            }
            catch (JsonException)
            {
                // Progress is an optional enhancement; keep the old-agent fallback.
            }
        }

        return new ExposureProgressStatus(state, lastError, exposure);
    }

    private static string Compact(string value)
    {
        string compact = string.Join(" ", value.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return compact.Length <= 500 ? compact : compact[..500] + "…";
    }
}

internal sealed record ExposureProgressObservation(ExposureProgressStatus Status, long ReceivedTimestamp)
{
    internal TimeSpan Age => Stopwatch.GetElapsedTime(ReceivedTimestamp);
}

internal sealed class PreviewFrameClock
{
    private ulong capturedAtUnixMs;
    private ulong sessionGeneration;
    private ulong sequence;
    private TimeSpan initialAge;
    private long receivedTimestamp;

    internal bool HasFrame { get; private set; }

    internal TimeSpan Age => GetAge(Stopwatch.GetTimestamp());

    internal void RecordFrame(ulong capturedAtUnixMs, ulong sessionGeneration, ulong sequence) =>
        RecordFrame(capturedAtUnixMs, sessionGeneration, sequence, DateTimeOffset.UtcNow, Stopwatch.GetTimestamp());

    internal void RecordFrame(
        ulong capturedAtUnixMs,
        ulong sessionGeneration,
        ulong sequence,
        DateTimeOffset utcNow,
        long receivedTimestamp)
    {
        if (HasFrame && this.capturedAtUnixMs == capturedAtUnixMs &&
            this.sessionGeneration == sessionGeneration && this.sequence == sequence)
        {
            // The preview pipe sends its cached frame on reconnect. Receiving it
            // again cannot reset that snapshot's age or its freshness deadline.
            return;
        }

        if (capturedAtUnixMs > 253_402_300_799_999)
        {
            throw new ArgumentOutOfRangeException(nameof(capturedAtUnixMs));
        }

        DateTimeOffset capturedAt = DateTimeOffset.FromUnixTimeMilliseconds((long)capturedAtUnixMs);
        initialAge = utcNow > capturedAt ? utcNow - capturedAt : TimeSpan.Zero;
        this.capturedAtUnixMs = capturedAtUnixMs;
        this.sessionGeneration = sessionGeneration;
        this.sequence = sequence;
        this.receivedTimestamp = receivedTimestamp;
        HasFrame = true;
    }

    internal TimeSpan GetAge(long nowTimestamp)
    {
        if (!HasFrame)
        {
            return TimeSpan.MaxValue;
        }

        // Consult the wall clock only once per distinct frame. Later clock
        // corrections cannot make an already aging snapshot young again.
        TimeSpan elapsed = Stopwatch.GetElapsedTime(receivedTimestamp, nowTimestamp);
        elapsed = elapsed < TimeSpan.Zero ? TimeSpan.Zero : elapsed;
        return elapsed > TimeSpan.MaxValue - initialAge ? TimeSpan.MaxValue : initialAge + elapsed;
    }
}

internal static class ExposurePresentation
{
    internal static readonly TimeSpan FreshFor = TimeSpan.FromSeconds(8);

    internal static bool IsCurrent(ExposureProgressObservation? observation, TimeSpan observationAge) =>
        observation is not null && observationAge >= TimeSpan.Zero && observationAge < FreshFor;

    internal static bool IsStale(
        TimeSpan frameAge,
        long? frameExposureUs,
        ulong frameSession,
        ExposureProgressObservation? observation,
        TimeSpan observationAge)
    {
        double deadlineSeconds = FallbackSeconds(frameExposureUs);
        if (IsCurrent(observation, observationAge))
        {
            ExposureProgressStatus status = observation!.Status;
            if (!status.IsActive)
            {
                return true;
            }

            if (status.Exposure is { } progress)
            {
                if (progress.SessionGeneration != frameSession)
                {
                    return true;
                }

                deadlineSeconds = Math.Max(deadlineSeconds, progress.FrameTimeoutMs / 1_000d);
                if (progress.WaitElapsedMs / 1_000d + observationAge.TotalSeconds >=
                    progress.FrameTimeoutMs / 1_000d)
                {
                    return true;
                }
            }
        }

        // Check total preview age as well as camera wait time. A healthy status
        // connection must not keep an abandoned preview stream alive indefinitely.
        return frameAge.TotalSeconds >= deadlineSeconds;
    }

    internal static string? Describe(
        ExposureProgressObservation? observation,
        TimeSpan observationAge,
        ulong? expectedSession = null)
    {
        if (!IsCurrent(observation, observationAge))
        {
            return null;
        }

        ExposureProgressStatus status = observation!.Status;
        if (!status.IsActive)
        {
            return status.State switch
            {
                "faulted" => string.IsNullOrWhiteSpace(status.LastError)
                    ? "Capture failed. Check AutoPierCam for details."
                    : $"Capture failed: {status.LastError}",
                "stopping" => "AutoPierCam is stopping capture.",
                "idle" => "AutoPierCam is idle; start capture to see new frames.",
                _ => null,
            };
        }

        if (status.Exposure is not { } progress ||
            (expectedSession.HasValue && progress.SessionGeneration != expectedSession.Value))
        {
            return null;
        }

        double elapsedSeconds = progress.WaitElapsedMs / 1_000d + observationAge.TotalSeconds;
        string stage = progress.Settling
            ? $"Settling: {progress.SettlingFrames:N0}/{progress.SettlingMinFrames:N0} minimum frames"
            : "Exposing";
        string timing = $"waiting {Math.Floor(elapsedSeconds):N0} s for a frame; exposure about {FormatExposure(progress.ExposureUs)}";
        return elapsedSeconds >= progress.FrameTimeoutMs / 1_000d
            ? $"Waiting for a frame past its expected deadline ({timing})."
            : $"{stage} — {timing}. Timing is estimated.";
    }

    internal static string FormatExposure(long exposureUs) => exposureUs switch
    {
        >= 1_000_000 => $"{exposureUs / 1_000_000d:0.###} s",
        >= 1_000 => $"{exposureUs / 1_000d:0.###} ms",
        _ => $"{exposureUs:N0} µs",
    };

    private static double FallbackSeconds(long? exposureUs) =>
        Math.Max(5, (exposureUs is > 0 ? exposureUs.Value / 1_000_000d * 2 : 0) + 5);
}
