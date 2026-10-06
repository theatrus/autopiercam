using System.Globalization;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Windows.Globalization.NumberFormatting;

namespace AutoPierCam.Viewer;

// Both tabs retain independent revisioned drafts, with one shared Save footer.
public sealed partial class MainWindow
{
    private SharingSetupState? _sharing;
    private bool _sharingInitialized;
    private bool _sharingBusy;
    private bool _sharingLoading;
    private bool _sharingInvalidDraft;
    private bool _sharingStatusUnknown;
    private volatile bool _sharingPollWanted;
    private (string Title, string Message, InfoBarSeverity Severity)? _sharingFeedback;

    private bool SharingHasEdits => _sharing is { } setup && (setup.IsDirty || _sharingInvalidDraft);
    private bool SharingSupported => _latestAgentStatus?.HasCapability("sharing.get") == true;

    private void InitializeSharingSection()
    {
        foreach (NumberBox box in SharingNumberBoxes)
        {
            TrackNumberEditor(box, SharingChanged);
            // Do not round a fractional draft into a different integer on blur.
            // WholeNumber validates the actual text on save.
            box.NumberFormatter = new DecimalFormatter { FractionDigits = 0, IntegerDigits = 1 };
            box.ValueChanged += (_, _) => SharingChanged();
            // Text can change before Value commits; track both, as Capture does.
            box.RegisterPropertyChangedCallback(NumberBox.TextProperty, (_, _) => SharingChanged());
        }
        SharingOriginTextBox.TextChanged += (_, _) => SharingChanged();
        foreach (ToggleSwitch toggle in new[] { SharingEnabledToggle, SharingSnapshotsToggle, SharingScenesToggle,
            SharingDayNightToggle, SharingTelescopeToggle, SharingChatToggle })
            toggle.Toggled += (_, _) => SharingChanged();
        SharingCodePasswordBox.PasswordChanged += (_, _) => RenderSharing();
        _sharingInitialized = true;
        RenderSharing();
    }

    private NumberBox[] SharingNumberBoxes => new[] {
        SharingIntervalNumberBox, SharingThresholdNumberBox, SharingBurstNumberBox, SharingSpacingNumberBox };

    private void SettingsSectionBar_SelectionChanged(SelectorBar sender, SelectorBarSelectionChangedEventArgs args)
    {
        // XAML can select the first item before later named elements exist.
        if (!_sharingInitialized) return;
        ShowSettingsSection(sharing: ReferenceEquals(sender.SelectedItem, SharingSectionItem));
    }

    private void ShowSettingsSection(bool sharing)
    {
        CaptureSection.Visibility = sharing ? Visibility.Collapsed : Visibility.Visible;
        SharingSection.Visibility = sharing ? Visibility.Visible : Visibility.Collapsed;
        UpdateSharingPolling();
        RenderSharing();
    }

    // Also runs when agent status arrives, so a section opened before the
    // agent answered still loads once sharing support is known.
    private void UpdateSharingPolling()
    {
        if (!_sharingInitialized) return;
        _sharingPollWanted = SettingsPane.Visibility == Visibility.Visible && SharingSupported;
        if (_sharingPollWanted && _sharing is null && !_sharingBusy && !_sharingStatusUnknown)
            _ = RunSharingOperationAsync("Loading Chatstronomy settings…", LoadSharingAsync);
    }

    private ushort SharingNumber(NumberBox box, ushort minimum, ushort maximum, string label)
    {
        return SharingSetupState.WholeNumber(LiveNumberText(box), minimum, maximum, label);
    }

    private SharingPreferences SharingInputs() => new() {
        HubOrigin = SharingOriginTextBox.Text.Trim(),
        Enabled = SharingEnabledToggle.IsOn,
        Snapshots = SharingSnapshotsToggle.IsOn,
        SceneChanges = SharingScenesToggle.IsOn,
        DayNight = SharingDayNightToggle.IsOn,
        SceneThresholdPercent = checked((byte)SharingNumber(SharingThresholdNumberBox, 5, 80, "Changed area threshold")),
        IntervalMinutes = SharingNumber(SharingIntervalNumberBox, 0, 1440, "Send an image every"),
        TelescopeEvents = SharingTelescopeToggle.IsOn,
        ChatConfiguration = SharingChatToggle.IsOn,
        BurstCount = checked((byte)SharingNumber(SharingBurstNumberBox, 1, 3, "Images per event")),
        SpacingSeconds = SharingNumber(SharingSpacingNumberBox, 60, 600, "Time between images"),
    };

