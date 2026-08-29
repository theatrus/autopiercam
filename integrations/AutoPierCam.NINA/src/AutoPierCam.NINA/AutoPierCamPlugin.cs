using System.ComponentModel.Composition;
using System.Runtime.ExceptionServices;
using AutoPierCam.NINA.Preview;
using NINA.Plugin;
using NINA.Plugin.Interfaces;

namespace AutoPierCam.NINA;

/// <summary>
/// Leases the process-wide preview runtime for this manifest's lifetime. The
/// dockable pane observes that runtime and never takes ownership of a camera.
/// </summary>
[Export(typeof(IPluginManifest))]
public sealed class AutoPierCamPlugin : PluginBase
{
    private readonly SemaphoreSlim lifecycleGate = new(1, 1);
    private readonly PierCameraPreviewLifetime previewLifetime;
    private bool baseInitialized;
    private IAsyncDisposable? previewLease;

    [ImportingConstructor]
    public AutoPierCamPlugin()
        : this(PierCameraPreviewProcess.Lifetime)
    {
    }

    internal AutoPierCamPlugin(PierCameraPreviewLifetime previewLifetime)
    {
        ArgumentNullException.ThrowIfNull(previewLifetime);
        this.previewLifetime = previewLifetime;
    }

    internal IPierCameraPreviewRuntime PreviewRuntime => previewLifetime.Runtime;

    public override async Task Initialize()
    {
        await lifecycleGate.WaitAsync().ConfigureAwait(true);
        try
        {
            if (previewLease is not null)
            {
                return;
            }

            if (!baseInitialized)
            {
                await base.Initialize().ConfigureAwait(true);
                baseInitialized = true;
            }

            previewLease = await previewLifetime.AcquireAsync().ConfigureAwait(true);
        }
        finally
        {
            lifecycleGate.Release();
        }
    }

    public override async Task Teardown()
    {
        await lifecycleGate.WaitAsync().ConfigureAwait(true);
        try
        {
            if (previewLease is null && !baseInitialized)
            {
                return;
            }

            IAsyncDisposable? lease = previewLease;
            previewLease = null;
            bool teardownBase = baseInitialized;
            baseInitialized = false;

            Exception? releaseFailure = null;
            try
            {
                if (lease is not null)
                {
                    await lease.DisposeAsync().ConfigureAwait(true);
                }
            }
            catch (Exception exception)
            {
                releaseFailure = exception;
            }

            Exception? baseFailure = null;
            try
            {
                if (teardownBase)
                {
                    await base.Teardown().ConfigureAwait(true);
                }
            }
            catch (Exception exception)
            {
                baseFailure = exception;
            }

            if (releaseFailure is not null && baseFailure is not null)
            {
                throw new AggregateException(
                    "AutoPierCam preview release and plugin teardown both failed.",
                    releaseFailure,
                    baseFailure);
            }

            if (releaseFailure is not null)
            {
                ExceptionDispatchInfo.Capture(releaseFailure).Throw();
            }

            if (baseFailure is not null)
            {
                ExceptionDispatchInfo.Capture(baseFailure).Throw();
            }
        }
        finally
        {
            lifecycleGate.Release();
        }
    }
}
