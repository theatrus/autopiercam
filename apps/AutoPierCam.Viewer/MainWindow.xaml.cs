using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.Graphics;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow : Window
{
    private readonly AgentPipeClient _agentClient = new();
    private readonly CancellationTokenSource _lifetime = new();
    private AgentConfigurationSnapshot? _configurationSnapshot;
    private bool _configurationNeedsRefresh = true;
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
        await RunUiOperationAsync(
            "Connecting to the local capture agent…",
            RefreshStatusAndConfigurationAsync);
    }

    private async void RefreshButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync(
            "Refreshing status and configuration…",
            RefreshStatusAndConfigurationAsync);
    }

    private async void CaptureButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Requesting an immediate capture…", CaptureAndRefreshAsync);
    }

    private async void SaveButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Validating and saving configuration…", SaveConfigurationAsync);
    }

    private async Task RefreshStatusAndConfigurationAsync(CancellationToken cancellationToken)
    {
        // A Refresh is only complete when both documents are received. Keep
        // saving disabled if status succeeds but config.get fails.
        _configurationNeedsRefresh = true;
        AgentStatus status = await _agentClient.GetStatusAsync(cancellationToken);
        ApplyStatus(status);
        AgentConfigurationSnapshot snapshot =
            await _agentClient.GetConfigurationAsync(cancellationToken);
        ApplyConfiguration(snapshot);
    }

    private async Task CaptureAndRefreshAsync(CancellationToken cancellationToken)
    {
        await _agentClient.CaptureNowAsync(cancellationToken);
        StatusText.Text = "Capture request accepted; refreshing status and configuration…";
        await RefreshStatusAndConfigurationAsync(cancellationToken);
    }

    private async Task SaveConfigurationAsync(CancellationToken cancellationToken)
    {
        AgentConfigurationSnapshot snapshot = _configurationSnapshot ??
            throw new UserInputException(
                "No configuration is loaded. Select Refresh before saving.");
        if (_configurationNeedsRefresh)
        {
            throw new UserInputException(
                "The configuration may be stale. Select Refresh before saving.");
        }

        AgentConfiguration updatedConfiguration =
            BuildConfigurationFromInputs(snapshot.Config);
        AgentConfigurationReplaceResult result =
            await _agentClient.ReplaceConfigurationAsync(
                snapshot.Revision,
                updatedConfiguration,
                cancellationToken);

        _configurationSnapshot = new AgentConfigurationSnapshot
        {
            Revision = result.Revision,
            Config = updatedConfiguration,
        };
        _configurationNeedsRefresh = false;
        ConfigInfoBar.Title = $"Configuration revision {result.Revision:N0}";
        ConfigInfoBar.Message = "Saved; a camera restart was scheduled.";
        ConfigInfoBar.Severity = InfoBarSeverity.Success;
        StatusText.Text = $"Configuration revision {result.Revision:N0} saved; camera restart scheduled.";
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
        SetControlsForOperation(inProgress: true);
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
        catch (AgentRequestException exception) when (exception.IsRevisionConflict)
        {
            ShowRevisionConflict(exception);
        }
        catch (AgentRequestException exception) when (exception.IsSavedWithoutRestart)
        {
            ShowSavedWithoutRestart(exception);
        }
        catch (AgentRequestException exception)
        {
            ShowAgentFailure(FormatAgentError(exception));
        }
        catch (AgentProtocolException exception)
        {
            _configurationNeedsRefresh = true;
            ShowAgentFailure(
                $"Protocol v1 error: {exception.Message} Restart the agent and viewer, then try Refresh.");
        }
        catch (UserInputException exception)
        {
            ShowConfigurationValidationFailure(exception.Message);
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
                SetControlsForOperation(inProgress: false);
            }
        }
    }

    private void ApplyConfiguration(AgentConfigurationSnapshot snapshot)
    {
        if (!DispatcherQueue.HasThreadAccess)
        {
            _ = DispatcherQueue.TryEnqueue(() => ApplyConfiguration(snapshot));
            return;
        }

        if (_closed)
        {
            return;
        }

        AgentConfiguration configuration = snapshot.Config;
        double maxExposureMs = configuration.Camera.MaxExposureUs / 1000.0;
        double stillIntervalSeconds = configuration.Capture.IntervalMs / 1000.0;
        MaxExposureNumberBox.Maximum = Math.Max(60_000, maxExposureMs);
        MaxGainNumberBox.Maximum = Math.Max(600, configuration.Camera.MaxGain);
        StillIntervalNumberBox.Maximum = Math.Max(86_400, stillIntervalSeconds);
        MaxExposureNumberBox.Value = maxExposureMs;
        MaxGainNumberBox.Value = configuration.Camera.MaxGain;
        StillIntervalNumberBox.Value = stillIntervalSeconds;
        UploadEnabledToggle.IsOn = configuration.Upload.Enabled;
        UploadEndpointTextBox.Text = configuration.Upload.Endpoint ?? string.Empty;
        VideoEnabledToggle.IsOn = configuration.Video.Enabled;

        _configurationSnapshot = snapshot;
        _configurationNeedsRefresh = false;
        ConfigInfoBar.Title = $"Configuration revision {snapshot.Revision:N0}";
        ConfigInfoBar.Message =
            "Loaded from the capture agent. Hidden settings are preserved when saving.";
        ConfigInfoBar.Severity = InfoBarSeverity.Informational;
    }

    private AgentConfiguration BuildConfigurationFromInputs(AgentConfiguration original)
    {
        long maxExposureUs = ReadScaledInt64(
            MaxExposureNumberBox.Value,
            1000,
            "Max exposure");
        long maxGain = ReadScaledInt64(MaxGainNumberBox.Value, 1, "Max gain");
        ulong intervalMs = ReadScaledUInt64(
            StillIntervalNumberBox.Value,
            1000,
            "Still interval");

        if (maxExposureUs < original.Camera.MinExposureUs)
        {
            throw new UserInputException(
                $"Max exposure must be at least {original.Camera.MinExposureUs / 1000.0:0.###} ms.");
        }

        if (maxGain < 0)
        {
            throw new UserInputException("Max gain cannot be negative.");
        }

        if (intervalMs == 0)
        {
            throw new UserInputException("Still interval must be greater than zero.");
        }

        string? endpoint = NormalizeOptionalText(UploadEndpointTextBox.Text);
        if (UploadEnabledToggle.IsOn)
        {
            if (endpoint is null)
            {
                throw new UserInputException(
                    "Upload endpoint is required when HTTP upload is enabled.");
            }

            if (!Uri.TryCreate(endpoint, UriKind.Absolute, out Uri? endpointUri) ||
                (endpointUri.Scheme != Uri.UriSchemeHttp &&
                 endpointUri.Scheme != Uri.UriSchemeHttps))
            {
                throw new UserInputException(
                    "Upload endpoint must be an absolute HTTP or HTTPS URL.");
            }
        }

        return original with
        {
            Camera = original.Camera with
            {
                MaxExposureUs = maxExposureUs,
                MaxGain = maxGain,
            },
            Capture = original.Capture with
            {
                IntervalMs = intervalMs,
            },
            Upload = original.Upload with
            {
                Enabled = UploadEnabledToggle.IsOn,
                Endpoint = endpoint,
            },
            Video = original.Video with
            {
                Enabled = VideoEnabledToggle.IsOn,
            },
        };
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

        _configurationNeedsRefresh = true;
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
        ConfigInfoBar.Title = "Configuration unavailable";
        ConfigInfoBar.Message =
            "Reconnect and select Refresh before editing or saving settings.";
        ConfigInfoBar.Severity = InfoBarSeverity.Warning;
    }

    private void ShowUncertain(string detail)
    {
        if (_closed)
        {
            return;
        }

        _configurationNeedsRefresh = true;
        StatusText.Text =
            $"Agent response timed out — {Compact(detail)} Select Refresh before retrying Capture now.";
        PreviewStatusText.Text = "UNKNOWN";
        ModeValueText.Text = "Unknown";
        AgentConnectionText.Text = "Rust agent: response timed out";
        AgentLastErrorText.Text =
            "The request may have completed. Refresh status before sending another capture request.";
        AgentLastErrorText.Visibility = Visibility.Visible;
        ConfigInfoBar.Title = "Refresh required";
        ConfigInfoBar.Message =
            "The timed-out request may have changed agent state. Refresh before saving configuration.";
        ConfigInfoBar.Severity = InfoBarSeverity.Warning;
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

    private void ShowRevisionConflict(AgentRequestException exception)
    {
        if (_closed)
        {
            return;
        }

        _configurationNeedsRefresh = true;
        string revision = exception.CurrentRevision is ulong currentRevision
            ? $" The agent is currently at revision {currentRevision:N0}."
            : string.Empty;
        string message =
            $"Configuration was changed by another client.{revision} Select Refresh to load it before making further edits.";
        StatusText.Text = message;
        ConfigInfoBar.Title = "Configuration revision conflict";
        ConfigInfoBar.Message = message;
        ConfigInfoBar.Severity = InfoBarSeverity.Error;
        AgentLastErrorText.Text = message;
        AgentLastErrorText.Visibility = Visibility.Visible;
    }

    private void ShowSavedWithoutRestart(AgentRequestException exception)
    {
        if (_closed)
        {
            return;
        }

        _configurationNeedsRefresh = true;
        string message =
            $"The settings file was saved, but the capture worker had already stopped. {FormatAgentError(exception)} Restart the agent and select Refresh.";
        StatusText.Text = Compact(message);
        ConfigInfoBar.Title = "Configuration saved; restart required";
        ConfigInfoBar.Message = Compact(message);
        ConfigInfoBar.Severity = InfoBarSeverity.Warning;
        AgentLastErrorText.Text = Compact(message);
        AgentLastErrorText.Visibility = Visibility.Visible;
    }

    private void ShowConfigurationValidationFailure(string detail)
    {
        if (_closed)
        {
            return;
        }

        string message = $"Settings were not saved: {Compact(detail)}";
        StatusText.Text = message;
        ConfigInfoBar.Title = "Check configuration values";
        ConfigInfoBar.Message = message;
        ConfigInfoBar.Severity = InfoBarSeverity.Error;
    }

    private void SetControlsForOperation(bool inProgress)
    {
        bool generalControlsEnabled = !inProgress && !_closed;
        RefreshButton.IsEnabled = generalControlsEnabled;
        CaptureButton.IsEnabled = generalControlsEnabled;

        bool configurationControlsEnabled =
            generalControlsEnabled &&
            _configurationSnapshot is not null &&
            !_configurationNeedsRefresh;
        MaxExposureNumberBox.IsEnabled = configurationControlsEnabled;
        MaxGainNumberBox.IsEnabled = configurationControlsEnabled;
        StillIntervalNumberBox.IsEnabled = configurationControlsEnabled;
        UploadEnabledToggle.IsEnabled = configurationControlsEnabled;
        UploadEndpointTextBox.IsEnabled = configurationControlsEnabled;
        VideoEnabledToggle.IsEnabled = configurationControlsEnabled;
        SaveButton.IsEnabled = configurationControlsEnabled;

        // Camera selection is descriptive in this viewer. Config replacement
        // preserves the agent's complete camera selector unchanged.
        CameraComboBox.IsEnabled = false;
    }

    private static long ReadScaledInt64(double value, double scale, string fieldName)
    {
        if (!double.IsFinite(value))
        {
            throw new UserInputException($"{fieldName} must be a number.");
        }

        double scaled = value * scale;
        double rounded = Math.Round(scaled, MidpointRounding.AwayFromZero);
        if (!double.IsFinite(scaled) ||
            rounded < long.MinValue ||
            rounded > long.MaxValue)
        {
            throw new UserInputException($"{fieldName} is outside the supported range.");
        }

        if (Math.Abs(scaled - rounded) > 0.000001)
        {
            throw new UserInputException(
                $"{fieldName} supports at most three decimal places.");
        }

        return (long)rounded;
    }

    private static ulong ReadScaledUInt64(double value, double scale, string fieldName)
    {
        if (!double.IsFinite(value) || value < 0)
        {
            throw new UserInputException($"{fieldName} must be a non-negative number.");
        }

        double scaled = value * scale;
        double rounded = Math.Round(scaled, MidpointRounding.AwayFromZero);
        if (!double.IsFinite(scaled) || rounded > ulong.MaxValue)
        {
            throw new UserInputException($"{fieldName} is outside the supported range.");
        }

        if (Math.Abs(scaled - rounded) > 0.000001)
        {
            throw new UserInputException(
                $"{fieldName} supports at most three decimal places.");
        }

        return (ulong)rounded;
    }

    private static string? NormalizeOptionalText(string value)
    {
        string trimmed = value.Trim();
        return trimmed.Length == 0 ? null : trimmed;
    }

    private static string FormatAgentError(AgentRequestException exception)
    {
        string message = $"{exception.Code}: {exception.Message}";
        if (exception.Details is JsonElement details)
        {
            message += $" Details: {details.GetRawText()}";
        }

        return $"The capture agent rejected the request: {Compact(message)}";
    }

    private static string Compact(string value)
    {
        string compact = string.Join(
            " ",
            value.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return compact.Length <= 500 ? compact : compact[..500] + "…";
    }

    private string PipeDisplayName => @"\\.\pipe\" + _agentClient.PipeName;

    private sealed class UserInputException : Exception
    {
        internal UserInputException(string message)
            : base(message)
        {
        }
    }

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
