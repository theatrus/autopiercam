namespace AutoPierCam.Viewer;

internal static class ViewerPresentation
{
    internal static string FormatExposure(long? exposureUs) => exposureUs switch
    {
        null => "—",
        >= 1_000_000 => $"{exposureUs.Value / 1_000_000.0:0.###} s",
        >= 1_000 => $"{exposureUs.Value / 1_000.0:0.###} ms",
        _ => $"{exposureUs.Value:N0} µs",
    };

    internal static string CaptureSummary(long? exposureUs, long? gain, string? mode)
    {
        // Unavailable telemetry should occupy no space, not become placeholder cards.
        var values = new List<string>(3);
        if (exposureUs is > 0) values.Add($"Exposure {FormatExposure(exposureUs)}");
        if (gain is >= 0) values.Add($"Gain {gain.Value:N0}");
        if (mode is "day" or "night") values.Add(mode == "day" ? "Day" : "Night");
        return string.Join(" · ", values);
    }

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
