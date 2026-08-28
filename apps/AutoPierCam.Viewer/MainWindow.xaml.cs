using System.Diagnostics;
using System.Text.Json;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Imaging;
using Windows.Graphics;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow : Window
{
    private static readonly TimeSpan PreviewStaleAfter = TimeSpan.FromSeconds(5);

    private readonly AgentPipeClient _agentClient = new();
    private readonly PreviewPipeClient _previewClient = new();
    private readonly CancellationTokenSource _lifetime = new();
    private readonly DispatcherQueueTimer _previewFreshnessTimer;
    private AgentConfigurationSnapshot? _configurationSnapshot;
    private AgentStatus? _latestAgentStatus;
    private Task? _previewTask;
    private string _lastPreviewDetail = "Waiting for preview stream";
    private ulong _activePreviewConnectionEpoch;
    private long _lastPreviewArrivalTimestamp;
    private bool _hasPreviewFrame;
    private bool _previewFrameError;
    private bool _configurationNeedsRefresh = true;
    private bool _initialRefreshStarted;
    private bool _operationInProgress;
    private bool _closed;

    public MainWindow()
    {
        InitializeComponent();
        Title = "AutoPierCam";
        AppWindow.Resize(new SizeInt32(1180, 760));
        _previewFreshnessTimer = DispatcherQueue.CreateTimer();
        _previewFreshnessTimer.Interval = TimeSpan.FromSeconds(1);
        _previewFreshnessTimer.IsRepeating = true;
        _previewFreshnessTimer.Tick += PreviewFreshnessTimer_Tick;
        Closed += MainWindow_Closed;
    }

    private async void RootGrid_Loaded(object sender, RoutedEventArgs e)
    {
        if (_initialRefreshStarted)
        {
            return;
        }

        _initialRefreshStarted = true;
        _previewFreshnessTimer.Start();
        _previewTask = RunPreviewLoopAsync(_lifetime.Token);
        await RunUiOperationAsync(
            "Connecting to the local capture agent…",
            RefreshStatusAndConfigurationAsync);
    }

    private async Task RunPreviewLoopAsync(CancellationToken cancellationToken)
    {
        try
        {
            await _previewClient.RunAsync(
                    ApplyPreviewFrameAsync,
                    ApplyPreviewStateAsync,
                    cancellationToken)
                .ConfigureAwait(false);
        }
        catch (Exception exception) when (!cancellationToken.IsCancellationRequested)
        {
            try
            {
                await RunOnDispatcherAsync(
                        () =>
                        {
                            ShowPreviewFrameError(
                                $"Preview client stopped unexpectedly: {Compact(exception.Message)}");
                            return Task.CompletedTask;
                        },
                        CancellationToken.None)
                    .ConfigureAwait(false);
            }
            catch
            {
                // The dispatcher may already be shutting down with the window.
            }
        }
    }

    private Task ApplyPreviewStateAsync(
        PreviewStreamState state,
        CancellationToken cancellationToken)
    {
        return RunOnDispatcherAsync(
            () =>
            {
                ApplyPreviewState(state);
                return Task.CompletedTask;
            },
            cancellationToken);
    }

    private Task ApplyPreviewFrameAsync(
        PreviewFrame frame,
        CancellationToken cancellationToken)
    {
        return RunOnDispatcherAsync(
            () => DecodeAndApplyPreviewFrameAsync(frame),
            cancellationToken);
    }

    private void ApplyPreviewState(PreviewStreamState state)
    {
        if (_closed)
        {
            return;
        }

        if (state.Phase == PreviewStreamPhase.Connecting)
        {
            if (_activePreviewConnectionEpoch != 0 &&
                state.ConnectionEpoch <= _activePreviewConnectionEpoch)
            {
                return;
            }

            _activePreviewConnectionEpoch = state.ConnectionEpoch;
            ClearPreviewImage();
            PreviewStatusText.Text = "CONNECTING";
            PreviewDetailText.Text =
                $@"Connecting to \\.\pipe\{_previewClient.PipeName}";
            return;
        }

        if (state.ConnectionEpoch != _activePreviewConnectionEpoch)
        {
            return;
        }

        switch (state.Phase)
        {
            case PreviewStreamPhase.WaitingForFrame:
                ClearPreviewImage();
                PreviewStatusText.Text = "WAITING";
                PreviewDetailText.Text = "Connected; waiting for the newest camera frame";
                break;
            case PreviewStreamPhase.Reconnecting:
                ClearPreviewImage();
                PreviewStatusText.Text = "RECONNECTING";
                string retry = state.RetryDelay is TimeSpan retryDelay
                    ? $" Retrying in {retryDelay.TotalSeconds:0.##} seconds."
                    : string.Empty;
                PreviewDetailText.Text = $"{Compact(state.Detail ?? "Preview stream disconnected.")}{retry}";
                break;
            case PreviewStreamPhase.Live:
                // A successfully decoded frame owns the LIVE presentation.
                // Do not overwrite FRAME ERROR if valid wire bytes failed decoding.
                break;
            case PreviewStreamPhase.Connecting:
            default:
                break;
        }
    }

    private async Task DecodeAndApplyPreviewFrameAsync(PreviewFrame frame)
    {
        if (_closed || frame.ConnectionEpoch != _activePreviewConnectionEpoch)
        {
            return;
        }

        try
        {
            using var jpegStream = new MemoryStream(frame.Jpeg, writable: false);
            using var randomAccessStream = jpegStream.AsRandomAccessStream();
            var bitmap = new BitmapImage();
            await bitmap.SetSourceAsync(randomAccessStream);

            if (_closed || frame.ConnectionEpoch != _activePreviewConnectionEpoch)
            {
                return;
            }

            if (bitmap.PixelWidth != frame.Metadata.Width ||
                bitmap.PixelHeight != frame.Metadata.Height)
            {
                throw new InvalidDataException(
                    $"Decoded JPEG dimensions {bitmap.PixelWidth}x{bitmap.PixelHeight} do not match metadata {frame.Metadata.Width}x{frame.Metadata.Height}.");
            }

            PreviewImage.Source = bitmap;
            PreviewImage.Visibility = Visibility.Visible;
            PreviewImage.Opacity = 1;
            PreviewPlaceholder.Visibility = Visibility.Collapsed;
            PreviewStatusText.Text = "LIVE";

            ExposureValueText.Text = FormatExposure(frame.Metadata.ExposureUs);
            GainValueText.Text = frame.Metadata.Gain is long gain ? gain.ToString("N0") : "—";
            ModeValueText.Text = FormatPreviewMode(frame.Metadata.Mode);

            _lastPreviewDetail = FormatPreviewDetail(frame.Metadata);
            PreviewDetailText.Text = _lastPreviewDetail;
            _lastPreviewArrivalTimestamp = Stopwatch.GetTimestamp();
            _hasPreviewFrame = true;
            _previewFrameError = false;
        }
        catch (Exception exception)
        {
            if (!_closed && frame.ConnectionEpoch == _activePreviewConnectionEpoch)
            {
                ShowPreviewFrameError($"Could not decode preview frame: {Compact(exception.Message)}");
            }
        }
    }

    private void PreviewFreshnessTimer_Tick(DispatcherQueueTimer sender, object args)
    {
        if (_closed || !_hasPreviewFrame || _previewFrameError)
        {
            return;
        }

        TimeSpan age = Stopwatch.GetElapsedTime(_lastPreviewArrivalTimestamp);
        if (age < PreviewStaleAfter)
        {
            return;
        }

        PreviewStatusText.Text = "STALE";
        PreviewImage.Opacity = 0.45;
        PreviewDetailText.Text =
            $"No new preview for {age.TotalSeconds:0} seconds · {_lastPreviewDetail}";
    }

    private void ShowPreviewFrameError(string detail)
    {
        if (_closed)
        {
            return;
        }

        _previewFrameError = true;
        PreviewStatusText.Text = "FRAME ERROR";
        PreviewImage.Opacity = 0.45;
        PreviewDetailText.Text = Compact(detail);
    }

    private void ClearPreviewImage()
    {
        PreviewImage.Source = null;
        PreviewImage.Visibility = Visibility.Collapsed;
        PreviewImage.Opacity = 1;
        PreviewPlaceholder.Visibility = Visibility.Visible;
        ExposureValueText.Text = "—";
        GainValueText.Text = "—";
        ModeValueText.Text = "—";
        _lastPreviewArrivalTimestamp = 0;
        _hasPreviewFrame = false;
        _previewFrameError = false;
        _lastPreviewDetail = "Waiting for preview stream";
    }

    private Task RunOnDispatcherAsync(
        Func<Task> operation,
        CancellationToken cancellationToken)
    {
        if (_closed)
        {
            return Task.CompletedTask;
        }

        if (DispatcherQueue.HasThreadAccess)
        {
            return operation();
        }

        var completion = new TaskCompletionSource<object?>(
            TaskCreationOptions.RunContinuationsAsynchronously);
        bool queued = DispatcherQueue.TryEnqueue(async () =>
        {
            if (_closed)
            {
                completion.TrySetResult(null);
                return;
            }

            if (cancellationToken.IsCancellationRequested)
            {
                completion.TrySetCanceled(cancellationToken);
                return;
            }

            try
            {
                await operation();
                completion.TrySetResult(null);
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                completion.TrySetCanceled(cancellationToken);
            }
            catch (Exception exception)
            {
                completion.TrySetException(exception);
            }
        });

        if (!queued)
        {
            completion.TrySetException(
                new InvalidOperationException("The Viewer dispatcher is no longer available."));
        }

        return completion.Task;
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
        _latestAgentStatus = null;
        _configurationNeedsRefresh = false;
        ApplyUploadActivity(null, updatedConfiguration.Upload.Enabled);
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
        ApplyUploadActivity(_latestAgentStatus?.Upload, configuration.Upload.Enabled);
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
        Uri? endpointUri = null;
        if (endpoint is not null &&
            (!Uri.TryCreate(endpoint, UriKind.Absolute, out endpointUri) ||
             (endpointUri.Scheme != Uri.UriSchemeHttp &&
              endpointUri.Scheme != Uri.UriSchemeHttps) ||
             string.IsNullOrWhiteSpace(endpointUri.Host) ||
             endpointUri.UserInfo.Length != 0 ||
             endpointUri.Fragment.Length != 0))
        {
            throw new UserInputException(
                "Upload endpoint must be an absolute HTTP or HTTPS URL without credentials or a fragment.");
        }

        if (UploadEnabledToggle.IsOn && endpointUri is null)
        {
            throw new UserInputException(
                "Upload endpoint is required when HTTP upload is enabled.");
        }

        if (!string.IsNullOrWhiteSpace(original.Upload.BearerTokenEnvironment) &&
            endpointUri is not null &&
            endpointUri.Scheme != Uri.UriSchemeHttps)
        {
            throw new UserInputException(
                "Bearer-authenticated uploads require an HTTPS endpoint.");
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
        FrameCountsText.Text =
            $"{status.FramesCaptured:N0} captured · {status.FramesSaved:N0} saved";
        LastArtifactText.Text = string.IsNullOrWhiteSpace(status.LastArtifact)
            ? "Last artifact: none"
            : $"Last artifact: {Compact(status.LastArtifact)}";
        AgentConnectionText.Text =
            $"Rust agent: connected · {PipeDisplayName}";
        _latestAgentStatus = status;
        bool? uploadEnabled = _configurationNeedsRefresh
            ? null
            : _configurationSnapshot?.Config.Upload.Enabled;
        ApplyUploadActivity(status.Upload, uploadEnabled);

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
        AgentConnectionText.Text =
            $"Rust agent: disconnected · {PipeDisplayName}";
        FrameCountsText.Text = "No live agent status available";
        LastArtifactText.Text = "Last artifact: unavailable while offline";
        AgentLastErrorText.Text = "Start or restart the local capture agent and select Refresh.";
        AgentLastErrorText.Visibility = Visibility.Visible;
        ClearUploadActivity("Unavailable", "Reconnect and refresh to load upload activity.");
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
        AgentConnectionText.Text = "Rust agent: response timed out";
        AgentLastErrorText.Text =
            "The request may have completed. Refresh status before sending another capture request.";
        AgentLastErrorText.Visibility = Visibility.Visible;
        ClearUploadActivity("Unavailable", "Refresh to confirm current upload activity.");
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
        AgentConnectionText.Text = "Rust agent: connected, request failed";
        AgentLastErrorText.Text = Compact(detail);
        AgentLastErrorText.Visibility = Visibility.Visible;
        ClearUploadActivity("Unavailable", "Refresh to reload upload activity after the request error.");
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
        ClearUploadActivity("Unavailable", "Refresh to reload upload activity after the revision conflict.");
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
        ClearUploadActivity("Unavailable", "Refresh after restarting to load upload activity.");
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
        ClearUploadActivity("Unavailable", "Refresh to reload upload activity after correcting the settings.");
    }

    private void ApplyUploadActivity(AgentUploadStatus? upload, bool? uploadEnabled)
    {
        if (upload is null)
        {
            if (uploadEnabled == false)
            {
                SetUploadActivityUnavailable(
                    "Disabled",
                    "HTTP upload is disabled in the current configuration.");
            }
            else
            {
                string detail = uploadEnabled == true
                    ? "HTTP upload is enabled, but this agent reported no activity telemetry."
                    : "No upload activity telemetry is available.";
                SetUploadActivityUnavailable("Unavailable", detail);
            }

            return;
        }

        UploadActivityStateText.Text = upload.Active switch
        {
            0 => "No active transfer",
            1 => "1 active transfer",
            _ => $"{upload.Active:N0} active transfers",
        };
        UploadActivityCountsText.Text =
            $"{upload.Pending:N0} pending · {upload.Retrying:N0} retrying";
        UploadActivityTotalsText.Text =
            $"{upload.Completed:N0} completed · {upload.PermanentlyFailed:N0} permanently failed";
        UploadActivityTotalsText.Visibility = Visibility.Visible;

        UploadLastSuccessText.Text = upload.LastSuccessUnixMs is ulong lastSuccess
            ? $"Latest success: {FormatUnixTimeMilliseconds(lastSuccess)}"
            : "Latest success: none";
        UploadLastSuccessText.Visibility = Visibility.Visible;

        string failureTime = upload.LastFailureUnixMs is ulong lastFailure
            ? FormatUnixTimeMilliseconds(lastFailure)
            : "none";
        string? lastError = string.IsNullOrWhiteSpace(upload.LastError)
            ? null
            : Compact(upload.LastError);
        UploadLastFailureText.Text = lastError is null
            ? $"Last failure: {failureTime}"
            : $"Last failure: {failureTime} · {lastError}";
        UploadLastFailureText.Visibility = Visibility.Visible;
    }

    private void ClearUploadActivity(string state, string detail)
    {
        _latestAgentStatus = null;
        SetUploadActivityUnavailable(state, detail);
    }

    private void SetUploadActivityUnavailable(string state, string detail)
    {
        UploadActivityStateText.Text = state;
        UploadActivityCountsText.Text = detail;
        UploadActivityTotalsText.Text = string.Empty;
        UploadActivityTotalsText.Visibility = Visibility.Collapsed;
        UploadLastSuccessText.Text = string.Empty;
        UploadLastSuccessText.Visibility = Visibility.Collapsed;
        UploadLastFailureText.Text = string.Empty;
        UploadLastFailureText.Visibility = Visibility.Collapsed;
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

    private static string FormatExposure(long? exposureUs)
    {
        return exposureUs switch
        {
            null => "—",
            >= 1_000_000 => $"{exposureUs.Value / 1_000_000.0:0.###} s",
            >= 1_000 => $"{exposureUs.Value / 1_000.0:0.###} ms",
            _ => $"{exposureUs.Value:N0} µs",
        };
    }

    private static string FormatPreviewMode(string mode)
    {
        return mode switch
        {
            "day" => "Day",
            "night" => "Night",
            _ => "Unknown",
        };
    }

    private static string FormatPreviewDetail(PreviewFrameMetadata metadata)
    {
        string capturedAt = FormatCaptureTime(metadata.CapturedAtUnixMs);
        return $"{capturedAt} · {metadata.Width:N0}×{metadata.Height:N0} · frame {metadata.Sequence:N0} · {metadata.DroppedFrames:N0} dropped";
    }

    private static string FormatCaptureTime(ulong capturedAtUnixMs)
    {
        return FormatUnixTimeMilliseconds(capturedAtUnixMs, "Unknown capture time");
    }

    private static string FormatUnixTimeMilliseconds(
        ulong unixTimeMilliseconds,
        string invalidValue = "Invalid timestamp")
    {
        ulong maxUnixTimeMilliseconds =
            (ulong)DateTimeOffset.MaxValue.ToUnixTimeMilliseconds();
        if (unixTimeMilliseconds > maxUnixTimeMilliseconds)
        {
            return invalidValue;
        }

        try
        {
            return DateTimeOffset
                .FromUnixTimeMilliseconds((long)unixTimeMilliseconds)
                .ToLocalTime()
                .ToString("G");
        }
        catch (ArgumentOutOfRangeException)
        {
            return invalidValue;
        }
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
        _previewFreshnessTimer.Stop();
        _lifetime.Cancel();
        try
        {
            if (_previewTask is not null)
            {
                await _previewTask.ConfigureAwait(false);
            }

            await _agentClient.DisposeAsync().ConfigureAwait(false);
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
