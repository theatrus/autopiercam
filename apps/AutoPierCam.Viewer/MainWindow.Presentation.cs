using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private bool _cameraSettingsPrompted;
    private string _previewDimensions = "";
    private long? _lastPreviewGain;
    private string? _lastPreviewMode;

    private void SetCaptureSummary(long? exposureUs, long? gain, string? mode)
    {
        string summary = ViewerPresentation.CaptureSummary(exposureUs, gain, mode);
        CaptureSummaryText.Text = summary;
        CaptureSummaryText.Visibility = summary.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
        ToolTipService.SetToolTip(CaptureSummaryText, summary);
    }

    private void SettingsButton_Click(object sender, RoutedEventArgs args) =>
        SetSettingsVisible(true);

    private void SetSettingsVisible(bool visible)
    {
        if (visible) EnsureSettingsWindow();
        SettingsPane.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        if (visible) _settingsWindow!.Activate();
        else _settingsWindow?.AppWindow.Hide();
        UpdateSettingsButton();
        UpdateSharingPolling();
        if (!visible) SettingsButton.Focus(FocusState.Programmatic);
    }

    private void WorkspaceGrid_SizeChanged(object sender, SizeChangedEventArgs args) => UpdateSettingsLayout();

    private void UpdateSettingsLayout()
    {
        // Settings has its own window; opening it never hides or shrinks preview.
        PreviewPane.Visibility = Visibility.Visible;
        PreviewColumn.Width = new GridLength(1, GridUnitType.Star);
        SettingsColumn.Width = new GridLength(0);
        WorkspaceGrid.ColumnSpacing = 0;
    }

    private void UpdateSettingsButton()
    {
        SettingsButton.Content = "Settings" + (_hasUnsavedSettings || SharingHasEdits ? " •" : "");
        UpdateSharedSave();
    }

    private void SetConfigurationFeedback(InfoBarSeverity severity, bool show, bool openSettings = false)
    {
        ConfigInfoBar.Severity = severity;
        ConfigInfoBar.IsOpen = show;
        ConfigInfoBar.IsIconVisible = show && severity is InfoBarSeverity.Warning or InfoBarSeverity.Error;
        if (openSettings)
        {
            SettingsSectionBar.SelectedItem = CaptureSectionItem;
            SetSettingsVisible(true);
        }
        UpdateSettingsButton();
    }

    private void SetPreviewDetail(string caption, string diagnostics)
    {
        PreviewDetailText.Text = caption;
        PreviewDiagnosticsText.Text = diagnostics;
        ToolTipService.SetToolTip(PreviewDetailText, diagnostics);
    }
}
