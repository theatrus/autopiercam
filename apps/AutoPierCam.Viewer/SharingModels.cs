using System.Text.Json.Serialization;

namespace AutoPierCam.Viewer;

internal sealed record SharingPreferences
{
    [JsonPropertyName("hub_origin")] public string HubOrigin { get; init; } = "";
    [JsonPropertyName("enabled")] public bool Enabled { get; init; }
    [JsonPropertyName("snapshots")] public bool Snapshots { get; init; }
    [JsonPropertyName("scene_changes")] public bool SceneChanges { get; init; }
    [JsonPropertyName("day_night")] public bool DayNight { get; init; }
    [JsonPropertyName("scene_threshold_percent")] public byte SceneThresholdPercent { get; init; } = 20;
    [JsonPropertyName("interval_minutes")] public ushort IntervalMinutes { get; init; }
    [JsonPropertyName("telescope_events")] public bool TelescopeEvents { get; init; }
    [JsonPropertyName("observation_context")] public bool ObservationContext { get; init; }
    [JsonPropertyName("timelapses")] public bool Timelapses { get; init; }
    [JsonPropertyName("overlays"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public SharingOverlayOptions? Overlays { get; init; }
    [JsonPropertyName("chat_configuration")] public bool ChatConfiguration { get; init; }
    [JsonPropertyName("burst_count")] public byte BurstCount { get; init; } = 1;
    [JsonPropertyName("spacing_seconds")] public ushort SpacingSeconds { get; init; } = 60;
}

internal sealed record SharingOverlayOptions
{
    [JsonPropertyName("telescope_id")] public long? TelescopeId { get; init; }
    [JsonPropertyName("target")] public bool Target { get; init; }
    [JsonPropertyName("exposure_count")] public bool ExposureCount { get; init; }
    [JsonPropertyName("temperature")] public string Temperature { get; init; } = "off";

    public static SharingOverlayOptions Parse(string telescopeId, bool target, bool count, string temperature)
    {
        long? id = null;
        if (!string.IsNullOrWhiteSpace(telescopeId))
        {
            if (!long.TryParse(telescopeId.Trim(), System.Globalization.NumberStyles.None,
                System.Globalization.CultureInfo.InvariantCulture, out long value) || value <= 0 || value > 9007199254740991)
                throw new InvalidOperationException("Enter the positive telescope record ID shown in the Hub.");
            id = value;
        }
        if (temperature is not ("off" or "focuser" or "external"))
            throw new InvalidOperationException("Choose a temperature source.");
        if ((target || count || temperature != "off") && id is null)
            throw new InvalidOperationException("Choose a Hub telescope ID before enabling overlays.");
        return new() { TelescopeId = id, Target = target, ExposureCount = count, Temperature = temperature };
    }
}

internal sealed record SharingTriggerRules
{
    [JsonPropertyName("interval_minutes")] public ushort IntervalMinutes { get; init; }
    [JsonPropertyName("scene_changes")] public bool SceneChanges { get; init; }
    [JsonPropertyName("day_night")] public bool DayNight { get; init; }
    [JsonPropertyName("telescope_events")] public bool TelescopeEvents { get; init; }
    [JsonPropertyName("burst_count")] public byte BurstCount { get; init; }
    [JsonPropertyName("spacing_seconds")] public ushort SpacingSeconds { get; init; }
}

internal sealed record SharingStatus
{
    [JsonPropertyName("revision")] public ulong Revision { get; init; }
    [JsonPropertyName("installation_id")] public string InstallationId { get; init; } = "";
    [JsonPropertyName("device_id")] public long? DeviceId { get; init; }
    [JsonPropertyName("preferences")] public SharingPreferences Preferences { get; init; } = new();
    [JsonPropertyName("connection")] public string Connection { get; init; } = "";
    [JsonPropertyName("last_delivery_unix_ms")] public ulong? LastDeliveryUnixMs { get; init; }
    [JsonPropertyName("active_triggers")] public SharingTriggerRules? ActiveTriggers { get; init; }
}
