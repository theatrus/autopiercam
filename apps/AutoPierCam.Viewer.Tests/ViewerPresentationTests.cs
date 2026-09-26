using AutoPierCam.Viewer;
using Xunit;

public sealed class ViewerPresentationTests
{
    [Theory]
    [InlineData(2_000_000L, 300L, "unknown", "Exposure 2 s · Gain 300")]
    [InlineData(500L, 0L, "night", "Exposure 500 µs · Gain 0 · Night")]
    [InlineData(10_000L, null, "day", "Exposure 10 ms · Day")]
    [InlineData(null, 25L, null, "Gain 25")]
    [InlineData(null, null, "unknown", "")]
    [InlineData(null, null, null, "")]
    [InlineData(0L, -1L, "offline", "")]
    public void SummaryIncludesOnlyAvailableReadings(long? exposureUs, long? gain, string? mode, string expected) =>
        Assert.Equal(expected, ViewerPresentation.CaptureSummary(exposureUs, gain, mode));

    [Fact]
    public void ViewerAcceptsFullHdAndRetainsStrictBounds()
    {
        var metadata = new PreviewFrameMetadata {
            Version = 1, Width = 1920, Height = 1080, ContentType = "image/jpeg", Mode = "unknown"
        };
        metadata.Validate();
        Assert.Throws<PreviewProtocolException>(() => (metadata with { Width = 1921 }).Validate());
        Assert.Throws<PreviewProtocolException>(() => (metadata with { Height = 1921 }).Validate());
    }

    [Theory]
    [InlineData("starting")]
    [InlineData("capturing")]
    [InlineData("paused")]
    public void NormalStatesHaveNoWarning(string state) =>
        Assert.False(ViewerPresentation.HasWarning(new AgentStatus { State = state }));

    [Fact]
    public void FaultsAndBlockedStorageKeepWarnings()
    {
        Assert.True(ViewerPresentation.HasWarning(new AgentStatus { State = "faulted" }));
        Assert.True(ViewerPresentation.HasWarning(new AgentStatus { State = "capturing", Storage = new() { Pressure = "blocked" } }));
        Assert.True(ViewerPresentation.HasWarning(new AgentStatus { State = "capturing", LastError = "SDK failed" }));
    }

    [Fact]
    public void CaptionIsShortAndDiagnosticsAreNotInTheOverlay()
    {
        Assert.Equal("Preview · 1920×1080 · 3s ago", ViewerPresentation.PreviewCaption("1920×1080", TimeSpan.FromSeconds(3), false, false));
        Assert.Equal("Capture stopped · 1920×1080 · 2m ago", ViewerPresentation.PreviewCaption("1920×1080", TimeSpan.FromMinutes(2), true, false));
    }

    [Fact]
    public void MissingCameraChoiceOpensSettingsButNormalStartupDoesNot()
    {
        Assert.True(ViewerPresentation.NeedsCameraSelection(new AgentStatus { State = "faulted", LastError = "multiple cameras; choose a camera" }));
        Assert.False(ViewerPresentation.NeedsCameraSelection(new AgentStatus { State = "starting" }));
    }
}
