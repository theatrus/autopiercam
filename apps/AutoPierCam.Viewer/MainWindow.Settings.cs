using Microsoft.UI.Xaml.Controls;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private bool _hasUnsavedSettings;

    private void TrackSettingsEdits()
    {
        foreach (NumberBox input in new[] { MaxExposureNumberBox, MaxGainNumberBox, StillIntervalNumberBox,
            RetentionMaxMiBNumberBox, RetentionMinFreeMiBNumberBox })
            input.ValueChanged += (_, _) => MarkSettingsEdited();
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
        if (_operationInProgress || _configurationSnapshot is null || _closed) return;
        _hasUnsavedSettings = true;
        ConfigInfoBar.Title = "Unsaved changes";
        ConfigInfoBar.Message = "Save applies your settings and restarts capture. The camera stays unchanged until then.";
        ConfigInfoBar.Severity = InfoBarSeverity.Informational;
        SetControlsForOperation(false);
    }
}
