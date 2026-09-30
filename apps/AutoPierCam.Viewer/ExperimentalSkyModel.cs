using System.Buffers.Binary;
using System.Diagnostics;
using System.Text;
using System.Text.Json;

namespace AutoPierCam.Viewer;

internal sealed record SkyModelOptions(bool Enabled = false, string WorkerPath = "", string ModelPath = "", string SpecPath = "", int IntervalSeconds = 10)
{
    internal void Validate()
    {
        if (IntervalSeconds is < 1 or > 3600) throw new InvalidDataException("Analysis interval must be 1–3600 seconds.");
        if (!Enabled) return;
        foreach (string path in new[] { WorkerPath, ModelPath, SpecPath })
            if (!Path.IsPathFullyQualified(path) || !File.Exists(path))
                throw new InvalidDataException("Choose existing absolute paths for the worker, ONNX model, and model JSON.");
    }

    internal static SkyModelOptions Load(string path)
    {
        if (!File.Exists(path)) return new();
        if (new FileInfo(path).Length > 16 * 1024) throw new InvalidDataException("Sky model settings are too large.");
        return JsonSerializer.Deserialize<SkyModelOptions>(File.ReadAllBytes(path))
            ?? throw new InvalidDataException("Invalid sky model settings.");
    }

    internal void Save(string path)
    {
        Validate();
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        string temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        try
        {
            File.WriteAllBytes(temporary, JsonSerializer.SerializeToUtf8Bytes(this));
            File.Move(temporary, path, overwrite: true);
        }
        finally { if (File.Exists(temporary)) File.Delete(temporary); }
    }
}

internal sealed record SkyEstimate(string? Label, double Score, string ModelId)
{
    internal string Caption => $"Sky estimate · {Label switch { "clear" => "Clear", "partly_cloudy" => "Partly cloudy", "overcast" => "Overcast", _ => "Uncertain" }} (experimental)";

    internal static SkyEstimate Parse(JsonElement root, string modelId)
    {
        if (root.TryGetProperty("error", out var error)) throw new InvalidDataException(error.GetString());
        if (root.GetProperty("task").GetString() != "sky" || root.GetProperty("model_id").GetString() != modelId)
            throw new InvalidDataException("Unexpected model response.");
        string? label = root.GetProperty("label").GetString();
        double score = root.GetProperty("confidence").GetDouble();
        var scores = root.GetProperty("probabilities").EnumerateArray().Select(v => v.GetDouble()).ToArray();
        string[] labels = ["clear", "partly_cloudy", "overcast"];
        if (scores.Length != 3 || scores.Any(v => !double.IsFinite(v) || v < 0 || v > 1)
            || !double.IsFinite(score) || Math.Abs(scores.Sum() - 1) > 0.0001
            || Math.Abs(scores.Max() - score) > 0.0001
            || (label is not null && label != labels[Array.IndexOf(scores, scores.Max())]))
            throw new InvalidDataException("Invalid model scores.");
        return new(label, score, modelId);
    }
}

// Only the supervised subprocess executes ONNX. Never runs on the dispatcher,
// acquires a camera, queues frames, or invokes an agent/Chatstronomy command.
internal sealed class ExperimentalSkyModel(SkyModelOptions options) : IDisposable
{
    private readonly CancellationTokenSource _stop = new();
    private readonly object _gate = new();
    private Process? _process;
    private string? _modelId;
    private string _workerError = "";
    private bool _disposed;
    private int _busy;

