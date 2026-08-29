using System.ComponentModel;
using System.Diagnostics;
using System.Globalization;
using System.IO;
using System.Runtime.CompilerServices;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Threading;
using NINA.Core.Utility;

namespace AutoPierCam.NINA.Preview;

public interface IPierCameraPreviewRuntime : INotifyPropertyChanged
{
    ImageSource? Image { get; }

    bool HasImage { get; }

    bool IsLive { get; }

    bool IsStale { get; }

    double ImageOpacity { get; }

    string StatusText { get; }

    string ConnectionText { get; }

    string CapturedAtText { get; }

    string FrameAgeText { get; }

    string DimensionsText { get; }

    string ExposureText { get; }

    string GainText { get; }

    string ModeText { get; }

    string DroppedFramesText { get; }

    void Start();

    Task StopAsync();
}

public sealed class PierCameraPreviewRuntime : IPierCameraPreviewRuntime
{
    internal static readonly TimeSpan StaleAfter = TimeSpan.FromSeconds(5);

    private readonly object lifecycleLock = new();
    private readonly SemaphoreSlim stopGate = new(1, 1);
    private CancellationTokenSource? lifetime;
    private Task? clientTask;
    private Task? freshnessTask;
    private PreviewStreamPhase? streamPhase;
    private PreviewFrameMetadata? metadata;
    private long? lastFrameReceivedTimestamp;
    private string? streamDetail;
    private string? frameError;
    private TimeSpan? retryDelay;
    private ImageSource? image;

    public event PropertyChangedEventHandler? PropertyChanged;

    public ImageSource? Image
    {
        get => image;
        private set
        {
            if (!ReferenceEquals(image, value))
            {
                image = value;
                RaisePropertyChanged();
                RaisePropertyChanged(nameof(HasImage));
            }
        }
    }

    public bool HasImage => Image is not null;

    public bool IsStale => HasImage &&
        (streamPhase != PreviewStreamPhase.Live ||
         frameError is not null ||
         FrameAge() >= StaleAfter);

    public bool IsLive => HasImage && !IsStale;

    public double ImageOpacity => IsStale ? 0.45 : 1.0;

    public string StatusText
    {
        get
        {
            if (frameError is not null)
            {
                return HasImage
                    ? $"Could not display the newest frame; showing the last good snapshot. {frameError}"
                    : $"Could not display the newest pier camera frame. {frameError}";
            }

            return streamPhase switch
            {
                PreviewStreamPhase.Connecting when HasImage =>
                    "Reconnecting to AutoPierCam; showing the last snapshot.",
                PreviewStreamPhase.Connecting =>
                    "Connecting to AutoPierCam…",
                PreviewStreamPhase.WaitingForFrame when HasImage =>
                    "Connected; waiting for a new frame. The last snapshot is retained.",
                PreviewStreamPhase.WaitingForFrame =>
                    "Connected to AutoPierCam; waiting for the first pier camera frame…",
                PreviewStreamPhase.Live when IsStale =>
                    $"No new pier camera frame for {FormatAge(FrameAge())}; showing the last snapshot.",
                PreviewStreamPhase.Live =>
                    "Live pier camera preview.",
                PreviewStreamPhase.Reconnecting when HasImage =>
                    $"AutoPierCam preview was interrupted; showing the last snapshot and retrying{RetrySuffix()}. {streamDetail}",
                PreviewStreamPhase.Reconnecting =>
                    $"Waiting for AutoPierCam; retrying{RetrySuffix()}. {streamDetail}",
                _ => "Waiting for AutoPierCam to start…",
            };
        }
    }

    public string ConnectionText => streamPhase switch
    {
        PreviewStreamPhase.Live when IsStale => "Stale",
        PreviewStreamPhase.Live => "Live",
        PreviewStreamPhase.WaitingForFrame => "Waiting for frame",
        PreviewStreamPhase.Connecting => "Connecting",
        PreviewStreamPhase.Reconnecting => "Reconnecting",
        _ => "Waiting",
    };

    public string CapturedAtText => metadata is null
        ? "Not received yet"
        : metadata.CapturedAt.ToLocalTime().ToString("yyyy-MM-dd HH:mm:ss.fff zzz", CultureInfo.CurrentCulture);

