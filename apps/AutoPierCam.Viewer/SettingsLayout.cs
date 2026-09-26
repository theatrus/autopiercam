namespace AutoPierCam.Viewer;

internal readonly record struct SettingsLayout(bool PreviewVisible, double PaneWidth, double Gap)
{
    internal static (int Width, int Height) InitialWindowSize(double scale, int workWidth, int workHeight) =>
        ((int)Math.Min(1180 * scale, workWidth * 0.9), (int)Math.Min(760 * scale, workHeight * 0.9));

    internal static SettingsLayout ForWidth(double workspaceWidth, bool open)
    {
        if (!open) return new(true, 0, 0);
        double usable = Math.Max(0, workspaceWidth - 32); // workspace padding
        // Keep at least 480 DIPs for the preview. At narrow widths/high DPI,
        // settings get the workspace instead of overflowing its right edge.
        return usable >= 980 ? new(true, 480, 20) : new(false, usable, 0);
    }
}
