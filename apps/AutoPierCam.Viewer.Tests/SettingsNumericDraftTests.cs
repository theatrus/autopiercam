using System.Globalization;
using AutoPierCam.Viewer;
using Xunit;

public class SettingsNumericDraftTests
{
    [Theory]
    [InlineData("en-US", "0.5", 0.5)]
    [InlineData("de-DE", "0,5", 0.5)]
    [InlineData("en-US", "20", 20)]
    public void LiveTextIsComparedAndSavedWithoutACommittedNumberBoxValue(string locale, string draft, double expected)
    {
        var culture = CultureInfo.GetCultureInfo(locale);
        Assert.NotEqual(SettingsFormValues.Number(1), SettingsFormValues.NumberText(draft, culture));
        Assert.Equal(expected, SettingsFormValues.ReadNumberText(draft, culture));
    }

    [Theory]
    [InlineData("-")]
    [InlineData("garbage")]
    [InlineData("NaN")]
    [InlineData("Infinity")]
    public void InvalidDraftIsDirtyAndNeverSilentlySavedAsAnOptionalBlank(string draft)
    {
        Assert.StartsWith("invalid:", SettingsFormValues.NumberText(draft, CultureInfo.InvariantCulture));
        Assert.Throws<InvalidOperationException>(() => SettingsFormValues.ReadNumberText(draft, CultureInfo.InvariantCulture));
    }

    [Fact]
    public void EmptyOptionalDraftRemainsDistinctFromInvalidInput()
    {
        Assert.True(double.IsNaN(SettingsFormValues.ReadNumberText("", CultureInfo.InvariantCulture)));
        Assert.Equal("disabled", SettingsFormValues.NumberText("", CultureInfo.InvariantCulture));
    }
}
