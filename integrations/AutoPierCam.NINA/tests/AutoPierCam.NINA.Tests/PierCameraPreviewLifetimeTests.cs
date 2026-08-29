using System.ComponentModel;
using System.Windows.Media;
using AutoPierCam.NINA.Preview;

namespace AutoPierCam.NINA.Tests;

public sealed class PierCameraPreviewLifetimeTests
{
    [Fact]
    public async Task TwoLeasesStartOnceAndStopAfterLastRelease()
    {
        var runtime = new FakePreviewRuntime();
        var lifetime = new PierCameraPreviewLifetime(runtime);

        IAsyncDisposable first = await lifetime.AcquireAsync();
        IAsyncDisposable second = await lifetime.AcquireAsync();

        Assert.Equal(1, runtime.StartCalls);
        await first.DisposeAsync();
        await first.DisposeAsync();
        Assert.Equal(0, runtime.StopCalls);

        await second.DisposeAsync();
        Assert.Equal(1, runtime.StopCalls);
    }

    [Fact]
    public async Task AcquireWaitsForStopThenRestartsRuntime()
    {
        var runtime = new FakePreviewRuntime();
        var lifetime = new PierCameraPreviewLifetime(runtime);
        TaskCompletionSource<object?> stopEntered = NewSignal();
        TaskCompletionSource<object?> allowStop = NewSignal();
        runtime.StopBehavior = async () =>
        {
            stopEntered.TrySetResult(null);
            await allowStop.Task;
        };

        IAsyncDisposable first = await lifetime.AcquireAsync();
        Task release = first.DisposeAsync().AsTask();
        await stopEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));

        Task<IAsyncDisposable> acquire = lifetime.AcquireAsync().AsTask();
        Assert.False(acquire.IsCompleted);

        allowStop.SetResult(null);
        await release;
        IAsyncDisposable second = await acquire;
        Assert.Equal(2, runtime.StartCalls);
        Assert.Equal(1, runtime.StopCalls);

        runtime.StopBehavior = () => Task.CompletedTask;
        await second.DisposeAsync();
        Assert.Equal(2, runtime.StopCalls);
    }

    [Fact]
    public async Task StartFailureLeavesLifetimeRecoverable()
    {
        var runtime = new FakePreviewRuntime();
        var lifetime = new PierCameraPreviewLifetime(runtime);
        int failuresRemaining = 1;
        runtime.StartBehavior = () =>
        {
            if (Interlocked.Exchange(ref failuresRemaining, 0) == 1)
            {
                throw new InvalidOperationException("Expected start failure.");
            }
        };

        await Assert.ThrowsAsync<InvalidOperationException>(
            () => lifetime.AcquireAsync().AsTask());
        Assert.Equal(1, runtime.StartCalls);
        Assert.Equal(0, runtime.StopCalls);

        IAsyncDisposable recovered = await lifetime.AcquireAsync();
        Assert.Equal(2, runtime.StartCalls);
        await recovered.DisposeAsync();
        Assert.Equal(1, runtime.StopCalls);
    }

    [Fact]
    public async Task StopFailureDoesNotPoisonFutureAcquire()
    {
        var runtime = new FakePreviewRuntime
        {
            StopBehavior = () => Task.FromException(
                new InvalidOperationException("Expected stop failure.")),
        };
        var lifetime = new PierCameraPreviewLifetime(runtime);

        IAsyncDisposable first = await lifetime.AcquireAsync();
        await Assert.ThrowsAsync<InvalidOperationException>(
            () => first.DisposeAsync().AsTask());
        Assert.Equal(1, runtime.StopCalls);

        runtime.StopBehavior = () => Task.CompletedTask;
        IAsyncDisposable recovered = await lifetime.AcquireAsync();
        Assert.Equal(2, runtime.StartCalls);
        await recovered.DisposeAsync();
        Assert.Equal(2, runtime.StopCalls);
    }

    [Fact]
    public async Task ConcurrentDuplicateDisposeReleasesOnlyOnce()
    {
        var runtime = new FakePreviewRuntime();
        var lifetime = new PierCameraPreviewLifetime(runtime);
        TaskCompletionSource<object?> beginDisposals = NewSignal();
        TaskCompletionSource<object?> stopEntered = NewSignal();
        TaskCompletionSource<object?> allowStop = NewSignal();
        runtime.StopBehavior = async () =>
        {
            stopEntered.TrySetResult(null);
            await allowStop.Task;
        };
        IAsyncDisposable lease = await lifetime.AcquireAsync();

        Task[] disposals = Enumerable.Range(0, 2)
            .Select(_ => Task.Run(async () =>
            {
                await beginDisposals.Task;
                await lease.DisposeAsync();
            }))
            .ToArray();
        beginDisposals.SetResult(null);
        await stopEntered.Task.WaitAsync(TimeSpan.FromSeconds(5));

        Assert.Equal(1, runtime.StopCalls);
        allowStop.SetResult(null);
        await Task.WhenAll(disposals);
        Assert.Equal(1, runtime.StopCalls);
    }

    [Fact]
    public async Task OverlappingManifestInstancesKeepRuntimeUntilLastTeardown()
    {
        var runtime = new FakePreviewRuntime();
        var lifetime = new PierCameraPreviewLifetime(runtime);
        var first = new AutoPierCamPlugin(lifetime);
        var second = new AutoPierCamPlugin(lifetime);

        await Task.WhenAll(
            first.Initialize(),
            first.Initialize(),
            second.Initialize(),
            second.Initialize());
        Assert.Same(runtime, first.PreviewRuntime);
        Assert.Same(first.PreviewRuntime, second.PreviewRuntime);
        Assert.Equal(1, runtime.StartCalls);

        await Task.WhenAll(first.Teardown(), first.Teardown());
        Assert.Equal(0, runtime.StopCalls);

        await second.Teardown();
        await second.Teardown();
        Assert.Equal(1, runtime.StopCalls);
    }

    [Fact]
    public async Task ManifestCanRetryAcquireAfterStartFailureAndTeardownIsIdempotent()
    {
        var runtime = new FakePreviewRuntime();
        var lifetime = new PierCameraPreviewLifetime(runtime);
        int failuresRemaining = 1;
        runtime.StartBehavior = () =>
        {
            if (Interlocked.Exchange(ref failuresRemaining, 0) == 1)
            {
                throw new InvalidOperationException("Expected start failure.");
            }
        };
        var manifest = new AutoPierCamPlugin(lifetime);

        await Assert.ThrowsAsync<InvalidOperationException>(manifest.Initialize);
        await manifest.Initialize();
        Assert.Equal(2, runtime.StartCalls);

        await manifest.Teardown();
        await manifest.Teardown();
        Assert.Equal(1, runtime.StopCalls);
    }

    private static TaskCompletionSource<object?> NewSignal() =>
        new(TaskCreationOptions.RunContinuationsAsynchronously);

    private sealed class FakePreviewRuntime : IPierCameraPreviewRuntime
    {
        private int startCalls;
        private int stopCalls;

        internal Action StartBehavior { get; set; } = () => { };

        internal Func<Task> StopBehavior { get; set; } = () => Task.CompletedTask;

        internal int StartCalls => Volatile.Read(ref startCalls);

        internal int StopCalls => Volatile.Read(ref stopCalls);

        public event PropertyChangedEventHandler? PropertyChanged
        {
            add { }
            remove { }
        }

        public ImageSource? Image => null;

        public bool HasImage => false;

        public bool IsLive => false;

        public bool IsStale => false;

        public double ImageOpacity => 1;

        public string StatusText => string.Empty;

        public string ConnectionText => string.Empty;

        public string CapturedAtText => string.Empty;

        public string FrameAgeText => string.Empty;

        public string DimensionsText => string.Empty;

        public string ExposureText => string.Empty;

        public string GainText => string.Empty;

        public string ModeText => string.Empty;

        public string DroppedFramesText => string.Empty;

        public void Start()
        {
            Interlocked.Increment(ref startCalls);
            StartBehavior();
        }

        public Task StopAsync()
        {
            Interlocked.Increment(ref stopCalls);
            return StopBehavior();
        }
    }
}
