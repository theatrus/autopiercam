using System.Buffers.Binary;
using System.Diagnostics;
using System.IO.Pipes;
using System.Text;
using System.Text.Json;
using AutoPierCam.NINA.Preview;
using AutoPierCam.Preview;

namespace AutoPierCam.NINA.Tests;

public sealed class ExposureProgressTests
{
    private const string RequestId = "exposure-test";
    private const string ValidExposure =
        """
        {"session_generation":4,"settling":true,"exposure_us":60000000,"gain":150,"max_exposure_us":60000000,"settling_frames":2,"settling_min_frames":6,"wait_elapsed_ms":20000,"frame_timeout_ms":125000}
        """;

    [Theory]
    [InlineData(30_000_000, 30, false)]
    [InlineData(30_000_000, 64, false)]
    [InlineData(30_000_000, 65, true)]
    [InlineData(60_000_000, 60, false)]
    [InlineData(60_000_000, 124, false)]
    [InlineData(60_000_000, 125, true)]
    [InlineData(1_000, 6, true)]
    public void OldAgentsUseExposureAwareButFiniteFrameDeadline(long exposure, int age, bool stale)
    {
        Assert.Equal(stale, ExposurePresentation.IsStale(
            TimeSpan.FromSeconds(age), exposure, 4, null, TimeSpan.MaxValue));
    }

    [Fact]
    public void FreshProgressAccommodatesDarkRampFromShortPreviewToLongExposure()
    {
        var observation = Observation();
        Assert.False(ExposurePresentation.IsStale(TimeSpan.FromSeconds(60), 1_000, 4, observation, TimeSpan.FromSeconds(1)));
        Assert.True(ExposurePresentation.IsStale(TimeSpan.FromSeconds(125), 1_000, 4, observation, TimeSpan.FromSeconds(1)));
    }

    [Fact]
    public void BrightRampDoesNotInvalidateLastLongExposurePrematurely()
    {
        ExposureProgress progress = Progress() with { ExposureUs = 1_000, MaxExposureUs = 60_000_000, WaitElapsedMs = 100 };
        var observation = Observation(progress);
        Assert.False(ExposurePresentation.IsStale(TimeSpan.FromSeconds(60), 60_000_000, 4, observation, TimeSpan.Zero));
        Assert.True(ExposurePresentation.IsStale(TimeSpan.FromSeconds(126), 60_000_000, 4, observation, TimeSpan.Zero));
    }

    [Fact]
    public void ProgressCannotBlessADifferentCameraSession()
    {
        var observation = Observation();
        Assert.True(ExposurePresentation.IsStale(TimeSpan.FromSeconds(1), 60_000_000, 3, observation, TimeSpan.Zero));
        Assert.Null(ExposurePresentation.Describe(observation, TimeSpan.Zero, 3));
    }

    [Fact]
    public void ExpiredStatusFallsBackAndStopsAdvertisingExposure()
    {
        var observation = Observation();
        Assert.True(ExposurePresentation.IsStale(TimeSpan.FromSeconds(60), 1_000, 4, observation, ExposurePresentation.FreshFor));
        Assert.Null(ExposurePresentation.Describe(observation, ExposurePresentation.FreshFor));
        Assert.Null(ExposurePresentation.Describe(null, TimeSpan.Zero));
    }

    [Fact]
    public void RepeatedFreshStatusCannotKeepAFrozenPreviewLive()
    {
        var observation = Observation(Progress() with { WaitElapsedMs = 0 });
        Assert.True(ExposurePresentation.IsStale(TimeSpan.FromSeconds(126), 1_000, 4, observation, TimeSpan.Zero));
    }

    [Fact]
    public void CachedFrameStartsAtItsCaptureAgeThenUsesMonotonicTime()
    {
        var clock = new PreviewFrameClock();
        DateTimeOffset now = DateTimeOffset.FromUnixTimeSeconds(1_800_000_000);
        ulong captured = (ulong)now.AddMinutes(-3).ToUnixTimeMilliseconds();
        clock.RecordFrame(captured, 4, 20, now, 0);
        Assert.Equal(TimeSpan.FromSeconds(181), clock.GetAge(Stopwatch.Frequency));
        Assert.True(ExposurePresentation.IsStale(clock.GetAge(0), 60_000_000, 4, Observation(), TimeSpan.Zero));
    }

