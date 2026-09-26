using System.Text.Json;
using AutoPierCam.Preview;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media.Imaging;
using Windows.Graphics;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow : Window
{
    private static readonly string[] ManagedOutboxStates =
        ["permanently_failed", "retrying", "pending"];
    private const double BytesPerMebibyte = 1024d * 1024d;
    private const ushort OutboxPageSize = 100;

    private readonly AgentPipeClient _agentClient = new();
    private readonly PreviewPipeClient _previewClient = new();
    private readonly PreviewFrameClock _previewFrameClock = new();
    private readonly CancellationTokenSource _lifetime = new();
    private readonly DispatcherQueueTimer _previewFreshnessTimer;
    private AgentConfigurationSnapshot? _configurationSnapshot;
    private AgentStatus? _latestAgentStatus;
    private Task? _previewTask;
    private Task? _progressTask;
    private ExposureProgressObservation? _progressObservation;
    private PreviewStreamPhase _previewPhase = PreviewStreamPhase.Connecting;
    private string _lastPreviewDetail = "Waiting for preview stream";
    private ulong _activePreviewConnectionEpoch;
    private long? _lastPreviewExposureUs;
    private ulong _lastPreviewSessionGeneration;
    private bool _hasPreviewFrame;
    private bool _previewFrameError;
    private bool _configurationNeedsRefresh = true;
    private bool _initialRefreshStarted;
    private bool _operationInProgress;
    private int _operationGeneration;
    private bool _liveStatusUnavailable = true;
    private bool _closed;
    private bool _captureNeedsReview;

    public MainWindow()
    {
        InitializeComponent();
        TrackSettingsEdits();
        InitializeSharingSection();
        Title = "AutoPierCam";
        AppWindow.SetIcon(Path.Combine(AppContext.BaseDirectory, "autopiercam.ico"));
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
        _progressTask = RunExposureProgressLoopAsync(_lifetime.Token);
        await RunUiOperationAsync(
            "Connecting to the local capture agent…",
            RefreshStatusAndConfigurationAsync);
    }

    private async Task RunExposureProgressLoopAsync(CancellationToken cancellationToken)
    {
        // A separate, short-deadline status client must not queue behind saves
        // or pairing. Poll status only, never config.get or camera enumeration.
        await using var client = new AgentPipeClient(connectTimeout: TimeSpan.FromSeconds(2), requestTimeout: TimeSpan.FromSeconds(2));
        try
        {
            while (!cancellationToken.IsCancellationRequested)
            {
                int generation = Volatile.Read(ref _operationGeneration);
                AgentStatus? status = null;
                try { status = await client.GetStatusAsync(cancellationToken).ConfigureAwait(false); }
                catch (AgentClientException) { /* A later poll can recover. */ }
                SharingStatus? sharing = null;
                if (_sharingPollWanted)
                {
                    try { sharing = await client.GetSharingAsync(cancellationToken).ConfigureAwait(false); }
                    catch (AgentClientException) { /* A later poll can recover. */ }
                }
                long received = System.Diagnostics.Stopwatch.GetTimestamp();
                await RunOnDispatcherAsync(() => {
                    // Never let a response begun before a save/command undo its
                    // newer UI state. Keep progress fresh during modal dialogs.
                    if (generation != Volatile.Read(ref _operationGeneration)) return Task.CompletedTask;
                    _progressObservation = status?.Progress is { } progress
                        ? new ExposureProgressObservation(progress, received) : null;
                    if (!_operationInProgress)
                    {
                        if (status is not null) ApplyStatus(status);
                        if (sharing is not null) ApplyPolledSharing(sharing);
                        else
                        {
                            _liveStatusUnavailable = true;
                            StatusWarningIcon.Visibility = Visibility.Visible;
                            StatusText.Text = "Agent status unavailable · reconnecting";
                            SetControlsForOperation(false);
                        }
                    }
                    UpdatePreviewPresentation();
                    return Task.CompletedTask;
                }, cancellationToken).ConfigureAwait(false);
                await Task.Delay(TimeSpan.FromSeconds(2), cancellationToken).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
            // Closing the window cancels both the request and the polling delay.
        }
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
            _previewPhase = state.Phase;
            _progressObservation = null;
            ClearPreviewImage();
            PreviewStatusText.Text = "CONNECTING";
            SetPreviewDetail("Connecting to preview…", $@"Connecting to \\.\pipe\{_previewClient.PipeName}");
            return;
        }

        if (state.ConnectionEpoch != _activePreviewConnectionEpoch)
        {
            return;
        }

        _previewPhase = state.Phase;
        switch (state.Phase)
        {
            case PreviewStreamPhase.WaitingForFrame:
                ClearPreviewImage();
                PreviewStatusText.Text = "WAITING";
                SetPreviewDetail("Waiting for first frame", "Connected; waiting for the newest camera frame");
                UpdatePreviewPresentation();
                break;
            case PreviewStreamPhase.Reconnecting:
                ClearPreviewImage();
                _progressObservation = null;
                PreviewStatusText.Text = "RECONNECTING";
                string retry = state.RetryDelay is TimeSpan retryDelay
                    ? $" Retrying in {retryDelay.TotalSeconds:0.##} seconds."
                    : string.Empty;
                SetPreviewDetail("Preview disconnected · retrying…", $"{Compact(state.Detail ?? "Preview stream disconnected.")}{retry}");
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

            _lastPreviewGain = frame.Metadata.Gain;
            _lastPreviewMode = frame.Metadata.Mode;

            _lastPreviewDetail = FormatPreviewDetail(frame.Metadata);
            _previewDimensions = $"{frame.Metadata.Width}×{frame.Metadata.Height}";
            _previewFrameClock.RecordFrame(
                frame.Metadata.CapturedAtUnixMs,
                frame.Metadata.SessionGeneration,
                frame.Metadata.Sequence);
            _lastPreviewExposureUs = frame.Metadata.ExposureUs;
            _lastPreviewSessionGeneration = frame.Metadata.SessionGeneration;
            _hasPreviewFrame = true;
            _previewFrameError = false;
            UpdatePreviewPresentation();
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
        UpdatePreviewPresentation();
    }

    private void UpdatePreviewPresentation()
    {
        if (_closed)
        {
            return;
        }

        TimeSpan observationAge = _progressObservation?.Age ?? TimeSpan.MaxValue;
        string? progressDetail = ExposurePresentation.Describe(
            _progressObservation, observationAge);
        ExposureProgressText.Text = progressDetail ??
            (_progressObservation is { Status.Exposure: null } &&
             ExposurePresentation.IsCurrent(_progressObservation, observationAge)
                ? "Exposure progress is not available with this capture agent."
                : "Waiting for camera exposure status");
        if (ExposurePresentation.IsCurrent(_progressObservation, observationAge) &&
            _progressObservation?.Status.Exposure is { } currentExposure)
        {
            ExposureProgressText.Text +=
                $" Exposure limit: {FormatExposure(currentExposure.MaxExposureUs)} · gain {currentExposure.Gain:N0}.";
        }

        // A status response cannot repair a broken preview transport or JPEG.
        if (_previewFrameError ||
            _previewPhase is PreviewStreamPhase.Connecting or PreviewStreamPhase.Reconnecting)
        {
            return;
        }

        bool currentStatus = ExposurePresentation.IsCurrent(_progressObservation, observationAge);
        ExposureProgressStatus? status = currentStatus ? _progressObservation?.Status : null;
        bool captureStopped = status is { IsActive: false };
        ExposureProgress? exposure = status?.Exposure;
        bool sameSession = !_hasPreviewFrame ||
            exposure?.SessionGeneration == _lastPreviewSessionGeneration;
        if (exposure is not null && sameSession)
        {
            SetCaptureSummary(exposure.ExposureUs, exposure.Gain, _lastPreviewMode);
        }
        else
        {
            SetCaptureSummary(_lastPreviewExposureUs, _lastPreviewGain, _lastPreviewMode);
        }

        if (!_hasPreviewFrame)
        {
            bool frameOverdue = exposure is not null &&
                exposure.WaitElapsedMs / 1_000d + observationAge.TotalSeconds >=
                exposure.FrameTimeoutMs / 1_000d;
            PreviewStatusText.Text = captureStopped
                ? "CAPTURE STOPPED"
                : frameOverdue ? "WAITING"
                : exposure?.Settling == true ? "SETTLING"
                : exposure is not null ? "EXPOSING" : "WAITING";
            SetPreviewDetail(captureStopped ? "Capture stopped" : "Waiting for first frame",
                progressDetail ?? "Connected; waiting for the newest camera frame");
            return;
        }

        TimeSpan age = _previewFrameClock.Age;
        bool stale = ExposurePresentation.IsStale(
            age,
            _lastPreviewExposureUs,
            _lastPreviewSessionGeneration,
            _progressObservation,
            observationAge);
        PreviewImage.Opacity = stale ? 0.45 : 1;
        PreviewStatusText.Text = captureStopped ? "CAPTURE STOPPED"
            : stale ? "STALE"
            : exposure?.Settling == true ? "SETTLING" : "LIVE";
        string? sameSessionDetail = ExposurePresentation.Describe(
            _progressObservation, observationAge, _lastPreviewSessionGeneration);
        string diagnostics = _lastPreviewDetail + "\n" + (sameSessionDetail ?? ExposureProgressText.Text);
        if (exposure?.Settling == true && sameSession)
            diagnostics += "\nPreview is active; still recording starts after exposure stabilizes.";
        if (stale) diagnostics += $"\nLast preview is {age.TotalSeconds:0} seconds old; waiting for a new frame.";
        SetPreviewDetail(ViewerPresentation.PreviewCaption(_previewDimensions, age, captureStopped, stale), diagnostics);
    }

    private void ShowPreviewFrameError(string detail)
    {
        if (_closed)
        {
            return;
        }

        _previewFrameError = true;
        SetCaptureSummary(null, null, null);
        PreviewStatusText.Text = "FRAME ERROR";
        PreviewImage.Opacity = 0.45;
        SetPreviewDetail("Preview error · see Details", Compact(detail));
    }

    private void ClearPreviewImage()
    {
        PreviewImage.Source = null;
        PreviewImage.Visibility = Visibility.Collapsed;
        PreviewImage.Opacity = 1;
        PreviewPlaceholder.Visibility = Visibility.Visible;
        _lastPreviewGain = null;
        _lastPreviewMode = null;
        SetCaptureSummary(null, null, null);
        // Keep the frame clock across reconnects: a cached frame is still old
        // when the preview pipe sends it again on a new connection.
        _lastPreviewExposureUs = null;
        _lastPreviewSessionGeneration = 0;
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

        // If the window closes after a callback was queued, the dispatcher may
        // never run it. Cancellation still releases both background clients.
        return completion.Task.WaitAsync(cancellationToken).WaitAsync(_lifetime.Token);
    }

    private async void RefreshButton_Click(object sender, RoutedEventArgs e)
    {
        if ((_hasUnsavedSettings || _captureNeedsReview) && !await ConfirmDiscardAsync()) return;
        await RunUiOperationAsync(
            "Refreshing status and configuration…",
            RefreshStatusAndConfigurationAsync);
    }

    private async void CaptureButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Requesting a still from the next completed frame…", CaptureAndRefreshAsync);
    }

    private async void SaveButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Validating and saving configuration…", SaveConfigurationAsync);
    }

    private async void ManageOutboxButton_Click(object sender, RoutedEventArgs e)
    {
        await RunUiOperationAsync("Loading the durable upload outbox…", ManageOutboxAsync);
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
        await RefreshCamerasAsync(cancellationToken, false);
    }

    private async Task CaptureAndRefreshAsync(CancellationToken cancellationToken)
    {
        await _agentClient.CaptureNowAsync(cancellationToken);
        ApplyStatus(await _agentClient.GetStatusAsync(cancellationToken));
        StatusText.Text = "Still requested; waiting for the next completed frame. Unsaved settings were not changed.";
    }

    private async void PauseButton_Click(object sender, RoutedEventArgs args)
    {
        await RunUiOperationAsync("Updating recording state…", async cancellationToken =>
        {
            AgentStatus status = await _agentClient.GetStatusAsync(cancellationToken);
            if (status.State is not ("capturing" or "paused"))
            {
                ApplyStatus(status);
                throw new UserInputException("Wait for capture to start before pausing or resuming recording.");
            }
            bool paused = status.State != "paused";
            await _agentClient.SetPausedAsync(paused, cancellationToken);
            ApplyStatus(status with { State = paused ? "paused" : "capturing" });
            StatusText.Text = paused
                ? "Pause requested. Live preview continues; scheduled stills, video, and sharing pause."
                : "Resume requested. Recording and permitted sharing will resume.";
        });
    }

    private async Task SaveConfigurationAsync(CancellationToken cancellationToken)
    {
        AgentConfigurationSnapshot snapshot = _configurationSnapshot ??
            throw new UserInputException(
                "No configuration is loaded. Select Reload settings before saving.");
        if (_configurationNeedsRefresh)
        {
            throw new UserInputException(
                "The configuration may be stale. Select Reload settings before saving.");
        }

        AgentConfiguration updatedConfiguration =
            BuildConfigurationFromInputs(snapshot.Config);
        AgentConfigurationReplaceResult result;
        try
        {
            result = await _agentClient.ReplaceConfigurationAsync(
                snapshot.Revision,
                updatedConfiguration,
                cancellationToken);
        }
        catch (AgentRequestException exception) when (exception.IsRevisionConflict)
        {
            // Only a capture save offers Keep my edits; outbox conflicts do not.
            _captureNeedsReview = true;
            throw;
        }

        _configurationSnapshot = new AgentConfigurationSnapshot
        {
            Revision = result.Revision,
            Config = updatedConfiguration,
        };
        _hasUnsavedSettings = false;
        // Capabilities remain valid across an accepted camera restart.
        if (result.RestartScheduled && _latestAgentStatus is { } priorStatus)
            _latestAgentStatus = priorStatus with { State = "starting", Camera = null, Upload = null, Storage = null };
        ApplyConfiguration(_configurationSnapshot);
        UpdateOutboxControlAvailability();
        UpdateRetentionControlAvailability();
        _configurationNeedsRefresh = false;
        if (result.RestartScheduled)
        {
            ApplyUploadActivity(null, updatedConfiguration.Upload.Enabled);
            ClearStorageStatus();
        }
        ConfigInfoBar.Title = "Settings saved";
        ConfigInfoBar.Message = result.RestartScheduled
            ? "Camera or image format changed. Capture is restarting."
            : "Settings saved. The camera stays running; changes apply on its next control poll.";
        SetConfigurationFeedback(InfoBarSeverity.Success, true);
        StatusText.Text = result.RestartScheduled
            ? "Settings saved; camera restart requested."
            : "Settings saved; capture continues.";
    }

    private async Task ManageOutboxAsync(CancellationToken cancellationToken)
    {
        if (_latestAgentStatus is not { } status ||
            !status.HasCapability(AgentPipeClient.UploadsListCapability) ||
            !status.HasCapability(AgentPipeClient.UploadsRequeueCapability))
        {
            throw new UserInputException(
                "This capture agent does not advertise durable outbox management. Refresh after upgrading or restarting the agent.");
        }

        if (status.Upload is null)
        {
            throw new UserInputException(
                "Durable outbox management is unavailable because the upload service is not running.");
        }

        UploadListResult page = await _agentClient.ListUploadsAsync(
            ManagedOutboxStates,
            OutboxPageSize,
            cancellationToken: cancellationToken);
        string ledgerId = page.LedgerId;
        string? nextCursor = page.NextCursor;
        var jobs = page.Jobs.ToList();
        string? notice = null;
        InfoBarSeverity noticeSeverity = InfoBarSeverity.Informational;

        async Task<Exception?> TryRefreshFirstPageAsync()
        {
            // A refresh never depends on the old cursor. Clear it before I/O
            // so a failed refresh cannot leave a known-stale action enabled.
            nextCursor = null;
            try
            {
                UploadListResult refreshed = await _agentClient.ListUploadsAsync(
                    ManagedOutboxStates,
                    OutboxPageSize,
                    cancellationToken: cancellationToken);
                ledgerId = refreshed.LedgerId;
                jobs = refreshed.Jobs.ToList();
                nextCursor = refreshed.NextCursor;
                return null;
            }
            catch (Exception exception)
            {
                return exception;
            }
        }

        while (!cancellationToken.IsCancellationRequested)
        {
            var dialog = new ContentDialog
            {
                XamlRoot = Content.XamlRoot,
                Title = "Durable upload outbox",
                PrimaryButtonText = "Requeue selected",
                CloseButtonText = "Close",
                DefaultButton = ContentDialogButton.Close,
                IsPrimaryButtonEnabled = false,
            };
            var content = new StackPanel
            {
                Width = 620,
                Spacing = 10,
            };
            content.Children.Add(new TextBlock
            {
                Text = "Pending and retrying jobs are shown for context. Only permanently failed jobs whose exact artifact still verifies can be requeued.",
                TextWrapping = TextWrapping.Wrap,
            });

            var messageBar = new InfoBar
            {
                IsClosable = false,
                IsOpen = notice is not null,
                Message = notice ?? string.Empty,
                Severity = noticeSeverity,
            };
            content.Children.Add(messageBar);

            var toolbar = new StackPanel
            {
                Orientation = Orientation.Horizontal,
                Spacing = 8,
            };
            var refreshButton = new Button { Content = "Refresh" };
            var loadMoreButton = new Button { Content = "Load more" };
            var pageSummary = new TextBlock
            {
                VerticalAlignment = VerticalAlignment.Center,
            };
            toolbar.Children.Add(refreshButton);
            toolbar.Children.Add(loadMoreButton);
            toolbar.Children.Add(pageSummary);
            content.Children.Add(toolbar);

            var jobList = new ListView
            {
                MaxHeight = 420,
                SelectionMode = ListViewSelectionMode.Single,
                HorizontalContentAlignment = HorizontalAlignment.Stretch,
            };
            content.Children.Add(jobList);
            dialog.Content = content;

            bool loading = false;

            void UpdateDialogActions()
            {
                loadMoreButton.Visibility = nextCursor is null
                    ? Visibility.Collapsed
                    : Visibility.Visible;
                loadMoreButton.IsEnabled = !loading && nextCursor is not null;
                refreshButton.IsEnabled = !loading;
                jobList.IsEnabled = !loading;
                dialog.IsPrimaryButtonEnabled =
                    !loading &&
                    jobList.SelectedItem is ListViewItem
                    {
                        Tag: UploadJobSummary { RequeueEligible: true },
                    };
            }

            void RenderJobs()
            {
                jobList.Items.Clear();
                foreach (UploadJobSummary job in jobs)
                {
                    jobList.Items.Add(new ListViewItem
                    {
                        Content = CreateUploadJobRow(job),
                        HorizontalContentAlignment = HorizontalAlignment.Stretch,
                        IsEnabled = job.RequeueEligible,
                        Tag = job,
                    });
                }

                pageSummary.Text = jobs.Count switch
                {
                    0 => "No pending, retrying, or permanently failed jobs",
                    1 => "1 job loaded",
                    _ => $"{jobs.Count:N0} jobs loaded",
                };
                UpdateDialogActions();
            }

            void ShowInlineError(Exception exception)
            {
                messageBar.Message = FormatOutboxError(exception);
                messageBar.Severity = InfoBarSeverity.Error;
                messageBar.IsOpen = true;
            }

            jobList.SelectionChanged += (_, _) => UpdateDialogActions();
            refreshButton.Click += async (_, _) =>
            {
                if (loading)
                {
                    return;
                }

                loading = true;
                RenderJobs();
                try
                {
                    UploadListResult refreshed = await _agentClient.ListUploadsAsync(
                        ManagedOutboxStates,
                        OutboxPageSize,
                        cancellationToken: cancellationToken);
                    ledgerId = refreshed.LedgerId;
                    jobs = refreshed.Jobs.ToList();
                    nextCursor = refreshed.NextCursor;
                    messageBar.IsOpen = false;
                }
                catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
                {
                    dialog.Hide();
                }
                catch (Exception exception)
                {
                    ShowInlineError(exception);
                }
                finally
                {
                    loading = false;
                    RenderJobs();
                }
            };
            loadMoreButton.Click += async (_, _) =>
            {
                if (loading || nextCursor is null)
                {
                    return;
                }

                loading = true;
                RenderJobs();
                try
                {
                    UploadListResult continuation = await _agentClient.ListUploadsAsync(
                        ManagedOutboxStates,
                        OutboxPageSize,
                        nextCursor,
                        cancellationToken);
                    if (!string.Equals(ledgerId, continuation.LedgerId, StringComparison.Ordinal))
                    {
                        throw new AgentProtocolException(
                            "uploads.list changed ledger identifiers within one paginated result.");
                    }

                    var knownJobIds = jobs.Select(job => job.JobId).ToHashSet();
                    jobs.AddRange(
                        continuation.Jobs.Where(job => knownJobIds.Add(job.JobId)));
                    nextCursor = continuation.NextCursor;
                    messageBar.IsOpen = false;
                }
                catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
                {
                    dialog.Hide();
                }
                catch (AgentRequestException exception) when (exception.IsStaleUploadCursor)
                {
                    // Every ledger mutation invalidates pagination. Never keep
                    // offering a cursor the agent has already rejected.
                    Exception? refreshFailure = await TryRefreshFirstPageAsync();
                    if (refreshFailure is null)
                    {
                        messageBar.Message =
                            "The outbox changed while pages were loading. Refreshed from the newest jobs.";
                        messageBar.Severity = InfoBarSeverity.Warning;
                        messageBar.IsOpen = true;
                    }
                    else if (refreshFailure is OperationCanceledException &&
                             cancellationToken.IsCancellationRequested)
                    {
                        dialog.Hide();
                    }
                    else
                    {
                        ShowInlineError(refreshFailure);
                    }
                }
                catch (Exception exception)
                {
                    ShowInlineError(exception);
                }
                finally
                {
                    loading = false;
                    RenderJobs();
                }
            };

            RenderJobs();
            ContentDialogResult action = await dialog.ShowAsync();
            if (action is not ContentDialogResult.Primary ||
                jobList.SelectedItem is not ListViewItem { Tag: UploadJobSummary selectedJob })
            {
                break;
            }

            var confirmation = new ContentDialog
            {
                XamlRoot = Content.XamlRoot,
                Title = "Requeue this upload?",
                Content = new TextBlock
                {
                    Text =
                        $"{selectedJob.Filename}\n\nThe agent will verify the exact file size, digest, ledger identity, delivery binding, state, and revision before making it pending again.",
                    TextWrapping = TextWrapping.Wrap,
                },
                PrimaryButtonText = "Requeue",
                CloseButtonText = "Cancel",
                DefaultButton = ContentDialogButton.Close,
            };
            if (await confirmation.ShowAsync() is not ContentDialogResult.Primary)
            {
                notice = null;
                continue;
            }

            UploadRequeueResult requeue;
            try
            {
                requeue = await _agentClient.RequeueUploadAsync(
                    ledgerId,
                    selectedJob.JobId,
                    selectedJob.JobRevision,
                    cancellationToken);
            }
            catch (AgentRequestException exception)
            {
                notice = FormatAgentError(exception);
                noticeSeverity = InfoBarSeverity.Error;
                Exception? refreshFailure = await TryRefreshFirstPageAsync();
                cancellationToken.ThrowIfCancellationRequested();
                if (refreshFailure is not null)
                {
                    // The rejection is still the primary outcome. Drop rows
                    // whose revisions could now be stale and preserve both
                    // messages for the operator.
                    jobs.Clear();
                    notice +=
                        $" The outbox list also could not refresh: {FormatOutboxError(refreshFailure)}";
                }

                continue;
            }

            notice = requeue.WorkerNotified
                ? $"{requeue.Job.Filename} was safely requeued and the uploader was notified."
                : $"{requeue.Job.Filename} was safely requeued. The durable job will resume when the upload worker is available.";
            noticeSeverity = requeue.WorkerNotified
                ? InfoBarSeverity.Success
                : InfoBarSeverity.Warning;

            // The accepted mutation invalidates every old cursor. Keep the
            // confirmed job as a safe non-actionable fallback if refreshing
            // the rest of the list fails.
            jobs = [requeue.Job];
            Exception? postRequeueRefreshFailure = await TryRefreshFirstPageAsync();
            cancellationToken.ThrowIfCancellationRequested();
            if (postRequeueRefreshFailure is not null)
            {
                jobs = [requeue.Job];
                notice +=
                    $" The durable requeue succeeded, but the list could not refresh: {FormatOutboxError(postRequeueRefreshFailure)}";
                noticeSeverity = InfoBarSeverity.Warning;
            }
        }

        if (!cancellationToken.IsCancellationRequested)
        {
            ApplyStatus(await _agentClient.GetStatusAsync(cancellationToken));
        }
    }

    private static FrameworkElement CreateUploadJobRow(UploadJobSummary job)
    {
        var row = new StackPanel
        {
            Padding = new Thickness(2, 8, 2, 8),
            Spacing = 3,
        };
        row.Children.Add(new TextBlock
        {
            Text = job.Filename,
            FontWeight = Microsoft.UI.Text.FontWeights.SemiBold,
            TextTrimming = TextTrimming.CharacterEllipsis,
            TextWrapping = TextWrapping.NoWrap,
        });

        string retry = job.NextAttemptAtUnixMs is ulong nextAttempt
            ? $" · next {FormatUnixTimeMilliseconds(nextAttempt)}"
            : string.Empty;
        row.Children.Add(new TextBlock
        {
            Text =
                $"{FormatUploadState(job.State)} · {FormatByteSize(job.FileSizeBytes)} · {job.AttemptCount:N0} attempts · {job.RequeueCount:N0} requeues{retry}",
            FontSize = 12,
            TextWrapping = TextWrapping.Wrap,
        });

        string updated = FormatUnixTimeMilliseconds(job.UpdatedAtUnixMs);
        string eligibility = job switch
        {
            { RequeueEligible: true } => "Select this row to requeue",
            { State: "permanently_failed" } => "Not eligible for requeue",
            _ => "Managed automatically by the uploader",
        };
        row.Children.Add(new TextBlock
        {
            Text = $"Updated {updated} · {eligibility}",
            FontSize = 12,
            Opacity = 0.72,
            TextWrapping = TextWrapping.Wrap,
        });

        if (!string.IsNullOrWhiteSpace(job.LastError))
        {
            string status = job.LastHttpStatus is ushort httpStatus
                ? $"HTTP {httpStatus}: "
                : string.Empty;
            row.Children.Add(new TextBlock
            {
                Text = status + Compact(job.LastError),
                FontSize = 12,
                Opacity = 0.72,
                TextWrapping = TextWrapping.Wrap,
            });
        }

        return row;
    }

    private static string FormatOutboxError(Exception exception)
    {
        return exception switch
        {
            AgentRequestException request => FormatAgentError(request),
            AgentTimeoutException timeout =>
                $"The request timed out and may still complete. Close the outbox and refresh before retrying: {Compact(timeout.Message)}",
            AgentClientException client => Compact(client.Message),
            _ => $"Could not load the upload outbox: {Compact(exception.Message)}",
        };
    }

    private static string FormatUploadState(string state)
    {
        return state switch
        {
            "pending" => "Pending",
            "in_progress" => "In progress",
            "retrying" => "Retrying",
            "completed" => "Completed",
            "permanently_failed" => "Permanently failed",
            _ => Compact(state),
        };
    }

    private static string FormatByteSize(ulong bytes)
    {
        const double kib = 1024d;
        const double mib = kib * 1024d;
        const double gib = mib * 1024d;
        return bytes switch
        {
            >= 1024UL * 1024UL * 1024UL => $"{bytes / gib:0.##} GiB",
            >= 1024UL * 1024UL => $"{bytes / mib:0.##} MiB",
            >= 1024UL => $"{bytes / kib:0.##} KiB",
            _ => $"{bytes:N0} bytes",
        };
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
        Interlocked.Increment(ref _operationGeneration);
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
        catch (AgentRequestException exception) when (exception.Code == "invalid_config")
        {
            ShowConfigurationValidationFailure(FormatAgentError(exception));
        }
        catch (AgentRequestException exception)
        {
            ShowAgentFailure(FormatAgentError(exception));
        }
        catch (AgentProtocolException exception)
        {
            _configurationNeedsRefresh = true;
            ShowAgentFailure(
                $"Protocol v1 error: {exception.Message} Restart the agent and viewer, then try Reload settings in Settings.");
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
            Interlocked.Increment(ref _operationGeneration);
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
        AdaptiveExposureToggle.IsOn = configuration.Camera.ExposureControl == "adaptive";
        Raw16Toggle.IsOn = configuration.Camera.Raw16 == true;
        CameraNameFilterTextBox.Text = configuration.Camera.NameContains ?? string.Empty;
        double maxExposureMs = configuration.Camera.MaxExposureUs / 1000.0;
        double stillIntervalSeconds = configuration.Capture.IntervalMs / 1000.0;
        MaxExposureNumberBox.Maximum = Math.Max(AdaptiveExposureToggle.IsOn ? 2_000_000 : 60_000, maxExposureMs);
        MaxGainNumberBox.Maximum = Math.Max(600, configuration.Camera.MaxGain);
        StillIntervalNumberBox.Maximum = Math.Max(86_400, stillIntervalSeconds);
        MaxExposureNumberBox.Value = maxExposureMs;
        MaxGainNumberBox.Value = configuration.Camera.MaxGain;
        StillIntervalNumberBox.Value = stillIntervalSeconds;
        RetentionMaxMiBNumberBox.Value = configuration.Capture.RetentionMaxBytes is ulong maxBytes
            ? maxBytes / BytesPerMebibyte
            : double.NaN;
        RetentionMinFreeMiBNumberBox.Value =
            configuration.Capture.RetentionMinFreeBytes is ulong minFreeBytes
                ? minFreeBytes / BytesPerMebibyte
                : double.NaN;
        UploadEnabledToggle.IsOn = configuration.Upload.Enabled;
        UploadEndpointTextBox.Text = configuration.Upload.Endpoint ?? string.Empty;
        VideoEnabledToggle.IsOn = configuration.Video.Enabled;
        FfmpegPathTextBox.Text = configuration.Video.FfmpegPath ?? string.Empty;

        _configurationSnapshot = snapshot;
        _settingsBaseline = SettingsFormValues.FromConfiguration(configuration);
        _configurationNeedsRefresh = false;
        _captureNeedsReview = false;
        CaptureKeepEditsButton.Visibility = Visibility.Collapsed;
        ApplyUploadActivity(_latestAgentStatus?.Upload, configuration.Upload.Enabled);
        _hasUnsavedSettings = false;
        ConfigInfoBar.Title = "Settings loaded";
        ConfigInfoBar.Message =
            "Choose your settings, then save to apply them.";
        SetConfigurationFeedback(InfoBarSeverity.Informational, false);
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
        bool retentionEditingSupported =
            _latestAgentStatus?.HasCapability(
                AgentPipeClient.StorageRetentionCapability) == true;
        // A legacy agent may return an existing value without advertising
        // retention editing. Preserve that exact value, but never synthesize
        // or change one unless the explicit capability is present.
        ulong? retentionMaxBytes = retentionEditingSupported
            ? ReadOptionalMebibytes(
                RetentionMaxMiBNumberBox.Value,
                "Managed image limit")
            : original.Capture.RetentionMaxBytes;
        ulong? retentionMinFreeBytes = retentionEditingSupported
            ? ReadOptionalMebibytes(
                RetentionMinFreeMiBNumberBox.Value,
                "Minimum disk free")
            : original.Capture.RetentionMinFreeBytes;

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

        string? ffmpegPath = _latestAgentStatus?.HasCapability("video.ffmpeg") == true
            ? NormalizeOptionalText(FfmpegPathTextBox.Text) : original.Video.FfmpegPath;
        if (_latestAgentStatus?.HasCapability("video.ffmpeg") == true && VideoEnabledToggle.IsOn &&
            (ffmpegPath is null || !System.IO.Path.IsPathFullyQualified(ffmpegPath) || !System.IO.File.Exists(ffmpegPath)))
        {
            throw new UserInputException("Select an existing, fully qualified FFmpeg executable path before enabling video.");
        }

        return original with
        {
            Camera = original.Camera with
            {
                MaxExposureUs = maxExposureUs,
                MaxGain = maxGain,
                ExposureControl = _latestAgentStatus?.HasCapability("camera.adaptive_exposure") == true
                    ? AdaptiveExposureToggle.IsOn ? "adaptive" : null : original.Camera.ExposureControl,
                Raw16 = _latestAgentStatus?.HasCapability("camera.raw16") == true
                    ? Raw16Toggle.IsOn ? true : null : original.Camera.Raw16,
                NameContains = _cameraInventoryLoaded && CameraComboBox.SelectedItem is CameraChoice { Id: not null } choice
                    ? choice.NameFilter : NormalizeOptionalText(CameraNameFilterTextBox.Text),
                CameraId = _cameraInventoryLoaded && CameraComboBox.SelectedItem is CameraChoice selection
                    ? selection.Id : string.Equals(original.Camera.NameContains ?? string.Empty, CameraNameFilterTextBox.Text.Trim(), StringComparison.Ordinal)
                        ? original.Camera.CameraId : null,
            },
            Capture = original.Capture with
            {
                IntervalMs = intervalMs,
                RetentionMaxBytes = retentionMaxBytes,
                RetentionMinFreeBytes = retentionMinFreeBytes,
            },
            Upload = original.Upload with
            {
                Enabled = UploadEnabledToggle.IsOn,
                Endpoint = endpoint,
            },
            Video = original.Video with
            {
                Enabled = VideoEnabledToggle.IsOn,
                FfmpegPath = ffmpegPath,
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

        string state = Compact(status.DisplayState);
        string cameraSummary = status.Camera is null
            ? "no camera"
            : $"{Compact(status.Camera.Name)} (id {status.Camera.Id})";

        StatusText.Text = $"{state} · {cameraSummary}";
        FrameCountsText.Text =
            $"{status.FramesCaptured:N0} captured · {status.FramesSaved:N0} saved";
        LastArtifactText.Text = string.IsNullOrWhiteSpace(status.LastArtifact)
            ? "Last artifact: none"
            : $"Last artifact: {Compact(status.LastArtifact)}";
        _latestAgentStatus = status;
        _liveStatusUnavailable = false;
        StatusWarningIcon.Visibility = ViewerPresentation.HasWarning(status) ? Visibility.Visible : Visibility.Collapsed;
        ToolTipService.SetToolTip(StatusWarningIcon, status.LastError ?? "Capture or storage needs attention.");
        if (!_cameraSettingsPrompted && ViewerPresentation.NeedsCameraSelection(status))
        {
            _cameraSettingsPrompted = true;
            SetSettingsVisible(true);
        }
        if (status.State is "capturing" or "paused") _cameraSettingsPrompted = false;
        UpdateOutboxControlAvailability();
        UpdateRetentionControlAvailability();
        bool? uploadEnabled = _configurationNeedsRefresh
            ? null
            : _configurationSnapshot?.Config.Upload.Enabled;
        ApplyUploadActivity(status.Upload, uploadEnabled);
        ApplyStorageStatus(
            status.Storage,
            status.HasCapability(AgentPipeClient.StorageRetentionCapability));

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
        SetControlsForOperation(_operationInProgress);
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
            $"Offline — {Compact(detail)} Start the AutoPierCam agent, then select Reload settings in Settings.";
        FrameCountsText.Text = "No live agent status available";
        LastArtifactText.Text = "Last artifact: unavailable while offline";
        AgentLastErrorText.Text = "Start or restart the local capture agent and select Reload settings.";
        AgentLastErrorText.Visibility = Visibility.Visible;
        ClearUploadActivity("Unavailable", "Reconnect and refresh to load upload activity.");
        _cameraInventoryLoaded = false;
        CameraComboBox.ItemsSource = null;
        CameraComboBox.SelectedIndex = -1;
        CameraComboBox.PlaceholderText = "Agent offline";
        ConfigInfoBar.Title = "Configuration unavailable";
        ConfigInfoBar.Message =
            "Reconnect and select Reload settings before editing or saving settings.";
        StatusWarningIcon.Visibility = Visibility.Visible;
        SetConfigurationFeedback(InfoBarSeverity.Warning, true);
    }

    private void ShowUncertain(string detail)
    {
        if (_closed)
        {
            return;
        }

        _configurationNeedsRefresh = true;
        StatusText.Text =
            $"Agent response timed out — {Compact(detail)} Select Reload settings in Settings before retrying Save next frame.";
        AgentLastErrorText.Text =
            "The request may have completed. Refresh status before sending another capture request.";
        AgentLastErrorText.Visibility = Visibility.Visible;
        ClearUploadActivity("Unavailable", "Refresh to confirm current upload activity.");
        ConfigInfoBar.Title = "Reload settings required";
        ConfigInfoBar.Message =
            "The timed-out request may have changed agent state. Refresh before saving configuration.";
        StatusWarningIcon.Visibility = Visibility.Visible;
        SetConfigurationFeedback(InfoBarSeverity.Warning, true);
    }

    private void ShowAgentFailure(string detail)
    {
        if (_closed)
        {
            return;
        }

        StatusText.Text = Compact(detail);
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
        string message = _captureNeedsReview
            ? "Settings changed elsewhere. Discard your edits to load them, or keep your edits to replace them on the next save."
            : "Settings were changed elsewhere. Select Reload settings before saving your changes.";
        CaptureKeepEditsButton.Visibility = _captureNeedsReview ? Visibility.Visible : Visibility.Collapsed;
        StatusText.Text = message;
        ConfigInfoBar.Title = "Settings changed elsewhere";
        ConfigInfoBar.Message = message;
        SetConfigurationFeedback(InfoBarSeverity.Error, true, openSettings: true);
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
            $"The settings file was saved, but the capture worker had already stopped. {FormatAgentError(exception)} Restart the agent and select Reload settings.";
        StatusText.Text = Compact(message);
        ConfigInfoBar.Title = "Configuration saved; restart required";
        ConfigInfoBar.Message = Compact(message);
        SetConfigurationFeedback(InfoBarSeverity.Warning, true, openSettings: true);
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
        StatusText.Text = "Settings were not saved — see the message beside Save.";
        ConfigInfoBar.Title = "Check configuration values";
        ConfigInfoBar.Message = message;
        SetConfigurationFeedback(InfoBarSeverity.Error, true, openSettings: true);
        // Rejected settings do not disconnect the agent or revoke capabilities.
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

    private void ApplyStorageStatus(
        AgentStorageStatus? storage,
        bool retentionSupported)
    {
        if (storage is null)
        {
            StorageStatusCard.Visibility = retentionSupported
                ? Visibility.Visible
                : Visibility.Collapsed;
            StoragePressureText.Text = "Unavailable";
            StorageBytesText.Text = retentionSupported
                ? "Waiting for the first retention sweep."
                : string.Empty;
            StorageFreeText.Text = string.Empty;
            StorageSweepText.Text = string.Empty;
            StorageErrorText.Text = string.Empty;
            StorageErrorText.Visibility = Visibility.Collapsed;
            return;
        }

        StorageStatusCard.Visibility = Visibility.Visible;
        string pressure = storage.Pressure switch
        {
            "ok" => "Storage pressure: OK",
            "cleanup_needed" => "Storage pressure: cleanup needed",
            "blocked" => "Storage pressure: blocked",
            _ => "Storage pressure: unknown",
        };
        StoragePressureText.Text = storage.CaptureSuspended
            ? $"{pressure} · scheduled stills suspended"
            : pressure;
        StorageBytesText.Text =
            $"{FormatByteSize(storage.ManagedBytes)} managed · {FormatByteSize(storage.ProtectedBytes)} protected · {FormatByteSize(storage.ReclaimableBytes)} reclaimable";
        StorageFreeText.Text = storage.FreeBytes is ulong freeBytes
            ? $"Capture volume free: {FormatByteSize(freeBytes)}"
            : "Capture volume free: unavailable";

        string reclaimed = storage.LastReclaimedFiles switch
        {
            0 => "no files reclaimed",
            1 => $"1 file / {FormatByteSize(storage.LastReclaimedBytes)} reclaimed",
            _ =>
                $"{storage.LastReclaimedFiles:N0} files / {FormatByteSize(storage.LastReclaimedBytes)} reclaimed",
        };
        StorageSweepText.Text = storage.LastSweepUnixMs is ulong lastSweep
            ? $"Latest sweep: {FormatUnixTimeMilliseconds(lastSweep)} · {reclaimed}"
            : $"No completed sweep · {reclaimed}";

        string? warning = string.IsNullOrWhiteSpace(storage.LastError)
            ? storage.CaptureSuspended
                ? "Scheduled still persistence is paused; Save next frame remains available."
                : null
            : Compact(storage.LastError);
        StorageErrorText.Text = warning ?? string.Empty;
        StorageErrorText.Visibility = warning is null
            ? Visibility.Collapsed
            : Visibility.Visible;
    }

    private void ClearStorageStatus()
    {
        StorageStatusCard.Visibility = Visibility.Collapsed;
        StoragePressureText.Text = "Unavailable";
        StorageBytesText.Text = string.Empty;
        StorageFreeText.Text = string.Empty;
        StorageSweepText.Text = string.Empty;
        StorageErrorText.Text = string.Empty;
        StorageErrorText.Visibility = Visibility.Collapsed;
    }

    private void ClearUploadActivity(string state, string detail)
    {
        _latestAgentStatus = null;
        UpdateOutboxControlAvailability();
        UpdateRetentionControlAvailability();
        ClearStorageStatus();
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
        SettingsButton.IsEnabled = generalControlsEnabled;
        CaptureButton.IsEnabled = generalControlsEnabled && !_liveStatusUnavailable;
        PauseButton.IsEnabled = generalControlsEnabled && !_liveStatusUnavailable && _latestAgentStatus?.State is "capturing" or "paused";
        PauseButton.Content = _latestAgentStatus?.State == "paused" ? "Resume recording" : "Pause recording";

        bool configurationControlsEnabled =
            generalControlsEnabled &&
            _configurationSnapshot is not null &&
            !_configurationNeedsRefresh;
        MaxExposureNumberBox.IsEnabled = configurationControlsEnabled;
        MaxGainNumberBox.IsEnabled = configurationControlsEnabled;
        AdaptiveExposureToggle.IsEnabled = configurationControlsEnabled && _latestAgentStatus?.HasCapability("camera.adaptive_exposure") == true;
        Raw16Toggle.IsEnabled = configurationControlsEnabled && _latestAgentStatus?.HasCapability("camera.raw16") == true;
        CameraNameFilterTextBox.IsEnabled = configurationControlsEnabled &&
            (!_cameraInventoryLoaded || CameraComboBox.SelectedItem is not CameraChoice { Id: not null });
        StillIntervalNumberBox.IsEnabled = configurationControlsEnabled;
        UploadEnabledToggle.IsEnabled = configurationControlsEnabled;
        UploadEndpointTextBox.IsEnabled = configurationControlsEnabled;
        VideoEnabledToggle.IsEnabled = configurationControlsEnabled && _latestAgentStatus?.HasCapability("video.ffmpeg") == true;
        FfmpegPathTextBox.IsEnabled = VideoEnabledToggle.IsEnabled;
        SaveButton.IsEnabled = configurationControlsEnabled && !_liveStatusUnavailable && _hasUnsavedSettings;
        CaptureDiscardButton.IsEnabled = generalControlsEnabled && _configurationSnapshot is not null &&
            (_hasUnsavedSettings || _captureNeedsReview);
        CaptureKeepEditsButton.IsEnabled = generalControlsEnabled && !_liveStatusUnavailable;
        UpdateOutboxControlAvailability();
        UpdateRetentionControlAvailability();

        CameraComboBox.IsEnabled = configurationControlsEnabled && _cameraInventoryLoaded;
        RefreshCamerasButton.IsEnabled = configurationControlsEnabled && _latestAgentStatus?.HasCapability("cameras.list") == true;
        UpdateSharingPolling();
        RenderSharing();
    }

    private void ExposureControl_Changed(object sender, RoutedEventArgs args)
    {
        if (MaxExposureNumberBox is not null)
        {
            MaxExposureNumberBox.Maximum = Math.Max(AdaptiveExposureToggle.IsOn ? 2_000_000 : 60_000,
                double.IsNaN(MaxExposureNumberBox.Value) ? 0 : MaxExposureNumberBox.Value);
        }
    }

    private void UpdateOutboxControlAvailability()
    {
        bool supported =
            _latestAgentStatus is { } status &&
            status.HasCapability(AgentPipeClient.UploadsListCapability) &&
            status.HasCapability(AgentPipeClient.UploadsRequeueCapability);
        bool serviceAvailable = supported && _latestAgentStatus?.Upload is not null;
        ManageOutboxButton.Visibility = supported
            ? Visibility.Visible
            : Visibility.Collapsed;
        ManageOutboxButton.IsEnabled =
            serviceAvailable && !_operationInProgress && !_closed;
        OutboxAvailabilityText.Visibility = supported && !serviceAvailable
            ? Visibility.Visible
            : Visibility.Collapsed;
        ToolTipService.SetToolTip(
            ManageOutboxButton,
            !serviceAvailable
                ? "The upload service is not running, so its durable ledger is unavailable."
                : _operationInProgress
                    ? "Wait for the current Viewer operation to finish."
                    : "Inspect and safely requeue durable upload jobs.");
    }

    private void UpdateRetentionControlAvailability()
    {
        bool settingsLoaded =
            _configurationSnapshot is not null && !_configurationNeedsRefresh;
        bool supported =
            _latestAgentStatus?.HasCapability(
                AgentPipeClient.StorageRetentionCapability) == true;
        bool editingEnabled =
            settingsLoaded && supported && !_operationInProgress && !_closed;
        RetentionMaxMiBNumberBox.IsEnabled = editingEnabled;
        RetentionMinFreeMiBNumberBox.IsEnabled = editingEnabled;

        bool showReadOnlyReason = settingsLoaded && !supported;
        RetentionSettingsAvailabilityText.Visibility = showReadOnlyReason
            ? Visibility.Visible
            : Visibility.Collapsed;
        if (showReadOnlyReason)
        {
            bool hasExistingLimit =
                _configurationSnapshot?.Config.Capture.RetentionMaxBytes is not null ||
                _configurationSnapshot?.Config.Capture.RetentionMinFreeBytes is not null;
            RetentionSettingsAvailabilityText.Text = hasExistingLimit
                ? "This agent does not advertise retention editing. Loaded limits are read-only and will be preserved unchanged."
                : "Refresh agent status to enable retention editing. Existing settings are preserved when saving.";
        }
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

    private static ulong? ReadOptionalMebibytes(double value, string fieldName)
    {
        if (double.IsNaN(value))
        {
            return null;
        }

        ulong bytes = ReadScaledUInt64(value, BytesPerMebibyte, fieldName);
        if (bytes == 0)
        {
            throw new UserInputException($"{fieldName} must be greater than zero or left blank.");
        }

        return bytes;
    }

    private static string? NormalizeOptionalText(string value)
    {
        string trimmed = value.Trim();
        return trimmed.Length == 0 ? null : trimmed;
    }

    private static string FormatExposure(long? exposureUs) => ViewerPresentation.FormatExposure(exposureUs);

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
            // Join every loop even if one encounters an error during shutdown.
            await Task.WhenAll(
                    _previewTask ?? Task.CompletedTask,
                    _progressTask ?? Task.CompletedTask)
                .ConfigureAwait(false);
        }
        catch
        {
            // Closing must never surface an asynchronous cleanup failure.
        }
        finally
        {
            try
            {
                await _agentClient.DisposeAsync().ConfigureAwait(false);
            }
            catch
            {
                // Release the lifetime even if control-client cleanup fails.
            }
            finally
            {
                _lifetime.Dispose();
            }
        }
    }
}
