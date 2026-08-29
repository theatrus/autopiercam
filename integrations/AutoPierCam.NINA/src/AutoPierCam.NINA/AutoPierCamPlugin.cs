using System.ComponentModel.Composition;
using AutoPierCam.NINA.Preview;
using NINA.Plugin;
using NINA.Plugin.Interfaces;

namespace AutoPierCam.NINA;

/// <summary>
/// Owns the single preview runtime for the lifetime of the N.I.N.A. process.
/// The dockable pane observes that runtime and never takes ownership of a camera.
/// </summary>
[Export(typeof(IPluginManifest))]
public sealed class AutoPierCamPlugin : PluginBase
{
    private readonly IPierCameraPreviewRuntime previewRuntime;

    [ImportingConstructor]
    public AutoPierCamPlugin()
    {
        previewRuntime = PierCameraPreviewProcess.Runtime;
    }

    internal IPierCameraPreviewRuntime PreviewRuntime => previewRuntime;

    public override async Task Initialize()
    {
        await base.Initialize().ConfigureAwait(true);
        previewRuntime.Start();
    }

    public override async Task Teardown()
    {
        await previewRuntime.StopAsync().ConfigureAwait(true);
        await base.Teardown().ConfigureAwait(true);
    }
}
