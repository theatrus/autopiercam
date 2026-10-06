using System.Xml.Linq;
using AutoPierCam.Viewer;
using Xunit;

public sealed class SettingsLayoutTests
{
    [Fact]
    public void UsbRecoveryIsExplicitlyOptIn()
    {
        var toggle = Named(Markup(), "UsbResetOnFaultToggle");
        Assert.Equal("False", (string?)toggle.Attribute("IsOn"));
        Assert.Equal("False", (string?)toggle.Attribute("IsEnabled"));
    }

    [Fact]
    public void PreviewRateAllowsDecimalInputBelowOneFps()
    {
        var rate = Named(Markup(), "PreviewMaxFpsNumberBox");
        Assert.Equal("0.01", (string?)rate.Attribute("Minimum"));
        Assert.Equal("30", (string?)rate.Attribute("Maximum"));
        Assert.Equal("0.1", (string?)rate.Attribute("SmallChange"));
        Assert.Equal("2", (string?)rate.Attribute("Value"));
    }

    [Fact]
    public void HeaderUsesCompactBrandingAndPreservesStatusAndActions()
    {
        var markup = Markup();
        var header = Named(markup, "HeaderBar");
        Assert.Equal("16,6", (string?)header.Attribute("Padding"));
        Assert.Equal("16", (string?)Named(markup, "BrandNameText").Attribute("FontSize"));
        var logo = Named(markup, "BrandLogo");
        Assert.Equal("32", (string?)logo.Attribute("Width"));
        Assert.Equal("32", (string?)logo.Attribute("Height"));
        Assert.Equal("ms-appx:///Assets/autopiercam.png", (string?)logo.Attribute("Source"));
        foreach (string name in new[] { "BrandLogo", "StatusText", "CaptureButton", "PauseButton", "SettingsButton" })
            Assert.Contains(header, Named(markup, name).Ancestors());
        Assert.NotNull(Named(markup, "StatusText").Attribute("ToolTipService.ToolTip"));
        var project = XDocument.Load(Path.Combine(AppContext.BaseDirectory, "Viewer.csproj"));
        var asset = Assert.Single(project.Descendants("Content"), e => (string?)e.Attribute("Link") == @"Assets\autopiercam.png");
        Assert.Equal("PreserveNewest", (string?)asset.Attribute("CopyToOutputDirectory"));
        Assert.Equal("PreserveNewest", (string?)asset.Attribute("CopyToPublishDirectory"));
    }

    [Fact]
    public void PreviewDetailsContainLiveDiagnosticsNotResolutionBoilerplate()
    {
        var markup = Markup();
        var details = Named(markup, "PreviewDiagnosticsText").Parent!;
        Assert.Equal(3, details.Elements().Count()); // heading, live data, opt-in model settings
        Assert.Contains(Named(markup, "SkyModelSettingsButton"), details.Elements());
        Assert.Equal("Collapsed", (string?)Named(markup, "SkyEstimateText").Attribute("Visibility"));
        Assert.DoesNotContain(markup.Descendants().Attributes("Text"), a =>
            a.Value.Contains("1920-pixel long edge") || a.Value.Contains("full sensor by default"));
    }

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
    [InlineData("SaveButton")]
    [InlineData("ConfigInfoBar")]
    public void SaveAndFeedbackRemainOutsideScrollingContent(string name)
    {
        var markup = XDocument.Load(Path.Combine(AppContext.BaseDirectory, "MainWindow.xaml"));
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        var element = Assert.Single(markup.Descendants(), element => (string?)element.Attribute(xaml + "Name") == name);
        Assert.DoesNotContain(element.Ancestors(), ancestor => ancestor.Name.LocalName == "ScrollViewer");
        if (name == "SaveButton") Assert.Equal("Save all settings", (string?)element.Attribute("Content"));
    }

