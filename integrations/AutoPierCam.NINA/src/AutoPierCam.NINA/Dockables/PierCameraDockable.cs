using System.ComponentModel.Composition;
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
    public PierCameraDockable(IProfileService profileService)
        : base(profileService)
    {
        Preview = PierCameraPreviewProcess.Runtime;
        Title = "Pier Camera";

        // Placeholder camera mark until the application-wide icon set is finalized.
        var geometry = new GeometryGroup();
        geometry.Children.Add(new RectangleGeometry(new(2, 8, 28, 20), 3, 3));
        geometry.Children.Add(new RectangleGeometry(new(8, 4, 9, 5), 1, 1));
        geometry.Children.Add(new EllipseGeometry(new(16, 18), 7, 7));
        geometry.Children.Add(new EllipseGeometry(new(16, 18), 3, 3));
        geometry.Freeze();
        ImageGeometry = geometry;
    }

    public override string ContentId => StableContentId;

    public IPierCameraPreviewRuntime Preview { get; }
}
