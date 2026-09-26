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

    private static XElement Named(XDocument markup, string name)
    {
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        return Assert.Single(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") == name);
    }

    private static XDocument Markup() => XDocument.Load(Path.Combine(AppContext.BaseDirectory, "MainWindow.xaml"));

    [Fact]
    public void ChatstronomyIsASettingsSectionNotAToolbarButton()
    {
        var markup = Markup();
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        Assert.DoesNotContain(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") == "SharingButton");
        var bar = Named(markup, "SettingsSectionBar");
        Assert.Equal("SelectorBar", bar.Name.LocalName);
        Assert.Equal(["Capture", "Chatstronomy"], bar.Elements().Select(e => (string?)e.Attribute("Text")));
        foreach (string section in new[] { "CaptureSection", "SharingSection" })
            Assert.Contains(Named(markup, section).Ancestors(), e => (string?)e.Attribute(xaml + "Name") == "SettingsPane");
        Assert.Equal("Collapsed", (string?)Named(markup, "SharingSection").Attribute("Visibility"));
    }

    [Theory]
    [InlineData("CaptureSection", "RefreshButton", "CaptureDiscardButton", "SaveButton", "ConfigInfoBar", "CaptureKeepEditsButton")]
    [InlineData("SharingSection", "SharingReloadButton", "SharingDiscardButton", "SharingSaveButton", "SharingInfoBar", "SharingKeepEditsButton")]
    public void BothSectionsShareReloadDiscardSaveAndReviewLayout(
        string section, string reload, string discard, string save, string infoBar, string keepEdits)
    {
        var markup = Markup();
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        foreach (var (name, content) in new[] {
            (reload, "Reload settings"), (discard, "Discard changes"), (save, "Save settings") })
        {
            var button = Named(markup, name);
            Assert.Equal(content, (string?)button.Attribute("Content"));
            Assert.Contains(button.Ancestors(), e => (string?)e.Attribute(xaml + "Name") == section);
            Assert.DoesNotContain(button.Ancestors(), e => e.Name.LocalName == "ScrollViewer");
        }
        Assert.Equal("{StaticResource AccentButtonStyle}", (string?)Named(markup, save).Attribute("Style"));
        var bar = Named(markup, infoBar);
        Assert.DoesNotContain(bar.Ancestors(), e => e.Name.LocalName == "ScrollViewer");
        var keep = Named(markup, keepEdits);
        Assert.Equal("Keep my edits", (string?)keep.Attribute("Content"));
        Assert.Equal("Collapsed", (string?)keep.Attribute("Visibility"));
        Assert.Contains(keep.Ancestors(), e => e == bar);
    }

    [Theory]
    [InlineData("StillIntervalNumberBox")]
    [InlineData("SharingIntervalNumberBox")]
    [InlineData("SharingThresholdNumberBox")]
    [InlineData("SharingBurstNumberBox")]
    [InlineData("SharingSpacingNumberBox")]
    public void NumbersUseBoundedNumberBoxesWithALabelAndUnitInBothSections(string name)
    {
        var box = Named(Markup(), name);
        Assert.Equal("NumberBox", box.Name.LocalName);
        Assert.NotNull(box.Attribute("Minimum"));
        Assert.NotNull(box.Attribute("Maximum"));
        Assert.Equal("Compact", (string?)box.Attribute("SpinButtonPlacementMode"));
        var label = box.ElementsBeforeSelf().Last();
        Assert.Equal("TextBlock", label.Name.LocalName);
    }

    [Fact]
    public void OnOffSettingsUseToggleSwitchesInBothSections()
    {
        var markup = Markup();
        foreach (string name in new[] { "AdaptiveExposureToggle", "Raw16Toggle", "UploadEnabledToggle",
            "SharingEnabledToggle", "SharingSnapshotsToggle", "SharingScenesToggle" })
            Assert.Equal("ToggleSwitch", Named(markup, name).Name.LocalName);
        // The only checkbox left confirms forgetting a pairing.
        var boxes = markup.Descendants().Where(e => e.Name.LocalName == "CheckBox").ToList();
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        Assert.Equal(["SharingForgetConsentCheckBox"], boxes.Select(e => (string?)e.Attribute(xaml + "Name")));
    }
}
