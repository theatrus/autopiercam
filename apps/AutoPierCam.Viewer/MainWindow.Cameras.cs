using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private bool _cameraInventoryLoaded;

    private async Task RefreshCamerasAsync(CancellationToken cancellationToken, bool preserveSelection)
    {
        if (!preserveSelection)
        {
            _cameraInventoryLoaded = false;
            CameraComboBox.ItemsSource = null;
        }
        if (_latestAgentStatus?.HasCapability("cameras.list") != true)
        {
            _cameraInventoryLoaded = false;
            CameraComboBox.ItemsSource = null;
            CameraComboBox.PlaceholderText = "Upgrade the agent to choose a camera";
            CameraHelpText.Text = "This agent does not support camera discovery. The model filter below remains available.";
            return;
        }
        CameraInventory inventory = await _agentClient.GetCamerasAsync(cancellationToken);
        CameraChoice? pending = preserveSelection ? CameraComboBox.SelectedItem as CameraChoice : null;
        int? selectedId = pending is not null ? pending.Id : _configurationSnapshot?.Config.Camera.CameraId;
        string? selectedFilter = pending is not null ? pending.NameFilter : _configurationSnapshot?.Config.Camera.NameContains;
        IReadOnlyList<CameraChoice> choices = CameraChoice.Create(inventory, selectedId, selectedFilter);
        CameraComboBox.ItemsSource = choices;
        CameraComboBox.SelectedItem = CameraChoice.Selected(choices, selectedId, selectedFilter);
        _cameraInventoryLoaded = true;
        string scan = inventory.ScannedAtUnixMs is { } timestamp
            ? $"Last scan: {DateTimeOffset.FromUnixTimeMilliseconds(timestamp).ToLocalTime():T}. " : "Waiting for the first scan. ";
        CameraHelpText.Text = scan + (inventory.Error is not null
            ? $"Discovery failed: {inventory.Error}. "
            : inventory.Cameras.Any(camera => camera.IsColor) ? string.Empty : "No supported color cameras detected. ") +
            "IDs may change after USB reconnects. Refresh discovery after connecting a camera (allow up to 30 seconds).";
    }

    private async void RefreshCamerasButton_Click(object sender, RoutedEventArgs args)
    {
        await RunUiOperationAsync("Refreshing detected cameras…", cancellationToken => RefreshCamerasAsync(cancellationToken, true));
    }

    private void CameraComboBox_SelectionChanged(object sender, SelectionChangedEventArgs args)
    {
        if (CameraNameFilterTextBox is not null)
        {
            MarkSettingsEdited();
            SetControlsForOperation(_operationInProgress);
        }
    }
}
