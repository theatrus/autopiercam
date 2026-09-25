using System.Text.Json.Serialization;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

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

public sealed partial class MainWindow
{
    private async void SharingButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Loading Chatstronomy sharing…", ShowSharingAsync);
    }

    private async Task ShowSharingAsync(CancellationToken cancellationToken)
    {
        SharingStatus status = await _agentClient.GetSharingAsync(cancellationToken);
        var panel = new StackPanel { Spacing = 12, Width = 460 };
        var notice = new TextBlock {
            Text = "Share the full pier-camera preview to the Discord channels selected on your Hub device. Pairing alone shares nothing. Snapshot now uses the latest completed frame (up to 120 seconds old) and never changes exposure.",
            TextWrapping = TextWrapping.Wrap
        };
        var origin = new TextBox { Header = "Hub HTTPS origin", PlaceholderText = "https://your-chatstronomy-hub" };
        var connection = new TextBlock { TextWrapping = TextWrapping.Wrap };
        var enabled = new ToggleSwitch { Header = "Connect and permit image sharing" };
        var snapshots = new ToggleSwitch { Header = "Allow Hub Snapshot now requests" };
        var scenes = new ToggleSwitch { Header = "Post persistent scene changes (entire preview)" };
        var dayNight = new ToggleSwitch { Header = "Post stable day/night transitions" };
        var threshold = new NumberBox { Header = "Changed area threshold (%)", Minimum = 5, Maximum = 80, SmallChange = 5 };
        var interval = new NumberBox { Header = "Periodic images: minutes (0 = disabled)", Minimum = 0, Maximum = 1440, SmallChange = 1 };
        var telescopeEvents = new ToggleSwitch { Header = "Allow slew / sequence event images" };
        var burst = new NumberBox { Header = "Images per scene / telescope event (maximum)", Minimum = 1, Maximum = 3, SmallChange = 1 };
        var spacing = new NumberBox { Header = "Seconds between event images (minimum)", Minimum = 60, Maximum = 600, SmallChange = 30 };
        var chatConfiguration = new ToggleSwitch { Header = "Allow the camera owner to configure triggers from chat" };
        var triggerNotice = new TextBlock {
            Text = "Chat can disable allowed triggers, slow periodic sends, or reduce bursts—but cannot enable a locally disabled source or exceed these limits. Save permissions resets chat overrides. Telescope triggers require the same owner and a shared Hub channel. Each burst waits for distinct completed frames; busy events are coalesced, not queued.",
            TextWrapping = TextWrapping.Wrap
        };
        var code = new PasswordBox { Header = "One-use device pairing code", PlaceholderText = "Code from Observatory devices in the Hub" };
        var pair = new Button { Content = "Pair camera (sharing stays off)" };
        var forget = new Button { Content = "Stop and forget local pairing" };
        var reload = new Button { Content = "Refresh status / reload settings" };
        var feedback = new TextBlock { TextWrapping = TextWrapping.Wrap };
        var warning = new TextBlock {
            Text = "Events are conservative scene observations, not person/animal or threat detection. Retries can duplicate a post after a Hub crash. Turning sharing off discards queued images but cannot recall messages already posted. Forgetting removes the local credential; revoke it in the Hub as well.",
            TextWrapping = TextWrapping.Wrap
        };
        foreach (var element in new FrameworkElement[] { notice, connection, origin, enabled, snapshots, scenes, dayNight, threshold, interval, telescopeEvents, burst, spacing, chatConfiguration, triggerNotice, code, pair, forget, reload, feedback, warning })
            panel.Children.Add(element);
        var dialog = new ContentDialog {
            XamlRoot = Content.XamlRoot, Title = "Chatstronomy image sharing",
            Content = new ScrollViewer { Content = panel, MaxHeight = 600 },
            PrimaryButtonText = "Save permissions", SecondaryButtonText = "Stop sharing",
            CloseButtonText = "Close", DefaultButton = ContentDialogButton.Close
        };
        bool busy = false;
        void Apply()
        {
            origin.Text = status.Preferences.HubOrigin;
            origin.IsEnabled = status.DeviceId is null && !busy;
            enabled.IsOn = status.Preferences.Enabled;
            snapshots.IsOn = status.Preferences.Snapshots;
            scenes.IsOn = status.Preferences.SceneChanges;
            dayNight.IsOn = status.Preferences.DayNight;
            threshold.Value = status.Preferences.SceneThresholdPercent;
            interval.Value = status.Preferences.IntervalMinutes;
            telescopeEvents.IsOn = status.Preferences.TelescopeEvents;
            burst.Value = status.Preferences.BurstCount;
            spacing.Value = status.Preferences.SpacingSeconds;
            chatConfiguration.IsOn = status.Preferences.ChatConfiguration;
            connection.Text = $"{status.Connection} · Device: {status.DeviceId?.ToString() ?? "not paired"}\nInstallation: {status.InstallationId}" +
                (status.LastDeliveryUnixMs is ulong ms ? $"\nLast delivery: {DateTimeOffset.FromUnixTimeMilliseconds((long)ms).ToLocalTime():g}" : "");
            if (status.ActiveTriggers is { } active)
                connection.Text += $"\nActive: periodic {(active.IntervalMinutes == 0 ? "off" : $"{active.IntervalMinutes} min")}; scene {active.SceneChanges}; day/night {active.DayNight}; telescope {active.TelescopeEvents}; burst {active.BurstCount}, {active.SpacingSeconds}s spacing";
        }
        SharingPreferences Inputs(bool permit) => new() {
            HubOrigin = origin.Text.Trim(), Enabled = permit && enabled.IsOn,
            Snapshots = snapshots.IsOn, SceneChanges = scenes.IsOn, DayNight = dayNight.IsOn,
            SceneThresholdPercent = double.IsFinite(threshold.Value) ? checked((byte)threshold.Value) : (byte)20,
            IntervalMinutes = double.IsFinite(interval.Value) ? checked((ushort)interval.Value) : (ushort)0,
            TelescopeEvents = telescopeEvents.IsOn, ChatConfiguration = chatConfiguration.IsOn,
            BurstCount = double.IsFinite(burst.Value) ? checked((byte)burst.Value) : (byte)1,
            SpacingSeconds = double.IsFinite(spacing.Value) ? checked((ushort)spacing.Value) : (ushort)60
        };
        async Task Operate(Func<Task> action)
        {
            if (busy) return;
            busy = true;
            pair.IsEnabled = forget.IsEnabled = reload.IsEnabled = false;
            dialog.IsPrimaryButtonEnabled = dialog.IsSecondaryButtonEnabled = false;
            feedback.Text = "";
            try { await action(); }
            catch (OperationCanceledException) {
                feedback.Text = "Operation cancelled. Refresh to confirm whether it completed.";
            }
            catch (Exception error) {
                feedback.Text = error.Message + " Refresh before retrying. A pairing code may have been consumed.";
            }
            finally {
                busy = false;
                pair.IsEnabled = forget.IsEnabled = reload.IsEnabled = true;
                dialog.IsPrimaryButtonEnabled = dialog.IsSecondaryButtonEnabled = true;
                origin.IsEnabled = status.DeviceId is null;
            }
        }
        pair.Click += async (_, _) => await Operate(async () => {
            string token = code.Password; code.Password = "";
            status = await _agentClient.ConfigureSharingAsync(status.Revision, Inputs(false), cancellationToken);
            Apply();
            status = await _agentClient.PairSharingAsync(status.Revision, token, cancellationToken);
            Apply(); feedback.Text = "Paired. Choose the permissions above and save to enable sharing.";
        });
        forget.Click += async (_, _) => await Operate(async () => {
            code.Password = "";
            status = await _agentClient.ForgetSharingAsync(status.Revision, cancellationToken);
            Apply(); feedback.Text = "Local pairing removed. Revoke the device credential in the Hub too.";
        });
        reload.Click += async (_, _) => await Operate(async () => {
            status = await _agentClient.GetSharingAsync(cancellationToken); Apply();
        });
        dialog.PrimaryButtonClick += async (_, args) => {
            args.Cancel = true;
            var deferral = args.GetDeferral();
            try { await Operate(async () => {
                status = await _agentClient.ConfigureSharingAsync(status.Revision, Inputs(true), cancellationToken);
                Apply(); feedback.Text = "Permissions saved. Refresh to check the connection.";
            }); } finally { deferral.Complete(); }
        };
        dialog.SecondaryButtonClick += async (_, args) => {
            args.Cancel = true;
            var deferral = args.GetDeferral();
            try { await Operate(async () => {
                status = await _agentClient.ConfigureSharingAsync(status.Revision, status.Preferences with { Enabled = false }, cancellationToken);
                Apply(); feedback.Text = "Sharing stopped; queued images discarded.";
            }); } finally { deferral.Complete(); }
        };
        dialog.Closing += (_, args) => { if (busy) args.Cancel = true; };
        Apply();
        await dialog.ShowAsync();
        code.Password = "";
    }
}
