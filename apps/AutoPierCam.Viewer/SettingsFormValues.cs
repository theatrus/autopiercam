using System.Globalization;

namespace AutoPierCam.Viewer;

// Compare only the editable values, not notifications or hidden config fields.
// Canonical numbers make localized formatting and trailing zeros immaterial.
internal sealed record SettingsFormValues
{
    internal string MaxExposure { get; init; } = "";
    internal string MaxGain { get; init; } = "";
    internal string Interval { get; init; } = "";
    internal string RetentionMax { get; init; } = "";
    internal string RetentionFree { get; init; } = "";
    internal int? CameraId { get; init; }
    internal string CameraFilter { get; init; } = "";
    internal bool Adaptive { get; init; }
    internal bool Raw16 { get; init; }
    internal bool Upload { get; init; }
    internal string Endpoint { get; init; } = "";
    internal bool Video { get; init; }
    internal string Ffmpeg { get; init; } = "";

    internal static string Text(string? value) => value?.Trim() ?? "";
    internal static string Number(double value) => double.IsNaN(value) ? "disabled" : value == 0 ? "0" : value.ToString("R", CultureInfo.InvariantCulture);
    internal static string NumberText(string? text, CultureInfo culture)
    {
        if (string.IsNullOrWhiteSpace(text)) return "disabled";
        return double.TryParse(text, NumberStyles.Float | NumberStyles.AllowThousands, culture, out double value) && double.IsFinite(value)
            ? Number(value) : "invalid:" + text;
    }

    internal static SettingsFormValues FromConfiguration(AgentConfiguration config) => new() {
        MaxExposure = Number(config.Camera.MaxExposureUs / 1000d), MaxGain = Number(config.Camera.MaxGain),
        Interval = Number(config.Capture.IntervalMs / 1000d),
        RetentionMax = Number(config.Capture.RetentionMaxBytes is ulong max ? max / (1024d * 1024d) : double.NaN),
        RetentionFree = Number(config.Capture.RetentionMinFreeBytes is ulong free ? free / (1024d * 1024d) : double.NaN),
        CameraId = config.Camera.CameraId, CameraFilter = Text(config.Camera.NameContains),
        Adaptive = config.Camera.ExposureControl == "adaptive", Raw16 = config.Camera.Raw16 == true,
        Upload = config.Upload.Enabled, Endpoint = Text(config.Upload.Endpoint),
        Video = config.Video.Enabled, Ffmpeg = Text(config.Video.FfmpegPath)
    };
}
