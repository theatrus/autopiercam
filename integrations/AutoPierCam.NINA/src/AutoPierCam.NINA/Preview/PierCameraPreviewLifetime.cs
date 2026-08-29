namespace AutoPierCam.NINA.Preview;

/// <summary>
/// Reference-counts manifest ownership of the process-wide preview runtime.
/// N.I.N.A. can overlap plugin manifest lifetimes while composing or reloading
/// plugins, so the runtime remains active until the final owner releases it.
/// </summary>
internal sealed class PierCameraPreviewLifetime
{
    private readonly SemaphoreSlim transitionGate = new(1, 1);
    private readonly IPierCameraPreviewRuntime runtime;
    private int leaseCount;

    internal PierCameraPreviewLifetime(IPierCameraPreviewRuntime runtime)
    {
        ArgumentNullException.ThrowIfNull(runtime);
        this.runtime = runtime;
    }

    internal IPierCameraPreviewRuntime Runtime => runtime;

    internal async ValueTask<IAsyncDisposable> AcquireAsync()
    {
        await transitionGate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (leaseCount == 0)
            {
                runtime.Start();
            }

            leaseCount = checked(leaseCount + 1);
            return new Lease(this);
        }
        finally
        {
            transitionGate.Release();
        }
    }

    private async ValueTask ReleaseAsync()
    {
        await transitionGate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (leaseCount <= 0)
            {
                throw new InvalidOperationException(
                    "The AutoPierCam preview lifetime lease count is invalid.");
            }

            leaseCount--;
            if (leaseCount == 0)
            {
                await runtime.StopAsync().ConfigureAwait(false);
            }
        }
        finally
        {
            transitionGate.Release();
        }
    }

    private sealed class Lease : IAsyncDisposable
    {
        private PierCameraPreviewLifetime? owner;

        internal Lease(PierCameraPreviewLifetime owner)
        {
            this.owner = owner;
        }

        public ValueTask DisposeAsync()
        {
            PierCameraPreviewLifetime? currentOwner = Interlocked.Exchange(ref owner, null);
            return currentOwner is null
                ? ValueTask.CompletedTask
                : currentOwner.ReleaseAsync();
        }
    }
}