    private void PopulateSharing()
    {
        if (_sharing is not { } setup) return;
        _sharingLoading = true;
        var p = setup.Draft;
        SharingOriginTextBox.Text = p.HubOrigin;
        SharingEnabledToggle.IsOn = p.Enabled;
        SharingSnapshotsToggle.IsOn = p.Snapshots;
        SharingScenesToggle.IsOn = p.SceneChanges;
        SharingDayNightToggle.IsOn = p.DayNight;
        SharingThresholdNumberBox.Value = p.SceneThresholdPercent;
        SharingIntervalNumberBox.Value = p.IntervalMinutes;
        SharingTelescopeToggle.IsOn = p.TelescopeEvents;
        SharingBurstNumberBox.Value = p.BurstCount;
        SharingSpacingNumberBox.Value = p.SpacingSeconds;
        SharingChatToggle.IsOn = p.ChatConfiguration;
        ResetNumberEditors(SharingNumberBoxes);
        _sharingInvalidDraft = false;
        _sharingLoading = false;
        RenderSharing();
    }

    private void SharingChanged()
    {
        if (_sharingLoading || _sharingBusy || _sharing is not { } setup) return;
        _settingsSaveResult = null;
        try { setup.Draft = SharingInputs(); _sharingInvalidDraft = false; }
        catch (InvalidOperationException) { _sharingInvalidDraft = true; }
        _sharingFeedback = null;
        RenderSharing();
    }

    private void RenderSharing()
    {
        if (_closed || !_sharingInitialized) return;
        bool supported = SharingSupported;
        bool loaded = _sharing is not null;
        bool paired = _sharing?.Status.DeviceId is not null;
        bool idle = !_sharingBusy && !_operationInProgress && !_savingAll && supported && loaded;
        bool review = _sharing?.NeedsReview == true;

        SharingPairingGroup.Visibility = paired ? Visibility.Collapsed : Visibility.Visible;
        foreach (Control control in new Control[] { SharingOriginTextBox, SharingCodePasswordBox, SharingSnapshotsToggle,
            SharingScenesToggle, SharingDayNightToggle, SharingTelescopeToggle, SharingChatToggle,
            SharingIntervalNumberBox, SharingThresholdNumberBox, SharingBurstNumberBox, SharingSpacingNumberBox })
            control.IsEnabled = idle;
        SharingEnabledToggle.IsEnabled = idle && paired && !_sharingStatusUnknown;
        SharingPairButton.IsEnabled = idle && !_sharingStatusUnknown && !review &&
            !string.IsNullOrWhiteSpace(SharingOriginTextBox.Text) && !string.IsNullOrWhiteSpace(SharingCodePasswordBox.Password);
        SharingForgetConsentCheckBox.IsEnabled = idle && paired;
        SharingForgetButton.IsEnabled = idle && paired && !_sharingStatusUnknown && SharingForgetConsentCheckBox.IsChecked == true;
        SharingStopButton.IsEnabled = idle &&
            (_sharing!.Status.Preferences.Enabled || _sharingStatusUnknown);
        SharingReloadButton.IsEnabled = !_sharingBusy && !_operationInProgress && !_savingAll && supported;
        SharingKeepEditsButton.Visibility = review ? Visibility.Visible : Visibility.Collapsed;
        SharingKeepEditsButton.IsEnabled = idle && !_sharingStatusUnknown;

        SharingConnectionText.Text = !supported ? "Unavailable · this agent does not support Chatstronomy sharing"
            : _sharing is not { } s ? "Not loaded"
            : paired ? $"Paired · {(s.Status.Preferences.Enabled ? s.Status.Connection : "Sharing off")}"
            : "Not paired · No images are shared";
        SharingDetailsText.Text = _sharing is { } loadedSetup ? SharingDetails(loadedSetup.Status) : "";

        // One InfoBar, as in Capture: the last action's result, otherwise the
        // edit state. Hidden when there is nothing to say.
        (string Title, string Message, InfoBarSeverity Severity)? state =
            !supported ? ("Unavailable", "Update the AutoPierCam agent to share images with Chatstronomy.", InfoBarSeverity.Warning)
            : _sharingStatusUnknown ? ("Status unknown", "Reload settings before saving or pairing.", InfoBarSeverity.Warning)
            : review ? ("Settings changed elsewhere", "Discard to load the new settings. Keep edits to overwrite them on Save.", InfoBarSeverity.Error)
            : _sharing?.HasChatOverrides == true ? ("Chat overrides active", "Save settings to restore your local trigger choices.", InfoBarSeverity.Informational)
            : null;
        if (_sharingFeedback is { } feedback && !review && !_sharingStatusUnknown) state = feedback;
        SharingInfoBar.IsOpen = state is not null;
        if (state is { } shown)
        {
            SharingInfoBar.Title = shown.Title;
            SharingInfoBar.Message = shown.Message;
            SharingInfoBar.Severity = shown.Severity;
            SharingInfoBar.IsIconVisible = shown.Severity is InfoBarSeverity.Warning or InfoBarSeverity.Error;
        }
        UpdateSettingsButton();
    }