    internal async Task<SkyEstimate> AnalyzeAsync(byte[] jpeg, CancellationToken cancellationToken)
    {
        if (Interlocked.Exchange(ref _busy, 1) != 0) throw new InvalidOperationException("Inference already in progress.");
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, _stop.Token);
        timeout.CancelAfter(TimeSpan.FromSeconds(20));
        try
        {
            options.Validate();
            if (!options.Enabled) throw new InvalidOperationException("Sky model is disabled.");
            if (jpeg.Length is < 1 or > 4 * 1024 * 1024) throw new InvalidDataException("Invalid preview length.");
            Process process;
            lock (_gate)
            {
                ObjectDisposedException.ThrowIf(_disposed, this);
                if (_process is null)
                {
                    _workerError = "";
                    var start = new ProcessStartInfo(options.WorkerPath) {
                        UseShellExecute = false, CreateNoWindow = true,
                        RedirectStandardInput = true, RedirectStandardOutput = true,
                        RedirectStandardError = true
                    };
                    foreach (string arg in new[] { "experimental-stream", "--model", options.ModelPath, "--spec", options.SpecPath })
                        start.ArgumentList.Add(arg);
                    _process = Process.Start(start) ?? throw new IOException("Could not start vision worker.");
                    _ = DrainErrorsAsync(_process);
                }
                process = _process;
            }
            if (_modelId is null)
            {
                using var ready = JsonDocument.Parse(await ReadLineAsync(process.StandardOutput, timeout.Token));
                var root = ready.RootElement;
                if (!root.GetProperty("ready").GetBoolean() || root.GetProperty("protocol").GetInt32() != 1
                    || root.GetProperty("task").GetString() != "sky") throw new InvalidDataException("Unsupported vision worker.");
                _modelId = root.GetProperty("model_id").GetString() ?? throw new InvalidDataException("Missing model ID.");
            }
            byte[] size = new byte[4];
            BinaryPrimitives.WriteUInt32LittleEndian(size, (uint)jpeg.Length);
            await process.StandardInput.BaseStream.WriteAsync(size, timeout.Token).ConfigureAwait(false);
            await process.StandardInput.BaseStream.WriteAsync(jpeg, timeout.Token).ConfigureAwait(false);
            await process.StandardInput.BaseStream.FlushAsync(timeout.Token).ConfigureAwait(false);
            using var result = JsonDocument.Parse(await ReadLineAsync(process.StandardOutput, timeout.Token));
            return SkyEstimate.Parse(result.RootElement, _modelId);
        }
        catch (OperationCanceledException) when (!cancellationToken.IsCancellationRequested && !_stop.IsCancellationRequested)
        {
            StopProcess();
            throw new TimeoutException("Vision worker timed out. Reapply model settings to retry.");
        }
        catch (Exception error)
        {
            string diagnostic;
            lock (_gate) diagnostic = _workerError;
            StopProcess();
            if (error is not OperationCanceledException && diagnostic.Length != 0)
                throw new InvalidDataException($"{error.Message} {diagnostic.Trim()}", error);
            throw;
        }
        finally { Interlocked.Exchange(ref _busy, 0); }
    }

    internal static async Task<string> ReadLineAsync(StreamReader reader, CancellationToken token)
    {
        var line = new StringBuilder();
        char[] character = new char[1];
        while (line.Length < 4096)
        {
            if (await reader.ReadAsync(character.AsMemory(), token).ConfigureAwait(false) == 0)
                throw new IOException("Vision worker stopped. Check the model checksum and worker path.");
            if (character[0] == '\n') return line.ToString();
            line.Append(character[0]);
        }
        throw new InvalidDataException("Vision response exceeds 4096 characters.");
    }

    private async Task DrainErrorsAsync(Process process)
    {
        Stream stream = process.StandardError.BaseStream;
        byte[] buffer = new byte[4096];
        try
        {
            int read;
            while ((read = await stream.ReadAsync(buffer).ConfigureAwait(false)) != 0)
                lock (_gate)
                    if (ReferenceEquals(_process, process) && _workerError.Length < 4096)
                    {
                        string text = Encoding.UTF8.GetString(buffer, 0, read);
                        _workerError += text[..Math.Min(text.Length, 4096 - _workerError.Length)];
                    }
        }
        catch (IOException) { }
        catch (ObjectDisposedException) { }
    }

    private void StopProcess()
    {
        lock (_gate)
        {
            if (_process is not null)
            {
                try { if (!_process.HasExited) _process.Kill(entireProcessTree: true); }
                catch (InvalidOperationException) { }
                catch (System.ComponentModel.Win32Exception) { }
                _process.Dispose();
                _process = null;
                _modelId = null;
            }
        }
    }

    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed) return;
            _disposed = true;
            _stop.Cancel();
        }
        StopProcess();
        // Cancellation source stays alive for any in-flight catch/filter.
    }
}
