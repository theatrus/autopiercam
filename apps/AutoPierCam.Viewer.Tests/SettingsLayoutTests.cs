using System.Xml.Linq;
using Xunit;

public sealed class SettingsLayoutTests
{
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
        if (name == "SaveButton") Assert.Equal("Save and restart capture", (string?)element.Attribute("Content"));
    }
}