    public string FrameAgeText
    {
        get
        {
            if (lastFrameReceivedTimestamp is null)
            {
                return "No frame received";
            }

            TimeSpan age = FrameAge();
            return age < TimeSpan.FromSeconds(1.5)
                ? "Received just now"
                : $"Received {FormatAge(age)} ago";
        }
    }

    public string DimensionsText => metadata is null
        ? "—"
        : $"{metadata.Width:N0} × {metadata.Height:N0}";

    public string ExposureText => metadata?.ExposureUs is long exposure
        ? FormatExposure(exposure)
        : "Unavailable";

    public string GainText => metadata?.Gain is long gain
        ? gain.ToString("N0", CultureInfo.CurrentCulture)
        : "Unavailable";

    public string ModeText => metadata?.Mode switch
    {
        "day" => "Day",
        "night" => "Night",
        "unknown" => "Automatic / unknown",
        _ => "—",
    };

    public string DroppedFramesText => metadata is null
        ? "—"
        : metadata.DroppedFrames.ToString("N0", CultureInfo.CurrentCulture);

    public void Start()
    {
        lock (lifecycleLock)
        {
            if (lifetime is not null)
            {
                return;
            }

            lifetime = new CancellationTokenSource();
            CancellationToken token = lifetime.Token;
            clientTask = Task.Run(() => SuperviseClientAsync(token), token);
            freshnessTask = Task.Run(() => MonitorFreshnessAsync(token), token);
        }
    }