    [Fact]
    public void ReconnectingWithTheSameCachedFrameCannotResetItsAge()
    {
        var clock = new PreviewFrameClock();
        DateTimeOffset now = DateTimeOffset.FromUnixTimeSeconds(1_800_000_000);
        ulong captured = (ulong)now.ToUnixTimeMilliseconds();
        clock.RecordFrame(captured, 4, 20, now, 0);
        // Even a simultaneous backward wall-clock correction cannot rejuvenate it.
        clock.RecordFrame(captured, 4, 20, now.AddHours(-1), Stopwatch.Frequency * 130);
        Assert.Equal(TimeSpan.FromSeconds(130), clock.GetAge(Stopwatch.Frequency * 130));
        Assert.True(ExposurePresentation.IsStale(clock.GetAge(Stopwatch.Frequency * 130), 60_000_000, 4, Observation(), TimeSpan.Zero));
    }

    [Fact]
    public void FutureCaptureTimestampNeverCreatesNegativeFrameAge()
    {
        var clock = new PreviewFrameClock();
        DateTimeOffset now = DateTimeOffset.FromUnixTimeSeconds(1_800_000_000);
        ulong captured = (ulong)now.AddHours(1).ToUnixTimeMilliseconds();
        clock.RecordFrame(captured, 4, 20, now, 0);
        Assert.Equal(TimeSpan.Zero, clock.GetAge(0));
        Assert.Equal(TimeSpan.FromSeconds(30), clock.GetAge(Stopwatch.Frequency * 30));
    }

    [Fact]
    public void RestartedAgentReusingCountersWithNewCaptureGetsNewFrameAge()
    {
        var clock = new PreviewFrameClock();
        DateTimeOffset now = DateTimeOffset.FromUnixTimeSeconds(1_800_000_000);
        clock.RecordFrame((ulong)now.ToUnixTimeMilliseconds(), 1, 1, now, 0);
        DateTimeOffset later = now.AddMinutes(10);
        clock.RecordFrame((ulong)later.ToUnixTimeMilliseconds(), 1, 1, later, Stopwatch.Frequency * 600);
        Assert.Equal(TimeSpan.FromSeconds(1), clock.GetAge(Stopwatch.Frequency * 601));
    }

    [Fact]
    public void MissedCameraDeadlineOverridesARecentlyReceivedCachedPreview()
    {
        var observation = Observation(Progress() with { WaitElapsedMs = 125_000 });
        Assert.True(ExposurePresentation.IsStale(TimeSpan.FromSeconds(1), 60_000_000, 4, observation, TimeSpan.Zero));
        Assert.Contains("past its expected deadline", ExposurePresentation.Describe(observation, TimeSpan.Zero));
    }

    [Theory]
    [InlineData("starting", false)]
    [InlineData("capturing", false)]
    [InlineData("paused", false)]
    [InlineData("faulted", true)]
    [InlineData("stopping", true)]
    [InlineData("idle", true)]
    public void NonCapturingStatusCannotMakeOldFramesLookLive(string state, bool stale)
    {
        var observation = Observation(state: state);
        Assert.Equal(stale, ExposurePresentation.IsStale(TimeSpan.FromSeconds(1), 1_000, 4, observation, TimeSpan.Zero));
    }

    [Fact]
    public void StartupDescriptionReportsSettlingWithoutInventingCountdown()
    {
        string? text = ExposurePresentation.Describe(Observation(), TimeSpan.FromSeconds(1));
        Assert.Contains("Settling: 2/6 minimum frames", text);
        Assert.Contains("21 s", text);
        Assert.Contains("about 60 s", text);
        Assert.Contains("estimated", text);
        Assert.DoesNotContain("remaining", text);
    }

    [Fact]
    public async Task NinaShowsSettlingBeforeFirstPreviewAndPreservesReconnectState()
    {
        var runtime = new PierCameraPreviewRuntime();
        await runtime.HandleStateAsync(new PreviewStreamState(PreviewStreamPhase.WaitingForFrame, 1), default);
        await runtime.HandleProgressAsync(Observation(), default);
        Assert.False(runtime.HasImage);
        Assert.Equal("Settling", runtime.ConnectionText);
        Assert.Contains("Settling: 2/6", runtime.StatusText);

        await runtime.HandleStateAsync(new PreviewStreamState(PreviewStreamPhase.Reconnecting, 1, "disconnected"), default);
        Assert.Equal("Reconnecting", runtime.ConnectionText);
        Assert.DoesNotContain("Exposing", runtime.StatusText);
        Assert.Contains("disconnected", runtime.StatusText);
    }

