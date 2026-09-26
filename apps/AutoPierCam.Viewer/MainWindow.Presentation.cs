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
        SetSettingsVisible(SettingsPane.Visibility != Visibility.Visible);

    private void SetSettingsVisible(bool visible)
    {
        // Collapsing never reloads controls or discards pending changes.
        SettingsPane.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        SettingsColumn.Width = new GridLength(visible ? 400 : 0);
        WorkspaceGrid.ColumnSpacing = visible ? 20 : 0;
        UpdateSettingsButton();
        UpdateSharingPolling();
        if (!visible) SettingsButton.Focus(FocusState.Programmatic);
    }

    private void UpdateSettingsButton() => SettingsButton.Content =
        (SettingsPane.Visibility == Visibility.Visible ? "Hide settings" : "Settings") + (_hasUnsavedSettings || SharingHasEdits ? " •" : "");

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
