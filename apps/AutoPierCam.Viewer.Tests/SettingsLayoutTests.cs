using System.Xml.Linq;
using Xunit;

public sealed class SettingsLayoutTests
{
    [Fact]
    public void ReloadIsInSettingsAndManualStillIsNotALiveRefresh()
    {
        var markup = XDocument.Load(Path.Combine(AppContext.BaseDirectory, "MainWindow.xaml"));
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        var reload = Assert.Single(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") == "RefreshButton");
        Assert.Equal("Reload settings", (string?)reload.Attribute("Content"));
        Assert.Contains(reload.Ancestors(), e => (string?)e.Attribute(xaml + "Name") == "SettingsPane");
        Assert.DoesNotContain(reload.Ancestors(), e => e.Name.LocalName == "ScrollViewer");
        var capture = Assert.Single(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") == "CaptureButton");
        Assert.Equal("Save next frame", (string?)capture.Attribute("Content"));
    }

    [Fact]
    public void CompactTelemetryReplacesCardsAndConnectionFooter()
    {
        var markup = XDocument.Load(Path.Combine(AppContext.BaseDirectory, "MainWindow.xaml"));
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        var rootGrid = Assert.Single(markup.Root!.Elements());
        var rootRows = Assert.Single(rootGrid.Elements(), e => e.Name.LocalName == "Grid.RowDefinitions");
        Assert.Equal(2, rootRows.Elements().Count());
        var summary = Assert.Single(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") == "CaptureSummaryText");
        Assert.Equal("TextBlock", summary.Name.LocalName);
        Assert.Equal("12", (string?)summary.Attribute("FontSize"));
        Assert.Equal("Collapsed", (string?)summary.Attribute("Visibility"));
        Assert.Equal("NoWrap", (string?)summary.Attribute("TextWrapping"));
        Assert.DoesNotContain(summary.Ancestors(), e => e.Name.LocalName == "Border");
        string[] removed = ["ExposureValueText", "GainValueText", "TemperatureValueText", "ModeValueText", "AgentConnectionText"];
        Assert.DoesNotContain(markup.Descendants(), e => removed.Contains((string?)e.Attribute(xaml + "Name")));
        Assert.DoesNotContain(markup.Descendants().Attributes("Text"), a => a.Value.Contains("Protocol v1") || a.Value.Contains("Rust agent:"));
    }

    [Theory]
    [InlineData("SettingsPane", "Visibility", "Collapsed")]
    [InlineData("SettingsColumn", "Width", "0")]
    [InlineData("StatusWarningIcon", "Visibility", "Collapsed")]
    [InlineData("ConfigInfoBar", "IsOpen", "False")]
    [InlineData("ConfigInfoBar", "IsIconVisible", "False")]
    [InlineData("PreviewDetailText", "TextWrapping", "NoWrap")]
    public void PreviewFirstLayoutHidesUnneededControls(string name, string attribute, string expected)
    {
        var markup = XDocument.Load(Path.Combine(AppContext.BaseDirectory, "MainWindow.xaml"));
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        var element = Assert.Single(markup.Descendants(), element => (string?)element.Attribute(xaml + "Name") == name);
        Assert.Equal(expected, (string?)element.Attribute(attribute));
    }

    [Theory]
    [InlineData("CameraComboBox")]
    [InlineData("SaveButton")]
    [InlineData("ConfigInfoBar")]
    public void CameraSaveAndFeedbackRemainOutsideScrollingContent(string name)
    {
        var markup = XDocument.Load(Path.Combine(AppContext.BaseDirectory, "MainWindow.xaml"));
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        var element = Assert.Single(markup.Descendants(), element => (string?)element.Attribute(xaml + "Name") == name);
        Assert.DoesNotContain(element.Ancestors(), ancestor => ancestor.Name.LocalName == "ScrollViewer");
        if (name == "SaveButton") Assert.Equal("Save settings", (string?)element.Attribute("Content"));
    }
}
