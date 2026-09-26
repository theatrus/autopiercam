using System.Globalization;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private bool _hasUnsavedSettings;
    private bool _settingsComparisonQueued;
    private SettingsFormValues? _settingsBaseline;

    private void TrackSettingsEdits()
    {
        foreach (NumberBox input in new[] { MaxExposureNumberBox, MaxGainNumberBox, StillIntervalNumberBox,
            RetentionMaxMiBNumberBox, RetentionMinFreeMiBNumberBox })
        {
            input.ValueChanged += (_, _) => MarkSettingsEdited();
            input.LostFocus += (_, _) => MarkSettingsEdited();
            // Text can change before Value commits on Enter/focus loss. Make
            // Save available during editing, not only after tabbing away.
            input.RegisterPropertyChangedCallback(NumberBox.TextProperty, (_, _) => MarkSettingsEdited());
        }
        foreach (TextBox input in new[] { CameraNameFilterTextBox, UploadEndpointTextBox, FfmpegPathTextBox })
            input.TextChanged += (_, _) => MarkSettingsEdited();
        foreach (ToggleSwitch input in new[] { UploadEnabledToggle, VideoEnabledToggle })
            input.Toggled += (_, _) => MarkSettingsEdited();
        foreach (CheckBox input in new[] { AdaptiveExposureCheckBox, Raw16CheckBox })
        {
            input.Checked += (_, _) => MarkSettingsEdited();
            input.Unchecked += (_, _) => MarkSettingsEdited();
        }
    }

    private void MarkSettingsEdited()
    {
        if (_operationInProgress || _settingsBaseline is null || _closed || _settingsComparisonQueued) return;
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

    private void ShowSettingsEditState()
    {
        ConfigInfoBar.Title = _hasUnsavedSettings ? "Unsaved changes" : "Settings loaded";
        ConfigInfoBar.Message = _hasUnsavedSettings
            ? "Save applies your settings and restarts capture. The camera stays unchanged until then."
            : "Settings match the saved configuration.";
        ConfigInfoBar.Severity = InfoBarSeverity.Informational;
    }

    private SettingsFormValues ReadSettingsForm()
    {
        string Number(NumberBox box)
        {
            // Focus can belong to the NumberBox's inner TextBox rather than
            // the NumberBox itself. Include that in pending-text detection.
            for (var focus = FocusManager.GetFocusedElement(Content.XamlRoot) as DependencyObject;
                 focus is not null; focus = VisualTreeHelper.GetParent(focus))
                if (ReferenceEquals(focus, box))
                    return SettingsFormValues.NumberText(box.Text, CultureInfo.CurrentCulture);
            return SettingsFormValues.Number(box.Value);
        }
        return new() {
            MaxExposure = Number(MaxExposureNumberBox), MaxGain = Number(MaxGainNumberBox),
            Interval = Number(StillIntervalNumberBox), RetentionMax = Number(RetentionMaxMiBNumberBox),
            RetentionFree = Number(RetentionMinFreeMiBNumberBox),
            CameraId = _cameraInventoryLoaded && CameraComboBox.SelectedItem is CameraChoice choice
                ? choice.Id : _configurationSnapshot?.Config.Camera.CameraId,
            CameraFilter = SettingsFormValues.Text(CameraNameFilterTextBox.Text),
            Adaptive = AdaptiveExposureCheckBox.IsChecked == true, Raw16 = Raw16CheckBox.IsChecked == true,
            Upload = UploadEnabledToggle.IsOn, Endpoint = SettingsFormValues.Text(UploadEndpointTextBox.Text),
            Video = VideoEnabledToggle.IsOn, Ffmpeg = SettingsFormValues.Text(FfmpegPathTextBox.Text)
        };
    }
}
