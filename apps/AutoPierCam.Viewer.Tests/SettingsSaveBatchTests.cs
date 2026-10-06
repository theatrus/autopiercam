using AutoPierCam.Viewer;
using Xunit;

public class SettingsSaveBatchTests
{
    [Theory]
    [InlineData(true)]
    [InlineData(false)]
    public async Task InvalidDraftInEitherTabPreventsBothWrites(bool badImaging)
    {
        int writes = 0;
        var result = await SettingsSaveBatch.RunAsync(true, true,
            () => { if (badImaging) throw new InvalidOperationException("bad imaging"); },
            () => { if (!badImaging) throw new InvalidOperationException("bad sharing"); },
            () => { writes++; return Task.FromResult(true); },
            () => { writes++; return Task.FromResult(true); });
        Assert.False(result.Success);
        Assert.Equal(0, writes);
        Assert.StartsWith("Nothing saved:", result.Message);
    }

    [Theory]
    [InlineData(true, true, true, true)]
    [InlineData(true, false, true, false)]
    [InlineData(false, true, false, true)]
    [InlineData(false, false, false, false)]
    public async Task SaveWritesOnlyChangedTabs(bool imaging, bool sharing, bool expectedImaging, bool expectedSharing)
    {
        bool imagingWritten = false, sharingWritten = false;
        var result = await SettingsSaveBatch.RunAsync(imaging, sharing, () => { }, () => { },
            () => Task.FromResult(imagingWritten = true), () => Task.FromResult(sharingWritten = true));
        Assert.True(result.Success);
        Assert.Equal(expectedImaging, imagingWritten);
        Assert.Equal(expectedSharing, sharingWritten);
    }

    [Fact]
    public async Task ImagingConflictPreventsSharingWriteAndRetainsBothDrafts()
    {
        bool sharingWritten = false;
        var result = await SettingsSaveBatch.RunAsync(true, true, () => { }, () => { },
            () => Task.FromResult(false), () => Task.FromResult(sharingWritten = true));
        Assert.False(result.Success);
        Assert.False(result.ImagingSaved);
        Assert.False(sharingWritten);
    }

    [Fact]
    public async Task SharingFailureAfterImagingSaveIsExplicitAndRetryDoesNotRewriteImaging()
    {
        int imagingWrites = 0;
        var result = await SettingsSaveBatch.RunAsync(true, true, () => { }, () => { },
            () => { imagingWrites++; return Task.FromResult(true); }, () => Task.FromResult(false));
        Assert.False(result.Success);
        Assert.True(result.ImagingSaved);
        Assert.False(result.SharingSaved);
        Assert.Contains("Imaging saved; Chatstronomy", result.Message);
        var retry = await SettingsSaveBatch.RunAsync(!result.ImagingSaved, true, () => { }, () => { },
            () => { imagingWrites++; return Task.FromResult(true); }, () => Task.FromResult(true));
        Assert.True(retry.Success);
        Assert.Equal(1, imagingWrites);
    }
}