    private static string SharingDetails(SharingStatus state)
    {
        string text = $"Hub: {state.Preferences.HubOrigin}\nDevice: {state.DeviceId?.ToString(CultureInfo.CurrentCulture) ?? "not paired"}\nInstallation: {state.InstallationId}\nConnection: {state.Connection}";
        if (state.LastDeliveryUnixMs is ulong ms && ms <= 253402300799999)
            text += $"\nLast delivery: {DateTimeOffset.FromUnixTimeMilliseconds((long)ms).ToLocalTime():g}";
        if (state.ActiveTriggers is { } active)
            text += $"\n{(state.Preferences.Enabled ? "Active" : "Configured")} triggers: every {active.IntervalMinutes} min (0 = off); scene {(active.SceneChanges ? "on" : "off")}; day/night {(active.DayNight ? "on" : "off")}; telescope {(active.TelescopeEvents ? "on" : "off")}; {active.BurstCount} images, {active.SpacingSeconds}s apart";
        return text;
    }

    private void SetSharingFeedback(string title, string message, InfoBarSeverity severity = InfoBarSeverity.Success) =>
        _sharingFeedback = (title, message, severity);

    private async Task LoadSharingAsync(CancellationToken cancellationToken)
    {
        var latest = await _agentClient.GetSharingAsync(cancellationToken);
        if (_sharing is null) _sharing = new SharingSetupState(latest);
        else _sharing.Accept(latest);
        _sharingStatusUnknown = false;
        _sharingFeedback = null;
        PopulateSharing();
    }

    private async Task<bool> RunSharingOperationAsync(string working, Func<CancellationToken, Task> action, bool pairing = false)
    {
        if (_sharingBusy || _closed) return false;
        _sharingBusy = true;
        _sharingFeedback = ("Working", working, InfoBarSeverity.Informational);
        SetControlsForOperation(_operationInProgress);
        try { await action(_lifetime.Token); return true; }
        catch (OperationCanceledException) when (_lifetime.IsCancellationRequested) { return false; }
        catch (Exception error)
        {
            string message = error is OperationCanceledException ? "Operation cancelled." : error.Message;
            // Pairing can persist a revision even if the request fails.
            // Reconcile without discarding the draft or retrying the code.
            if (_sharing is { } setup)
            {
                try
                {
                    setup.Refresh(await _agentClient.GetSharingAsync(_lifetime.Token), _sharingInvalidDraft);
                    _sharingStatusUnknown = false;
                }
                catch { _sharingStatusUnknown = true; }
            }
            else _sharingStatusUnknown = true;
            if (pairing) message += " A code may have been consumed; get a new code from the Hub before retrying.";
            SetSharingFeedback("Not completed", message, InfoBarSeverity.Error);
            return false;
        }
        finally
        {
            _sharingBusy = false;
            if (!_closed) SetControlsForOperation(_operationInProgress);
        }
    }

    // Called by the status loop while the settings window is open, so
    // connection state stays current the way Capture status does.
    private void ApplyPolledSharing(SharingStatus latest)
    {
        if (_sharingBusy || _savingAll || _sharing is not { } setup || _closed) return;
        var shown = setup.Draft;
        setup.Refresh(latest, _sharingInvalidDraft);
        _sharingStatusUnknown = false;
        if (!SharingHasEdits && !setup.NeedsReview && setup.Draft != shown) PopulateSharing();
        else RenderSharing();
    }

    private async void SharingReloadButton_Click(object sender, RoutedEventArgs e)
    {
        if ((SharingHasEdits || _sharing?.NeedsReview == true) && !await ConfirmDiscardAsync()) return;
        await RunSharingOperationAsync("Reloading Chatstronomy settings…", LoadSharingAsync);
    }