    private static XElement Named(XDocument markup, string name)
    {
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        return Assert.Single(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") == name);
    }

    private static XDocument Markup() => XDocument.Load(Path.Combine(AppContext.BaseDirectory, "MainWindow.xaml"));

    [Theory]
    [InlineData(1, 1180, 760)]
    [InlineData(1.5, 1770, 1140)]
    [InlineData(2, 2360, 1520)]
    public void DefaultWindowUsesLogicalSizeAtHighDpi(double scale, int width, int height) =>
        Assert.Equal((width, height), SettingsLayout.InitialWindowSize(scale, 3840, 2160));

    [Fact]
    public void DefaultWindowStaysWithinSmallMonitorWorkArea() =>
        Assert.Equal((1228, 691), SettingsLayout.InitialWindowSize(2, 1365, 768));

    [Theory]
    [InlineData("CaptureSection", "CaptureSettingsScroll", "CameraComboBox")]
    [InlineData("CaptureSection", "CaptureSettingsScroll", "MinGainNumberBox")]
    [InlineData("CaptureSection", "CaptureSettingsScroll", "PreferShortExposuresToggle")]
    [InlineData("SharingSection", "SharingSettingsScroll", "SharingIntervalNumberBox")]
    public void EachSectionHasOneWidthConstrainedScrollRegion(string section, string scroll, string field)
    {
        var markup = Markup();
        var region = Named(markup, scroll);
        Assert.Same(region, Assert.Single(Named(markup, section).Descendants(), e => e.Name.LocalName == "ScrollViewer"));
        Assert.Contains(region, Named(markup, field).Ancestors());
        Assert.Equal("1", (string?)region.Attribute("Grid.Row"));
        Assert.Equal("Disabled", (string?)region.Attribute("HorizontalScrollMode"));
        Assert.Equal("Disabled", (string?)region.Attribute("HorizontalScrollBarVisibility"));
        Assert.Equal("Stretch", (string?)region.Attribute("HorizontalContentAlignment"));
        Assert.Equal("0,4,20,12", (string?)region.Attribute("Padding"));
        // Expanded groups do not contain a second explicit scroll viewport.
        Assert.DoesNotContain(region.Descendants(), e => e.Name.LocalName == "ScrollViewer");
    }

    [Theory]
    [InlineData(1, 640, 820)]
    [InlineData(1.5, 960, 1230)]
    [InlineData(2, 1280, 1640)]
    public void SettingsWindowHasIndependentDpiAwareSize(double scale, int width, int height)
    {
        Assert.Equal((width, height), SettingsLayout.SettingsWindowSize(scale, 3840, 2160));
        Assert.Equal((1228, 691), SettingsLayout.SettingsWindowSize(2, 1365, 768));
    }

    [Theory]
    [InlineData("MaxGainNumberBox")]
    [InlineData("RetentionMinFreeMiBNumberBox")]
    [InlineData("SharingSpacingNumberBox")]
    public void LongNumericFieldsStackInsteadOfClippingInHalfWidthColumns(string name)
    {
        var field = Named(Markup(), name);
        Assert.DoesNotContain(field.Ancestors().TakeWhile(e => e.Name.LocalName != "ScrollViewer")
            .Select(e => (string?)e.Attribute("Grid.Column")), value => value == "1");
    }

    [Fact]
    public void ChatstronomyIsASettingsSectionNotAToolbarButton()
    {
        var markup = Markup();
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        Assert.DoesNotContain(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") == "SharingButton");
        var bar = Named(markup, "SettingsSectionBar");
        Assert.Equal("SelectorBar", bar.Name.LocalName);
        Assert.Equal(["Imaging", "Chatstronomy"], bar.Elements().Select(e => (string?)e.Attribute("Text")));
        foreach (string section in new[] { "CaptureSection", "SharingSection" })
            Assert.Contains(Named(markup, section).Ancestors(), e => (string?)e.Attribute(xaml + "Name") == "SettingsPane");
        Assert.Equal("Collapsed", (string?)Named(markup, "SharingSection").Attribute("Visibility"));
    }

    [Fact]
    public void BothTabsUseOneSharedSaveAndDiscardOutsideTabsAndScrolling()
    {
        var markup = Markup();
        XNamespace xaml = "http://schemas.microsoft.com/winfx/2006/xaml";
        foreach (var (name, label) in new[] { ("SaveButton", "Save all settings"), ("CaptureDiscardButton", "Discard all changes") })
        {
            var button = Named(markup, name);
            Assert.Equal(label, (string?)button.Attribute("Content"));
            Assert.Contains(button.Ancestors(), e => (string?)e.Attribute(xaml + "Name") == "SettingsPane");
            Assert.DoesNotContain(button.Ancestors(), e => e.Name.LocalName == "ScrollViewer" ||
                (string?)e.Attribute(xaml + "Name") is "CaptureSection" or "SharingSection");
        }
        Assert.DoesNotContain(markup.Descendants(), e => (string?)e.Attribute(xaml + "Name") is "SharingSaveButton" or "SharingDiscardButton");
        foreach (string name in new[] { "ConfigInfoBar", "SharingInfoBar", "SettingsSaveSummary" })
            Assert.DoesNotContain(Named(markup, name).Ancestors(), e => e.Name.LocalName == "ScrollViewer" ||
                (string?)e.Attribute(xaml + "Name") is "CaptureSection" or "SharingSection");
        Assert.Equal("{StaticResource AccentButtonStyle}", (string?)Named(markup, "SaveButton").Attribute("Style"));
    }

    [Theory]
    [InlineData("StillIntervalNumberBox")]
    [InlineData("PreviewMaxFpsNumberBox")]
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
