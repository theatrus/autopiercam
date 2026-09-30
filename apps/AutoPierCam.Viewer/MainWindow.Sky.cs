using AutoPierCam.Preview;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.Storage.Pickers;

namespace AutoPierCam.Viewer;

public sealed partial class MainWindow
{
    private readonly string _skyOptionsPath = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "AutoPierCam", "viewer-sky.json");
    private SkyModelOptions _skyOptions = new();
    private ExperimentalSkyModel? _skyModel;
    private Task? _skyTask;
    private SkyEstimate? _skyEstimate;
    private string? _skyError;
    private long _skyGeneration;
    private long _skyLastStarted;
    private DateTimeOffset _skyCapturedAt;

    private void InitializeSkyModel()
    {
        try
        {
            _skyOptions = SkyModelOptions.Load(_skyOptionsPath);
            _skyOptions.Validate();
            if (_skyOptions.Enabled) _skyModel = new(_skyOptions);
        }
        catch (Exception error) { _skyError = Compact(error.Message); }
        UpdateSkyPresentation();
    }

    private void InvalidateSkyEstimate()
    {
        _skyGeneration++;
        _skyEstimate = null;
        UpdateSkyPresentation();
    }

    private void TryAnalyzeSky(PreviewFrame frame)
    {
        if (_skyModel is null || _skyError is not null || _skyTask is { IsCompleted: false }) return;
        if (frame.Metadata.CapturedAtUnixMs > 253402300799999UL) return;
        var captured = DateTimeOffset.FromUnixTimeMilliseconds((long)frame.Metadata.CapturedAtUnixMs);
        if (DateTimeOffset.UtcNow - captured > TimeSpan.FromSeconds(60) || captured > DateTimeOffset.UtcNow.AddSeconds(5)) return;
        if (_skyLastStarted != 0 && System.Diagnostics.Stopwatch.GetElapsedTime(_skyLastStarted).TotalSeconds < _skyOptions.IntervalSeconds) return;
        _skyLastStarted = System.Diagnostics.Stopwatch.GetTimestamp();
        long generation = _skyGeneration;
        ulong session = frame.Metadata.SessionGeneration;
        ulong connection = frame.ConnectionEpoch;
        ExperimentalSkyModel model = _skyModel;
        // Never await this from the preview callback. Busy frames are dropped,
        // and the next arriving frame is analyzed after the interval.
        _skyTask = Task.Run(async () =>
        {
            SkyEstimate? estimate = null;
            string? failure = null;
            try { estimate = await model.AnalyzeAsync(frame.Jpeg, _lifetime.Token).ConfigureAwait(false); }
            catch (OperationCanceledException) { return; }
            catch (Exception error) { failure = Compact(error.Message); }
            await RunOnDispatcherAsync(() =>
            {
                if (_closed || generation != _skyGeneration || connection != _activePreviewConnectionEpoch
                    || session != _lastPreviewSessionGeneration) return Task.CompletedTask;
                _skyEstimate = estimate;
                _skyCapturedAt = captured;
                _skyError = failure;
                UpdateSkyPresentation();
                return Task.CompletedTask;
            }, _lifetime.Token).ConfigureAwait(false);
        });
    }

    private void UpdateSkyPresentation()
    {
        if (SkyEstimateText is null || _closed) return;
        SkyEstimateText.Visibility = _skyOptions.Enabled || _skyError is not null ? Visibility.Visible : Visibility.Collapsed;
        string caption = _skyError is not null ? "Sky estimate unavailable · see model settings"
            : _skyEstimate is null ? "Sky estimate · waiting (experimental)"
            : DateTimeOffset.UtcNow - _skyCapturedAt > TimeSpan.FromSeconds(60)
                ? "Sky estimate · stale (experimental)" : _skyEstimate.Caption;
        SkyEstimateText.Text = caption;
        ToolTipService.SetToolTip(SkyEstimateText, _skyError ?? (_skyEstimate is { } estimate
            ? $"{estimate.ModelId} · score {estimate.Score:P0} (uncalibrated) · frame {_skyCapturedAt.LocalDateTime:T}. Display only; assumes visible sky."
            : "Display only. Uses preview frames; no alerts or camera changes."));
    }

    private async void SkyModelSettings_Click(object sender, RoutedEventArgs e)
    {
        PreviewDetailsFlyout.Hide();
        var enable = new ToggleSwitch { Header = "Experimental sky estimates", IsOn = _skyOptions.Enabled };
        var model = new TextBox { Header = "ONNX model", Text = _skyOptions.ModelPath };
        var spec = new TextBox { Header = "Model JSON", Text = _skyOptions.SpecPath };
        string installedWorker = Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "..", "autopiercam-vision.exe"));
        var worker = new TextBox { Header = "Rust worker", Text = string.IsNullOrEmpty(_skyOptions.WorkerPath) ? installedWorker : _skyOptions.WorkerPath };
        var interval = new TextBox { Header = "Analyze every (seconds)", Text = _skyOptions.IntervalSeconds.ToString(System.Globalization.CultureInfo.InvariantCulture) };
        var feedback = new TextBlock { Text = _skyError ?? "", TextWrapping = TextWrapping.Wrap };
        var fields = new StackPanel { Spacing = 10 };
        fields.Children.Add(enable);
        fields.Children.Add(new TextBlock { Text = "Display only. Unvalidated model; assumes an open roof. No alerts or camera changes.", TextWrapping = TextWrapping.Wrap });
        void FileField(TextBox field, string extension)
        {
            fields.Children.Add(field);
            var browse = new Button { Content = "Browse…" };
            browse.Click += async (_, _) =>
            {
                try
                {
                    var picker = new FileOpenPicker();
                    picker.FileTypeFilter.Add(extension);
                    WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
                    if (await picker.PickSingleFileAsync() is { } file)
                    {
                        field.Text = file.Path;
                        if (ReferenceEquals(field, model) && string.IsNullOrWhiteSpace(spec.Text)) spec.Text = Path.ChangeExtension(file.Path, ".json");
                    }
                }
                catch (Exception error) { feedback.Text = Compact(error.Message); }
            };
            fields.Children.Add(browse);
        }
        FileField(model, ".onnx");
        FileField(spec, ".json");
        FileField(worker, ".exe");
        fields.Children.Add(interval);
        fields.Children.Add(feedback);
        var dialog = new ContentDialog { XamlRoot = Content.XamlRoot, Title = "Experimental sky model",
            PrimaryButtonText = "Apply", CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Close,
            Content = new ScrollViewer { Content = fields, MaxHeight = 480 } };
        dialog.PrimaryButtonClick += (_, args) =>
        {
            try
            {
                if (!int.TryParse(interval.Text, out int seconds)) throw new InvalidDataException("Enter an integer interval in seconds.");
                var options = new SkyModelOptions(enable.IsOn, worker.Text.Trim(), model.Text.Trim(), spec.Text.Trim(), seconds);
                options.Save(_skyOptionsPath);
                _skyModel?.Dispose();
                _skyOptions = options;
                _skyModel = options.Enabled ? new(options) : null;
                _skyError = null;
                _skyLastStarted = 0;
                InvalidateSkyEstimate();
            }
            catch (Exception error) { args.Cancel = true; feedback.Text = Compact(error.Message); }
        };
        SkyModelSettingsButton.IsEnabled = false;
        try { await dialog.ShowAsync(); }
        catch (Exception error)
        {
            if (!_closed) { _skyError = Compact(error.Message); UpdateSkyPresentation(); }
        }
        finally { SkyModelSettingsButton.IsEnabled = true; }
    }
}
