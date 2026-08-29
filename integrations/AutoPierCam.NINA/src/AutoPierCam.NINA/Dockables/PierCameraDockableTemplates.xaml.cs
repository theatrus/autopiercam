using System.ComponentModel.Composition;
using System.Windows;

namespace AutoPierCam.NINA.Dockables;

[Export(typeof(ResourceDictionary))]
public partial class PierCameraDockableTemplates : ResourceDictionary
{
    public PierCameraDockableTemplates()
    {
        InitializeComponent();
    }
}
