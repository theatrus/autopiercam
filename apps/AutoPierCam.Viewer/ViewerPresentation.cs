namespace AutoPierCam.Viewer;

internal static class ViewerPresentation
{
    internal static string PreviewCaption(string dimensions, TimeSpan age, bool stopped, bool stale)
    {
        double seconds = Math.Max(0, age.TotalSeconds);
        string elapsed = seconds < 1 ? "just now" : seconds < 60 ? $"{seconds:0}s ago" : $"{seconds / 60:0}m ago";
        string prefix = stopped ? "Capture stopped" : stale ? "Waiting for new frame" : "Preview";
        return $"{prefix} · {dimensions} · {elapsed}";
    }

    internal static bool HasWarning(AgentStatus status) =>
        status.State == "faulted" || !string.IsNullOrWhiteSpace(status.LastError) ||
        status.Storage?.Pressure is "blocked";

    internal static bool NeedsCameraSelection(AgentStatus status) => status.State == "faulted" &&
        status.LastError?.Contains("choose a camera", StringComparison.OrdinalIgnoreCase) == true;
}
