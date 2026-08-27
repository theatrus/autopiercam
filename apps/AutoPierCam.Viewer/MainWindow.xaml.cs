using Microsoft.UI.Xaml;
using Windows.Graphics;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow : Window
{
    private readonly AgentPipeClient _agentClient = new();
    private readonly CancellationTokenSource _lifetime = new();
    private bool _initialRefreshStarted;
    private bool _operationInProgress;
    private bool _closed;

    public MainWindow()
    {
        InitializeComponent();
        Title = "AutoPierCam";
        AppWindow.Resize(new SizeInt32(1180, 760));
        Closed += MainWindow_Closed;
    }

    private async void RootGrid_Loaded(object sender, RoutedEventArgs e)
    {
        if (_initialRefreshStarted)
        {
            return;
        }

        _initialRefreshStarted = true;
        await RunUiOperationAsync("Connecting to the local capture agent…", RefreshStatusAsync);
    }

    private async void RefreshButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Refreshing capture status…", RefreshStatusAsync);
    }

    private async void CaptureButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Requesting an immediate capture…", CaptureAndRefreshAsync);
    }

    private void SaveButton_Click(object sender, RoutedEventArgs e)
    {
        StatusText.Text = "Settings are read-only; no config.replace request was sent.";
    }

    private async Task RefreshStatusAsync(CancellationToken cancellationToken)
    {
        AgentStatus status = await _agentClient.GetStatusAsync(cancellationToken);
        ApplyStatus(status);
    }

    private async Task CaptureAndRefreshAsync(CancellationToken cancellationToken)
    {
        await _agentClient.CaptureNowAsync(cancellationToken);
        StatusText.Text = "Capture request accepted; refreshing status…";
        AgentStatus status = await _agentClient.GetStatusAsync(cancellationToken);
        ApplyStatus(status);
    }

    private async Task RunUiOperationAsync(
        string workingMessage,
        Func<CancellationToken, Task> operation)
    {
        if (_operationInProgress || _closed)
        {
            return;
        }

        _operationInProgress = true;
        RefreshButton.IsEnabled = false;
        CaptureButton.IsEnabled = false;
        StatusText.Text = workingMessage;

        try
        {
            await operation(_lifetime.Token);
        }
        catch (OperationCanceledException) when (_lifetime.IsCancellationRequested)
        {
            // Window shutdown cancels pending pipe I/O; there is no UI left to update.
        }
        catch (AgentUnavailableException exception)
        {
            ShowOffline(exception.Message);
        }
        catch (AgentTimeoutException exception)
        {
            ShowUncertain(exception.Message);
        }
        catch (AgentRequestException exception)
        {
            ShowAgentFailure($"The capture agent rejected the request: {exception.Message}");
        }
        catch (AgentProtocolException exception)
        {
            ShowAgentFailure(
                $"Protocol v1 error: {exception.Message} Restart the agent and viewer, then try Refresh.");
        }
        catch (Exception exception)
        {
            ShowOffline($"Unexpected client error: {Compact(exception.Message)}");
        }
        finally
        {
            _operationInProgress = false;
            if (!_closed)
            {
                RefreshButton.IsEnabled = true;
                CaptureButton.IsEnabled = true;
            }
        }
    }

    private void ApplyStatus(AgentStatus status)
    {
        if (!DispatcherQueue.HasThreadAccess)
        {
            _ = DispatcherQueue.TryEnqueue(() => ApplyStatus(status));
            return;
        }

        if (_closed)
        {
            return;
        }

        string state = Compact(status.State);
        string cameraSummary = status.Camera is null
            ? "no camera"
            : $"{Compact(status.Camera.Name)} (id {status.Camera.Id})";

        StatusText.Text = $"{state} · {cameraSummary}";
        PreviewStatusText.Text = state.ToUpperInvariant();
        ModeValueText.Text = state;
        FrameCountsText.Text =
            $"{status.FramesCaptured:N0} captured · {status.FramesSaved:N0} saved";
        LastArtifactText.Text = string.IsNullOrWhiteSpace(status.LastArtifact)
            ? "Last artifact: none"
            : $"Last artifact: {Compact(status.LastArtifact)}";
        AgentConnectionText.Text =
            $"Rust agent: connected · {PipeDisplayName}";

        CameraComboBox.Items.Clear();
        if (status.Camera is null)
        {
            CameraComboBox.SelectedIndex = -1;
            CameraComboBox.PlaceholderText = "No camera reported by agent";
        }
        else
        {
            CameraComboBox.Items.Add($"{status.Camera.Id}: {Compact(status.Camera.Name)}");
            CameraComboBox.SelectedIndex = 0;
        }

        if (string.IsNullOrWhiteSpace(status.LastError))
        {
            AgentLastErrorText.Text = string.Empty;
            AgentLastErrorText.Visibility = Visibility.Collapsed;
        }
        else
        {
            AgentLastErrorText.Text = $"Last error: {Compact(status.LastError)}";
            AgentLastErrorText.Visibility = Visibility.Visible;
        }
    }

    private void ShowOffline(string detail)
    {
        if (!DispatcherQueue.HasThreadAccess)
        {
            _ = DispatcherQueue.TryEnqueue(() => ShowOffline(detail));
            return;
        }

        if (_closed)
        {
            return;
        }

        StatusText.Text =
            $"Offline — {Compact(detail)} Start the AutoPierCam agent, then select Refresh.";
        PreviewStatusText.Text = "OFFLINE";
        ModeValueText.Text = "Offline";
        AgentConnectionText.Text =
            $"Rust agent: disconnected · {PipeDisplayName}";
        FrameCountsText.Text = "No live agent status available";
        LastArtifactText.Text = "Last artifact: unavailable while offline";
        AgentLastErrorText.Text = "Start or restart the local capture agent and select Refresh.";
        AgentLastErrorText.Visibility = Visibility.Visible;
        CameraComboBox.Items.Clear();
        CameraComboBox.SelectedIndex = -1;
        CameraComboBox.PlaceholderText = "Agent offline";
    }

    private void ShowUncertain(string detail)
    {
        if (_closed)
        {
            return;
        }

        StatusText.Text =
            $"Agent response timed out — {Compact(detail)} Select Refresh before retrying Capture now.";
        PreviewStatusText.Text = "UNKNOWN";
        ModeValueText.Text = "Unknown";
        AgentConnectionText.Text = "Rust agent: response timed out";
        AgentLastErrorText.Text =
            "The request may have completed. Refresh status before sending another capture request.";
        AgentLastErrorText.Visibility = Visibility.Visible;
    }

    private void ShowAgentFailure(string detail)
    {
        if (_closed)
        {
            return;
        }

        StatusText.Text = Compact(detail);
        PreviewStatusText.Text = "ERROR";
        ModeValueText.Text = "Error";
        AgentConnectionText.Text = "Rust agent: connected, request failed";
        AgentLastErrorText.Text = Compact(detail);
        AgentLastErrorText.Visibility = Visibility.Visible;
    }

    private static string Compact(string value)
    {
        string compact = string.Join(
            " ",
            value.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return compact.Length <= 500 ? compact : compact[..500] + "…";
    }

    private string PipeDisplayName => @"\\.\pipe\" + _agentClient.PipeName;

    private async void MainWindow_Closed(object sender, WindowEventArgs args)
    {
        _closed = true;
        _lifetime.Cancel();
        try
        {
            await _agentClient.DisposeAsync();
        }
        catch
        {
            // Closing must never surface an asynchronous cleanup failure.
        }
        finally
        {
            _lifetime.Dispose();
        }
    }
}
