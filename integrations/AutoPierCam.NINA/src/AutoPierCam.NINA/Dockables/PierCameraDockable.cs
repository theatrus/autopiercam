using System.ComponentModel.Composition;
using System.Windows;
using System.Windows.Media;
using AutoPierCam.NINA.Preview;
using NINA.Equipment.Interfaces.ViewModel;
using NINA.Profile.Interfaces;
using NINA.WPF.Base.ViewModel;

namespace AutoPierCam.NINA.Dockables;

[Export(typeof(IDockableVM))]
public sealed class PierCameraDockable : DockableVM
{
    internal const string StableContentId = "AutoPierCam.PierCamera";

    [ImportingConstructor]
    public PierCameraDockable(
        IProfileService profileService,
        IPierCameraPreviewRuntime preview)
        : base(profileService)
    {
        Preview = preview;
        Title = "Pier Camera";

        var resources = new ResourceDictionary
        {
            Source = new Uri(
                "AutoPierCam.NINA;component/Dockables/PierCameraDockableTemplates.xaml",
                UriKind.RelativeOrAbsolute),
        };
        if (resources["AutoPierCam_PierCameraSVG"] is GeometryGroup geometry)
        {
            geometry.Freeze();
            ImageGeometry = geometry;
        }
    }

    public override string ContentId => StableContentId;

    public IPierCameraPreviewRuntime Preview { get; }
}
