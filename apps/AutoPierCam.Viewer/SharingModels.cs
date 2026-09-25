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
    [JsonPropertyName("chat_configuration")] public bool ChatConfiguration { get; init; }
    [JsonPropertyName("burst_count")] public byte BurstCount { get; init; } = 1;
    [JsonPropertyName("spacing_seconds")] public ushort SpacingSeconds { get; init; } = 60;
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
