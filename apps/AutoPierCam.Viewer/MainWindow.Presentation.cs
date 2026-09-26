using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private bool _cameraSettingsPrompted;
    private string _previewDimensions = "";

    private void SettingsButton_Click(object sender, RoutedEventArgs args) =>
        SetSettingsVisible(SettingsPane.Visibility != Visibility.Visible);

    private void SetSettingsVisible(bool visible)
    {
        // Collapsing never reloads controls or discards pending changes.
        SettingsPane.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        SettingsColumn.Width = new GridLength(visible ? 360 : 0);
        WorkspaceGrid.ColumnSpacing = visible ? 20 : 0;
        UpdateSettingsButton();
        if (!visible) SettingsButton.Focus(FocusState.Programmatic);
    }

    private void UpdateSettingsButton() => SettingsButton.Content =
        (SettingsPane.Visibility == Visibility.Visible ? "Hide settings" : "Settings") + (_hasUnsavedSettings ? " •" : "");

    private void SetConfigurationFeedback(InfoBarSeverity severity, bool show, bool openSettings = false)
    {
        ConfigInfoBar.Severity = severity;
        ConfigInfoBar.IsOpen = show;
        ConfigInfoBar.IsIconVisible = show && severity is InfoBarSeverity.Warning or InfoBarSeverity.Error;
        if (openSettings) SetSettingsVisible(true);
        UpdateSettingsButton();
    }

    private void SetPreviewDetail(string caption, string diagnostics)
    {
        PreviewDetailText.Text = caption;
        PreviewDiagnosticsText.Text = diagnostics;
        ToolTipService.SetToolTip(PreviewDetailText, diagnostics);
    }
}
