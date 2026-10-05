namespace AutoPierCam.Viewer;

internal static class SettingsLayout
{
    internal static (int Width, int Height) InitialWindowSize(double scale, int workWidth, int workHeight) =>
        ((int)Math.Min(1180 * scale, workWidth * 0.9), (int)Math.Min(760 * scale, workHeight * 0.9));

    internal static (int Width, int Height) SettingsWindowSize(double scale, int workWidth, int workHeight) =>
        ((int)Math.Min(640 * scale, workWidth * 0.9), (int)Math.Min(820 * scale, workHeight * 0.9));
}
