using System.Text.Json.Serialization;

namespace AutoPierCam.Viewer;

internal sealed record DetectedCamera(
    [property: JsonPropertyName("id"), JsonRequired] int Id,
    [property: JsonPropertyName("name"), JsonRequired] string Name,
    [property: JsonPropertyName("is_color"), JsonRequired] bool IsColor);

internal sealed record CameraInventory(
    [property: JsonPropertyName("cameras"), JsonRequired] IReadOnlyList<DetectedCamera> Cameras,
    [property: JsonPropertyName("scanned_at_unix_ms"), JsonRequired] long? ScannedAtUnixMs,
    [property: JsonPropertyName("error"), JsonRequired] string? Error);

internal sealed record CameraChoice(int? Id, string? NameFilter, string Label)
{
    public override string ToString() => Label;

    internal static IReadOnlyList<CameraChoice> Create(CameraInventory inventory, int? savedId, string? savedFilter)
    {
        List<CameraChoice> choices = [new(null, null, "Automatic (use model filter)")];
        foreach (DetectedCamera camera in inventory.Cameras.Where(camera => camera.IsColor))
        {
            choices.Add(new(camera.Id, camera.Name, $"{camera.Name} · camera ID {camera.Id}"));
        }
        if (savedId is { } id && !choices.Any(choice => Matches(choice, id, savedFilter)))
        {
            choices.Add(new(id, savedFilter, $"Saved camera ID {id} · unavailable"));
        }
        return choices;
    }

    internal static CameraChoice Selected(IReadOnlyList<CameraChoice> choices, int? id, string? filter) =>
        choices.Last(choice => Matches(choice, id, filter));

    private static bool Matches(CameraChoice choice, int? id, string? filter) =>
        choice.Id == id && (id is null || string.IsNullOrEmpty(filter) ||
            choice.NameFilter?.Contains(filter, StringComparison.OrdinalIgnoreCase) == true);
}
