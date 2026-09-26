using System.Globalization;
using System.Text.Json;
using AutoPierCam.Viewer;
using Xunit;

public sealed class SettingsFormValuesTests
{
    private static AgentConfiguration Config => JsonSerializer.Deserialize<AgentConfiguration>(
        File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "config-default.json")))!;

    [Fact]
    public void LoadingAndRepeatingProgrammaticNotificationsDoNotCreateChanges()
    {
        var baseline = SettingsFormValues.FromConfiguration(Config);
        for (int notification = 0; notification < 10; notification++)
            Assert.Equal(baseline, SettingsFormValues.FromConfiguration(Config));
        Assert.Equal("disabled", baseline.RetentionMax);
        Assert.Equal("disabled", baseline.RetentionFree);
    }

    [Theory]
    [InlineData("en-US", "60,000.000", 60000d)]
    [InlineData("de-DE", "60.000,000", 60000d)]
    [InlineData("fr-FR", "8765,125", 8765.125)]
    [InlineData("en-US", "6e4", 60000d)]
    public void NumberFormattingIsNotAnEdit(string culture, string text, double value)
    {
        Assert.Equal(SettingsFormValues.Number(value), SettingsFormValues.NumberText(text, CultureInfo.GetCultureInfo(culture)));
    }

    [Fact]
    public void PendingInvalidTextIsDirtyAndBlankOptionalLimitsStayDisabled()
    {
        Assert.NotEqual(SettingsFormValues.Number(60000), SettingsFormValues.NumberText("60x", CultureInfo.InvariantCulture));
        Assert.Equal(SettingsFormValues.Number(double.NaN), SettingsFormValues.NumberText("", CultureInfo.InvariantCulture));
        Assert.NotEqual(SettingsFormValues.Number(double.NaN), SettingsFormValues.NumberText("NaN", CultureInfo.InvariantCulture));
    }

    [Fact]
    public void EditingThenRevertingMatchesTheBaseline()
    {
        var original = SettingsFormValues.FromConfiguration(Config);
        var edited = original with { MaxExposure = SettingsFormValues.Number(8765), Upload = !original.Upload };
        Assert.NotEqual(original, edited);
        Assert.Equal(original, edited with { MaxExposure = original.MaxExposure, Upload = original.Upload });
    }

    [Fact]
    public void CameraRediscoveryIsCleanButOperatorSelectionIsAnEdit()
    {
        var config = Config;
        config = config with { Camera = config.Camera with { CameraId = 0, NameContains = "ASI662MC" } };
        var loaded = SettingsFormValues.FromConfiguration(config);
        Assert.Equal(loaded, loaded with { CameraId = 0 });
        Assert.NotEqual(loaded, loaded with { CameraId = 1 });
        Assert.NotEqual(loaded, loaded with { CameraId = null });
        Assert.NotEqual(loaded, loaded with { CameraFilter = "ASI676MC" });
    }

    [Fact]
    public void OptionalDefaultsAndSurroundingWhitespaceAreNormalized()
    {
        var config = Config;
        var equivalent = config with {
            Camera = config.Camera with { Raw16 = false, NameContains = " " },
            Upload = config.Upload with { Endpoint = "  " }
        };
        config = config with { Camera = config.Camera with { Raw16 = null, NameContains = null }, Upload = config.Upload with { Endpoint = null } };
        Assert.Equal(SettingsFormValues.FromConfiguration(config), SettingsFormValues.FromConfiguration(equivalent));
    }
}
