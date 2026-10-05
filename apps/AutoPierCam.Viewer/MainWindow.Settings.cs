using System.Globalization;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private bool _hasUnsavedSettings;
    private bool _settingsComparisonQueued;
    private SettingsFormValues? _settingsBaseline;

    private void TrackSettingsEdits()
    {
        foreach (NumberBox input in CaptureNumberBoxes)
        {
            TrackNumberEditor(input, MarkSettingsEdited);
            input.ValueChanged += (_, _) => MarkSettingsEdited();
            input.LostFocus += (_, _) => MarkSettingsEdited();
            // Text can change before Value commits on Enter/focus loss. Make
            // Save available during editing, not only after tabbing away.
            input.RegisterPropertyChangedCallback(NumberBox.TextProperty, (_, _) => MarkSettingsEdited());
        }
        foreach (TextBox input in new[] { CameraNameFilterTextBox, CameraSerialTextBox, UploadEndpointTextBox, FfmpegPathTextBox })
            input.TextChanged += (_, _) => MarkSettingsEdited();
        foreach (ToggleSwitch input in new[] { UploadEnabledToggle, VideoEnabledToggle, AdaptiveExposureToggle, Raw16Toggle, PreferShortExposuresToggle, UsbResetOnFaultToggle })
            input.Toggled += (_, _) => MarkSettingsEdited();
        CameraDriverComboBox.SelectionChanged += (_, _) => MarkSettingsEdited();
        WhiteBalanceModeComboBox.SelectionChanged += (_, _) => {
            MarkSettingsEdited();
            SetControlsForOperation(_operationInProgress);
        };

    }

    private NumberBox[] CaptureNumberBoxes => new[] { MaxExposureNumberBox, MaxGainNumberBox, MinGainNumberBox, StillIntervalNumberBox,
        RetentionMaxMiBNumberBox, RetentionMinFreeMiBNumberBox, PreviewMaxFpsNumberBox, LatitudeNumberBox, LongitudeNumberBox,
        WhiteBalanceRedNumberBox, WhiteBalanceBlueNumberBox };

    private void MarkSettingsEdited()
    {
        if (_operationInProgress || _settingsBaseline is null || _closed || _settingsComparisonQueued) return;
        _settingsSaveResult = null;
        // NumberBox can notify Text and Value separately during formatting.
        // Compare after the current control update, not its intermediate state.
        _settingsComparisonQueued = DispatcherQueue.TryEnqueue(() => {
            _settingsComparisonQueued = false;
            if (_operationInProgress || _settingsBaseline is null || _closed) return;
            bool changed = ReadSettingsForm() != _settingsBaseline;
            if (changed == _hasUnsavedSettings) return;
            _hasUnsavedSettings = changed;
            ShowSettingsEditState();
            SetControlsForOperation(false);
        });
    }

    private async void CaptureDiscardButton_Click(object sender, RoutedEventArgs e) => await DiscardAllSettingsAsync();

    private async void CaptureKeepEditsButton_Click(object sender, RoutedEventArgs e) =>
        await RunUiOperationAsync("Loading the latest settings to keep your edits…", KeepCaptureEditsAsync);

    // Adopt the newer revision and hidden fields without touching the form,
    // like SharingSetupState.KeepEdits. The next save replaces the other edit.
    private async Task KeepCaptureEditsAsync(CancellationToken cancellationToken)
    {
        AgentConfigurationSnapshot latest = await _agentClient.GetConfigurationAsync(cancellationToken);
        _configurationSnapshot = latest;
        _settingsBaseline = SettingsFormValues.FromConfiguration(latest.Config);
        _configurationNeedsRefresh = false;
        _captureNeedsReview = false;
        CaptureKeepEditsButton.Visibility = Visibility.Collapsed;
        _hasUnsavedSettings = ReadSettingsForm() != _settingsBaseline;
        ConfigInfoBar.Title = "Your edits are kept";
        ConfigInfoBar.Message = "Save will overwrite the other changes.";
        SetConfigurationFeedback(InfoBarSeverity.Informational, true);
        StatusText.Text = "Your settings edits are kept; save to apply them.";
    }

    private void ShowSettingsEditState()
    {
        ConfigInfoBar.Title = _hasUnsavedSettings ? "Unsaved changes" : "Settings loaded";
        ConfigInfoBar.Message = _hasUnsavedSettings
            ? "Save applies changes. Camera, RAW format or white-balance changes restart capture."
            : "Settings match the saved configuration.";
        SetConfigurationFeedback(InfoBarSeverity.Informational, _hasUnsavedSettings);
    }

    private SettingsFormValues ReadSettingsForm()
    {
        string Number(NumberBox box)
        {
            return SettingsFormValues.NumberText(LiveNumberText(box), CultureInfo.CurrentCulture);
        }
        return new() {
            Driver = CameraDriverComboBox.SelectedIndex == 1 ? "zwo_direct" : "zwo_sdk",
            Serial = SettingsFormValues.Text(CameraSerialTextBox.Text),
            UsbResetOnFault = UsbResetOnFaultToggle.IsOn,
            Latitude = Number(LatitudeNumberBox), Longitude = Number(LongitudeNumberBox),
            WhiteBalanceMode = SelectedWhiteBalanceMode(),
            WhiteBalanceRed = WhiteBalanceModeComboBox.SelectedIndex > 0 ? Number(WhiteBalanceRedNumberBox) : "1",
            WhiteBalanceBlue = WhiteBalanceModeComboBox.SelectedIndex > 0 ? Number(WhiteBalanceBlueNumberBox) : "1",
            MaxExposure = Number(MaxExposureNumberBox), MaxGain = Number(MaxGainNumberBox),
            MinGain = Number(MinGainNumberBox), PreferShortExposures = PreferShortExposuresToggle.IsOn,
            Interval = Number(StillIntervalNumberBox), RetentionMax = Number(RetentionMaxMiBNumberBox),
            PreviewRate = Number(PreviewMaxFpsNumberBox),
            RetentionFree = Number(RetentionMinFreeMiBNumberBox),
            CameraId = _cameraInventoryLoaded && CameraComboBox.SelectedItem is CameraChoice choice
                ? choice.Id : _configurationSnapshot?.Config.Camera.CameraId,
            CameraFilter = SettingsFormValues.Text(CameraNameFilterTextBox.Text),
            Adaptive = AdaptiveExposureToggle.IsOn, Raw16 = Raw16Toggle.IsOn,
            Upload = UploadEnabledToggle.IsOn, Endpoint = SettingsFormValues.Text(UploadEndpointTextBox.Text),
            Video = VideoEnabledToggle.IsOn, Ffmpeg = SettingsFormValues.Text(FfmpegPathTextBox.Text)
        };
    }
}