    public async Task StopAsync()
    {
        await stopGate.WaitAsync().ConfigureAwait(false);
        try
        {
            CancellationTokenSource? source;
            Task[] tasks;
            lock (lifecycleLock)
            {
                source = lifetime;
                if (source is null)
                {
                    return;
                }

                source.Cancel();
                tasks = new[] { clientTask!, freshnessTask! };
            }

            try
            {
                await Task.WhenAll(tasks).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (source.IsCancellationRequested)
            {
            }
            finally
            {
                lock (lifecycleLock)
                {
                    if (ReferenceEquals(lifetime, source))
                    {
                        lifetime = null;
                        clientTask = null;
                        freshnessTask = null;
                    }
                }
                source.Dispose();
            }
        }
        finally
        {
            stopGate.Release();
        }
    }

    private async Task SuperviseClientAsync(CancellationToken cancellationToken)
    {
        while (!cancellationToken.IsCancellationRequested)
        {
            try
            {
                var client = new PreviewPipeClient();
                await client.RunAsync(HandleFrameAsync, HandleStateAsync, cancellationToken)
                    .ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                return;
            }
            catch (Exception exception)
            {
                Logger.Error(exception);
                await DispatchAsync(
                        () =>
                        {
                            streamPhase = PreviewStreamPhase.Reconnecting;
                            streamDetail =
                                $"The preview reader stopped unexpectedly: {Compact(exception.Message)}";
                            retryDelay = TimeSpan.FromSeconds(5);
                            frameError = null;
                            RefreshPresentation();
                        },
                        cancellationToken)
                    .ConfigureAwait(false);
                await Task.Delay(TimeSpan.FromSeconds(5), cancellationToken).ConfigureAwait(false);
            }
        }
    }

    private async Task MonitorFreshnessAsync(CancellationToken cancellationToken)
    {
        using var timer = new PeriodicTimer(TimeSpan.FromSeconds(1));
        try
        {
            while (await timer.WaitForNextTickAsync(cancellationToken).ConfigureAwait(false))
            {
                await DispatchAsync(RefreshPresentation, cancellationToken).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
        {
        }
    }

    private async Task HandleFrameAsync(PreviewFrame frame, CancellationToken cancellationToken)
    {
        BitmapSource? decoded = null;
        string? decodeError = null;
        try
        {
            decoded = DecodeAndValidate(frame);
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            decodeError = Compact(exception.Message);
        }

        await DispatchAsync(
                () =>
                {
                    if (decoded is not null)
                    {
                        Image = decoded;
                        metadata = frame.Metadata;
                        lastFrameReceivedTimestamp = Stopwatch.GetTimestamp();
                        frameError = null;
                    }
                    else
                    {
                        frameError = decodeError ?? "The JPEG could not be decoded.";
                    }

                    RefreshPresentation();
                },
                cancellationToken)
            .ConfigureAwait(false);
    }

    private Task HandleStateAsync(
        PreviewStreamState state,
        CancellationToken cancellationToken) =>
        DispatchAsync(
            () =>
            {
                streamPhase = state.Phase;
                streamDetail = state.Detail;
                retryDelay = state.RetryDelay;
                if (state.Phase != PreviewStreamPhase.Live)
                {
                    frameError = null;
                }
                RefreshPresentation();
            },
            cancellationToken);

    internal static BitmapSource DecodeAndValidate(PreviewFrame frame)
    {
        using var stream = new MemoryStream(frame.Jpeg, writable: false);
        BitmapDecoder decoder = BitmapDecoder.Create(
            stream,
            BitmapCreateOptions.PreservePixelFormat,
            BitmapCacheOption.OnLoad);
        if (decoder.Frames.Count != 1)
        {
            throw new PreviewProtocolException(
                $"Expected one JPEG frame, but decoded {decoder.Frames.Count:N0}.");
        }

        BitmapFrame bitmap = decoder.Frames[0];
        if ((uint)bitmap.PixelWidth != frame.Metadata.Width ||
            (uint)bitmap.PixelHeight != frame.Metadata.Height)
        {
            throw new PreviewProtocolException(
                $"Decoded JPEG dimensions {bitmap.PixelWidth:N0}x{bitmap.PixelHeight:N0} do not match metadata {frame.Metadata.Width:N0}x{frame.Metadata.Height:N0}.");
        }

        bitmap.Freeze();
        return bitmap;
    }

    private async Task DispatchAsync(Action action, CancellationToken cancellationToken)
    {
        Dispatcher? dispatcher = Application.Current?.Dispatcher;
        if (dispatcher?.HasShutdownStarted == true || dispatcher?.HasShutdownFinished == true)
        {
            return;
        }

        if (dispatcher is null || dispatcher.CheckAccess())
        {
            cancellationToken.ThrowIfCancellationRequested();
            action();
            return;
        }

        try
        {
            await dispatcher
                .InvokeAsync(action, DispatcherPriority.DataBind, cancellationToken)
                .Task
                .ConfigureAwait(false);
        }
        catch (TaskCanceledException) when (
            dispatcher.HasShutdownStarted || dispatcher.HasShutdownFinished)
        {
        }
    }

    private void RefreshPresentation()
    {
        RaisePropertyChanged(nameof(IsStale));
        RaisePropertyChanged(nameof(IsLive));
        RaisePropertyChanged(nameof(ImageOpacity));
        RaisePropertyChanged(nameof(StatusText));
        RaisePropertyChanged(nameof(ConnectionText));
        RaisePropertyChanged(nameof(CapturedAtText));
        RaisePropertyChanged(nameof(FrameAgeText));
        RaisePropertyChanged(nameof(DimensionsText));
        RaisePropertyChanged(nameof(ExposureText));
        RaisePropertyChanged(nameof(GainText));
        RaisePropertyChanged(nameof(ModeText));
        RaisePropertyChanged(nameof(DroppedFramesText));
    }

    private TimeSpan FrameAge() => lastFrameReceivedTimestamp is null
        ? TimeSpan.MaxValue
        : Stopwatch.GetElapsedTime(lastFrameReceivedTimestamp.Value);

    private string RetrySuffix() => retryDelay is TimeSpan delay
        ? $" in {delay.TotalSeconds:0.#} seconds"
        : string.Empty;

    internal static string FormatExposure(long exposureUs)
    {
        if (exposureUs >= 1_000_000)
        {
            return $"{exposureUs / 1_000_000d:0.###} s";
        }

        if (exposureUs >= 1_000)
        {
            return $"{exposureUs / 1_000d:0.###} ms";
        }

        return $"{exposureUs:N0} µs";
    }

    internal static string FormatAge(TimeSpan age)
    {
        if (age < TimeSpan.FromSeconds(1.5))
        {
            return "just now";
        }

        if (age < TimeSpan.FromMinutes(1))
        {
            return $"{Math.Floor(age.TotalSeconds):N0} seconds";
        }

        if (age < TimeSpan.FromHours(1))
        {
            return $"{Math.Floor(age.TotalMinutes):N0} minutes";
        }

        return $"{Math.Floor(age.TotalHours):N0} hours";
    }

    private static string Compact(string value)
    {
        string compact = string.Join(
            " ",
            value.Split((char[]?)null, StringSplitOptions.RemoveEmptyEntries));
        return compact.Length <= 300 ? compact : compact[..300] + "…";
    }

    private void RaisePropertyChanged([CallerMemberName] string? propertyName = null) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));
}
