using System.Text;
using System.Text.Json;
using AutoPierCam.Viewer;
using Xunit;

public sealed class ExperimentalSkyModelTests
{
    [Fact]
    public void DisabledByDefaultAndDoesNotRequireModelFiles()
    {
        var options = new SkyModelOptions();
        Assert.False(options.Enabled);
        options.Validate();
        Assert.Throws<InvalidDataException>(() => (options with { Enabled = true }).Validate());
        Assert.Throws<InvalidDataException>(() => (options with { IntervalSeconds = 0 }).Validate());
    }

    [Fact]
    public void PredictionsAreLabeledExperimentalAndRejectBrokenScores()
    {
        var valid = new { task = "sky", model_id = "model", label = "clear", confidence = 0.9, probabilities = new[] { 0.9, 0.05, 0.05 } };
        using var json = JsonDocument.Parse(JsonSerializer.Serialize(valid));
        Assert.Contains("experimental", SkyEstimate.Parse(json.RootElement, "model").Caption);
        Assert.Throws<InvalidDataException>(() => SkyEstimate.Parse(json.RootElement, "other"));
        foreach (var bad in new[] {
            "{\"task\":\"sky\",\"model_id\":\"model\",\"label\":\"clear\",\"confidence\":0.9,\"probabilities\":[0.9]}",
            "{\"task\":\"sky\",\"model_id\":\"model\",\"label\":\"overcast\",\"confidence\":0.9,\"probabilities\":[0.9,0.05,0.05]}",
            "{\"error\":\"bad model\"}" })
        {
            using var document = JsonDocument.Parse(bad);
            Assert.Throws<InvalidDataException>(() => SkyEstimate.Parse(document.RootElement, "model"));
        }
        Assert.Contains("Uncertain", new SkyEstimate(null, 0.6, "model").Caption);
    }

    [Fact]
    public async Task WorkerResponsesAreBoundedAndTruncationFails()
    {
        static StreamReader Reader(string text) => new(new MemoryStream(Encoding.UTF8.GetBytes(text)));
        using var valid = Reader("{}\nnext");
        Assert.Equal("{}", await ExperimentalSkyModel.ReadLineAsync(valid, default));
        using var partial = Reader("{}");
        await Assert.ThrowsAsync<IOException>(() => ExperimentalSkyModel.ReadLineAsync(partial, default));
        using var huge = Reader(new string('x', 4097) + "\n");
        await Assert.ThrowsAsync<InvalidDataException>(() => ExperimentalSkyModel.ReadLineAsync(huge, default));
        using var canceled = new CancellationTokenSource();
        canceled.Cancel();
        using var input = Reader("{}\n");
        await Assert.ThrowsAnyAsync<OperationCanceledException>(() => ExperimentalSkyModel.ReadLineAsync(input, canceled.Token));
    }

    [Fact]
    public async Task DisabledModelNeverStartsWorker()
    {
        using var model = new ExperimentalSkyModel(new());
        await Assert.ThrowsAsync<InvalidOperationException>(() => model.AnalyzeAsync([1], default));
        model.Dispose(); // idempotent; UI disable/close may both dispose
    }

    [Fact]
    public void ViewerOptionsRoundTripWithoutAgentConfiguration()
    {
        string directory = Path.Combine(Path.GetTempPath(), "apc-sky-test-" + Guid.NewGuid());
        string path = Path.Combine(directory, "viewer-sky.json");
        try
        {
            Assert.False(SkyModelOptions.Load(path).Enabled);
            var expected = new SkyModelOptions(IntervalSeconds: 30);
            expected.Save(path);
            Assert.Equal(expected, SkyModelOptions.Load(path));
            Assert.Single(Directory.GetFiles(directory));
        }
        finally { if (Directory.Exists(directory)) Directory.Delete(directory, recursive: true); }
    }
}
