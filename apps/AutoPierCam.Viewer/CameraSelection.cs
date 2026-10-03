using System.Text.Json.Serialization;

namespace AutoPierCam.Viewer;

internal sealed record DetectedCamera(
    [property: JsonPropertyName("id"), JsonRequired] int Id,
    [property: JsonPropertyName("name"), JsonRequired] string Name,
    [property: JsonPropertyName("is_color"), JsonRequired] bool IsColor,
    [property: JsonPropertyName("serial")] string? Serial = null,
    [property: JsonPropertyName("discovery_error")] string? DiscoveryError = null);

internal sealed record CameraInventory(
    [property: JsonPropertyName("cameras"), JsonRequired] IReadOnlyList<DetectedCamera> Cameras,
    [property: JsonPropertyName("scanned_at_unix_ms"), JsonRequired] long? ScannedAtUnixMs,
    [property: JsonPropertyName("error"), JsonRequired] string? Error);

internal sealed record CameraChoice(int? Id, string? NameFilter, string Label, string? Serial = null)
{
    public override string ToString() => Label;

    internal static IReadOnlyList<CameraChoice> Create(CameraInventory inventory, int? savedId, string? savedFilter, string? savedSerial = null)
    {
        List<CameraChoice> choices = [new(null, null, "Automatic (use model filter)")];
        foreach (DetectedCamera camera in inventory.Cameras.Where(camera => camera.IsColor))
        {
            string identity = camera.Serial is { Length: > 0 } serial ? $"serial {serial}" : $"camera ID {camera.Id} · serial unavailable";
            string error = camera.DiscoveryError is { } detail ? $" · {detail}" : string.Empty;
            choices.Add(new(camera.Id, camera.Name, $"{camera.Name} · {identity}{error}", camera.Serial));
        }
        if ((savedId is not null || savedSerial is not null) && choices.Count(choice => Matches(choice, savedId, savedFilter, savedSerial)) != 1)
        {
            choices.Add(new(savedId, savedFilter, $"Saved camera {savedSerial ?? savedId?.ToString()} · unavailable or ambiguous", savedSerial));
        }
        return choices;
    }

    internal static CameraChoice Selected(IReadOnlyList<CameraChoice> choices, int? id, string? filter, string? serial = null) =>
        choices.Last(choice => Matches(choice, id, filter, serial));

    private static bool Matches(CameraChoice choice, int? id, string? filter, string? serial) =>
        (serial is not null ? string.Equals(choice.Serial, serial, StringComparison.OrdinalIgnoreCase) : choice.Id == id) &&
        (id is null && serial is null || string.IsNullOrEmpty(filter) ||
            choice.NameFilter?.Contains(filter, StringComparison.OrdinalIgnoreCase) == true);
}