    [Fact]
    public async Task NinaShowsCaptureFaultEvenWithAnOpenPreviewConnection()
    {
        var runtime = new PierCameraPreviewRuntime();
        await runtime.HandleStateAsync(new PreviewStreamState(PreviewStreamPhase.WaitingForFrame, 1), default);
        await runtime.HandleProgressAsync(new ExposureProgressObservation(
            new ExposureProgressStatus("faulted", "Camera disconnected.", null), Stopwatch.GetTimestamp()), default);
        Assert.Equal("Capture failed", runtime.ConnectionText);
        Assert.Contains("Camera disconnected", runtime.StatusText);
    }

    [Fact]
    public void ReadsProgressAndIgnoresUnrelatedAdditiveStatusFields()
    {
        ExposureProgressStatus status = ParseStatus(StatusJson(ValidExposure));
        Assert.Equal("capturing", status.State);
        Assert.Equal(60_000_000, status.Exposure!.ExposureUs);
        Assert.Equal((ulong)4, status.Exposure.SessionGeneration);
    }

    [Theory]
    [InlineData("{\"state\":\"capturing\"}")]
    [InlineData("{\"state\":\"capturing\",\"capabilities\":[\"exposure.progress\"]}")]
    [InlineData("{\"state\":\"capturing\",\"capabilities\":[\"exposure.progress\"],\"exposure\":null}")]
    public void OldOrNotYetCapturingAgentsHaveNoProgress(string json)
    {
        Assert.Null(ParseStatus(json).Exposure);
    }

    [Theory]
    [InlineData("\"exposure_us\":60000000", "\"exposure_us\":0")]
    [InlineData("\"gain\":150", "\"gain\":-1")]
    [InlineData("\"max_exposure_us\":60000000", "\"max_exposure_us\":0")]
    [InlineData("\"settling_min_frames\":6", "\"settling_min_frames\":0")]
    [InlineData("\"frame_timeout_ms\":125000", "\"frame_timeout_ms\":0")]
    [InlineData("\"session_generation\":4", "\"session_generation\":-1")]
    [InlineData("\"wait_elapsed_ms\":20000", "\"wait_elapsed_ms\":\"broken\"")]
    [InlineData("\"settling_frames\":2,", "")]
    public void InvalidOptionalProgressFallsBackWithoutBreakingPreview(string original, string replacement)
    {
        Assert.Null(ParseStatus(StatusJson(ValidExposure.Replace(original, replacement, StringComparison.Ordinal))).Exposure);
    }

    [Theory]
    [InlineData("\"version\":1", "\"version\":2")]
    [InlineData("\"request_id\":\"exposure-test\"", "\"request_id\":\"other\"")]
    [InlineData("\"version\":1", "\"version\":1,\"version\":1")]
    [InlineData("\"result\":", "\"error\":{},\"result\":")]
    public void RejectsInvalidStatusEnvelope(string original, string replacement)
    {
        byte[] body = Encoding.UTF8.GetBytes(ResponseJson(StatusJson(ValidExposure)).Replace(original, replacement, StringComparison.Ordinal));
        Assert.Throws<ExposureProgressProtocolException>(() => ExposureProgressClient.ParseResponse(body, RequestId));
    }

    [Theory]
    [InlineData(0)]
    [InlineData(ExposureProgressClient.MaxMessageBytes + 1)]
    public async Task RejectsInvalidLengthBeforeReadingPayload(int length)
    {
        byte[] prefix = new byte[sizeof(uint)];
        BinaryPrimitives.WriteUInt32LittleEndian(prefix, (uint)length);
        await using var stream = new MemoryStream(prefix);
        await Assert.ThrowsAsync<ExposureProgressProtocolException>(() =>
            ExposureProgressClient.ReadResponseAsync(stream, RequestId));
    }

    [Fact]
    public async Task ConnectTimeoutIsBounded()
    {
        var client = new ExposureProgressClient(NewPipeName(), connectTimeout: TimeSpan.FromMilliseconds(100));
        await Assert.ThrowsAsync<TimeoutException>(() => client.GetAsync().WaitAsync(TimeSpan.FromSeconds(5)));
    }

    [Fact]
    public async Task PartialStatusResponseTimesOutAndOnlyReadOnlyStatusWasRequested()
    {
        string pipeName = NewPipeName();
        await using var server = Server(pipeName);
        Task connected = server.WaitForConnectionAsync();
        var client = new ExposureProgressClient(pipeName, responseTimeout: TimeSpan.FromMilliseconds(300));
        Task<ExposureProgressStatus> get = client.GetAsync();
        await connected.WaitAsync(TimeSpan.FromSeconds(5));
        using JsonDocument request = await ReadRequest(server);
        Assert.Equal("status.get", request.RootElement.GetProperty("method").GetString());
        Assert.Empty(request.RootElement.GetProperty("payload").EnumerateObject());
        await server.WriteAsync(new byte[] { 1 });
        await Assert.ThrowsAsync<TimeoutException>(() => get.WaitAsync(TimeSpan.FromSeconds(5)));
    }

