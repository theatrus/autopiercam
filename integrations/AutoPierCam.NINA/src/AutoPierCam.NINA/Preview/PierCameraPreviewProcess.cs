namespace AutoPierCam.NINA.Preview;

/// <summary>
/// Holds the process-wide preview runtime explicitly. N.I.N.A. composes plugin
/// manifests and dockables in separate MEF containers, so MEF shared creation
/// policy cannot provide a singleton across those scopes.
/// </summary>
internal static class PierCameraPreviewProcess
{
    internal static IPierCameraPreviewRuntime Runtime { get; } =
        new PierCameraPreviewRuntime();
}
