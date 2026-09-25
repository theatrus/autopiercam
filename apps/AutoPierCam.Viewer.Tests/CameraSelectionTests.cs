using AutoPierCam.Viewer;
using Xunit;

public sealed class CameraSelectionTests
{
    private static readonly CameraInventory Inventory = new([
        new(3, "ZWO ASI676MC", true), new(7, "ZWO ASI662MC", true), new(9, "ZWO ASI120MM", false)], 1, null);

    [Fact]
    public void MultipleCamerasRequireExplicitChoice()
    {
        var choices = CameraChoice.Create(Inventory, null, null);
        Assert.Equal(3, choices.Count);
        Assert.Null(CameraChoice.Selected(choices, null, null).Id);
        Assert.Equal("ZWO ASI662MC", CameraChoice.Selected(choices, 7, null).NameFilter);
    }

    [Theory]
    [InlineData(4, "ASI676")]
    [InlineData(3, "ASI662")]
    [InlineData(9, "ASI120MM")]
    public void MissingOrReassignedCameraDoesNotSilentlySwitch(int id, string filter)
    {
        var choices = CameraChoice.Create(Inventory, id, filter);
        var selected = CameraChoice.Selected(choices, id, filter);
        Assert.Equal(id, selected.Id);
        Assert.Equal(filter, selected.NameFilter);
        Assert.Contains("unavailable", selected.Label);
    }

    [Fact]
    public void SavedFilterMatchesCaseInsensitively()
    {
        var choices = CameraChoice.Create(Inventory, 3, "asi676");
        Assert.Equal("ZWO ASI676MC", CameraChoice.Selected(choices, 3, "asi676").NameFilter);
    }

    [Fact]
    public void DiscoveryFailurePreservesSavedSelection()
    {
        var choices = CameraChoice.Create(new([], 1, "SDK unavailable"), 3, "ASI676");
        Assert.Equal(2, choices.Count);
        Assert.Equal(3, CameraChoice.Selected(choices, 3, "ASI676").Id);
    }
}