    [Fact]
    public async Task CancellationJoinsPollingWhileWaitingForResponse()
    {
        string pipeName = NewPipeName();
        await using var server = Server(pipeName);
        Task connected = server.WaitForConnectionAsync();
        using var cancellation = new CancellationTokenSource();
        var client = new ExposureProgressClient(pipeName);
        int callbacks = 0;
        Task poll = client.RunAsync((_, _) =>
        {
            Interlocked.Increment(ref callbacks);
            return Task.CompletedTask;
        }, cancellation.Token);
        await connected.WaitAsync(TimeSpan.FromSeconds(5));
        using JsonDocument request = await ReadRequest(server);
        cancellation.Cancel();
        await poll.WaitAsync(TimeSpan.FromSeconds(5));
        Assert.Equal(0, callbacks);
    }

    [Fact]
    public async Task PollerSerializesCallbacksAndRejectsASecondRun()
    {
        string pipeName = NewPipeName();
        await using var server = Server(pipeName);
        Task connected = server.WaitForConnectionAsync();
        using var cancellation = new CancellationTokenSource();
        var callbackEntered = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var client = new ExposureProgressClient(pipeName, pollInterval: TimeSpan.FromMilliseconds(10));
        int callbacks = 0;
        Task poll = client.RunAsync(async (observation, token) =>
        {
            Assert.NotNull(observation);
            Interlocked.Increment(ref callbacks);
            callbackEntered.SetResult();
            await Task.Delay(Timeout.Infinite, token);
        }, cancellation.Token);
        await connected.WaitAsync(TimeSpan.FromSeconds(5));
        using JsonDocument request = await ReadRequest(server);
        string requestId = request.RootElement.GetProperty("request_id").GetString()!;
        await WriteResponse(server, ResponseJson(StatusJson(ValidExposure), requestId));
        await callbackEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));
        await Assert.ThrowsAsync<InvalidOperationException>(() => client.RunAsync((_, _) => Task.CompletedTask, cancellation.Token));
        cancellation.Cancel();
        await poll.WaitAsync(TimeSpan.FromSeconds(5));
        Assert.Equal(1, callbacks);
    }

    private static ExposureProgress Progress() => ParseStatus(StatusJson(ValidExposure)).Exposure!;

    private static ExposureProgressObservation Observation(ExposureProgress? progress = null, string state = "capturing") =>
        new(new ExposureProgressStatus(state, null, progress ?? Progress()), Stopwatch.GetTimestamp());

    private static ExposureProgressStatus ParseStatus(string json) =>
        ExposureProgressClient.ParseResponse(Encoding.UTF8.GetBytes(ResponseJson(json)), RequestId);

    private static string StatusJson(string exposure) =>
        "{\"state\":\"capturing\",\"capabilities\":[\"exposure.progress\"],\"exposure\":" + exposure + ",\"unrelated\":{\"future\":true}}";

    private static string ResponseJson(string status, string requestId = RequestId) =>
        "{\"version\":1,\"request_id\":\"" + requestId + "\",\"result\":" + status + "}";

    private static string NewPipeName() => "autopiercam-exposure-test-" + Guid.NewGuid().ToString("N");

    private static NamedPipeServerStream Server(string name) =>
        new(name, PipeDirection.InOut, 1, PipeTransmissionMode.Byte, PipeOptions.Asynchronous);

    private static async Task<JsonDocument> ReadRequest(Stream stream)
    {
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(5));
        byte[] prefix = new byte[sizeof(uint)];
        await stream.ReadExactlyAsync(prefix, timeout.Token);
        byte[] body = new byte[BinaryPrimitives.ReadUInt32LittleEndian(prefix)];
        await stream.ReadExactlyAsync(body, timeout.Token);
        return JsonDocument.Parse(body);
    }

    private static async Task WriteResponse(Stream stream, string json)
    {
        byte[] body = Encoding.UTF8.GetBytes(json);
        byte[] prefix = new byte[sizeof(uint)];
        BinaryPrimitives.WriteUInt32LittleEndian(prefix, checked((uint)body.Length));
        await stream.WriteAsync(prefix);
        await stream.WriteAsync(body);
        await stream.FlushAsync();
    }
}