    private void SharingKeepEditsButton_Click(object sender, RoutedEventArgs e)
    {
        if (_sharing is not { } setup) return;
        setup.KeepEdits();
        SetSharingFeedback("Your edits are kept", "Saving will replace the agent settings and reset chat overrides.", InfoBarSeverity.Informational);
        RenderSharing();
    }

    private async Task SaveSharingAsync(CancellationToken ct)
    {
        if (_sharing is not { } setup) return;
        // Always validate the current text, never a cached load-time error.
        setup.Draft = SharingInputs();
        _sharingInvalidDraft = false;
        setup.Accept(await _agentClient.ConfigureSharingAsync(setup.ExpectedRevision, setup.ForSave(), ct));
        PopulateSharing();
        SetSharingFeedback("Settings saved", setup.Status.Preferences.Enabled
            ? "Connecting to the Hub. Connection status updates automatically."
            : "Sharing is off.");
    }

    private async void SharingStopButton_Click(object sender, RoutedEventArgs e)
    {
        if (_sharing is not { } setup) return;
        await RunSharingOperationAsync("Stopping sharing…", async ct => {
            // Stop immediately using saved permissions, not the pending draft.
            var latest = await _agentClient.GetSharingAsync(ct);
            setup.Stopped(latest, await _agentClient.ConfigureSharingAsync(latest.Revision, latest.Preferences with { Enabled = false }, ct), _sharingInvalidDraft);
            _sharingStatusUnknown = false;
            if (!_sharingInvalidDraft) PopulateSharing();
            else
            {
                _sharingLoading = true;
                SharingEnabledToggle.IsOn = false;
                _sharingLoading = false;
            }
            SetSharingFeedback("Sharing stopped", "Queued images were discarded. Your other edits are still here.");
        });
    }

    private async void SharingPairButton_Click(object sender, RoutedEventArgs e)
    {
        if (_sharing is not { } setup) return;
        string token;
        try { token = SharingSetupState.PairingCode(SharingCodePasswordBox.Password); setup.Draft = SharingInputs(); }
        catch (InvalidOperationException error)
        {
            SetSharingFeedback("Check the pairing details", error.Message, InfoBarSeverity.Error);
            RenderSharing();
            return;
        }
        SharingCodePasswordBox.Password = "";
        await RunSharingOperationAsync("Pairing with the Hub…", async ct => {
            setup.Accept(await _agentClient.ConfigureSharingAsync(setup.ExpectedRevision, setup.Draft with { Enabled = false }, ct));
            PopulateSharing();
            setup.Accept(await _agentClient.PairSharingAsync(setup.ExpectedRevision, token, ct));
            PopulateSharing();
            SetSharingFeedback("Paired", "Your choices are saved. Turn on Enable image sharing and save when ready.");
        }, pairing: true);
        if (setup.Status.DeviceId is not null) SharingEnabledToggle.Focus(FocusState.Programmatic);
    }

    private void SharingForgetConsent_Changed(object sender, RoutedEventArgs e) => RenderSharing();

    private async void SharingForgetButton_Click(object sender, RoutedEventArgs e)
    {
        if (_sharing is not { } setup) return;
        await RunSharingOperationAsync("Forgetting the pairing…", async ct => {
            setup.Accept(await _agentClient.ForgetSharingAsync(setup.Status.Revision, ct));
            SharingCodePasswordBox.Password = "";
            SharingForgetConsentCheckBox.IsChecked = false;
            PopulateSharing();
            SetSharingFeedback("Pairing removed", "Pairing and permissions cleared. Revoke the device credential in the Hub too.");
        });
    }

    // Shared by both sections' Reload buttons.
    private async Task<bool> ConfirmDiscardAsync()
    {
        if (_closingSettingsDialog) return false;
        _closingSettingsDialog = true;
        try
        {
            var confirm = new ContentDialog {
                XamlRoot = SettingsPane.XamlRoot,
                Title = "Discard unsaved settings?",
                Content = "Reloading this tab from the agent will discard its unsaved changes. Edits in the other tab are kept.",
                PrimaryButtonText = "Discard and reload",
                CloseButtonText = "Keep editing",
                DefaultButton = ContentDialogButton.Close,
            };
            return await confirm.ShowAsync() == ContentDialogResult.Primary;
        }
        finally { _closingSettingsDialog = false; }
    }
}
