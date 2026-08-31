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
    internal static GeometryGroup DockIconGeometry { get; } = CreateDockIconGeometry();

    [ImportingConstructor]
    public PierCameraDockable(IProfileService profileService)
        : base(profileService)
    {
        Preview = PierCameraPreviewProcess.Runtime;
        Title = "Pier Camera";
        ImageGeometry = DockIconGeometry;
    }

    public override string ContentId => StableContentId;

    public IPierCameraPreviewRuntime Preview { get; }

    private static GeometryGroup CreateDockIconGeometry()
    {
        // These path data are the three paths from the canonical 32x32
        // assets/branding/autopiercam-monochrome.svg mark. Unioning them keeps
        // the camera, pier, orbit, and star as one theme-tintable silhouette.
        Geometry orbit = Geometry.Parse(
            "M16 2a11 11 0 1 1 0 22 11 11 0 0 1 0-22Zm0 2.5a8.5 8.5 0 1 0 0 17 8.5 8.5 0 0 0 0-17Z");
        Geometry cameraAndPier = Geometry.Parse(
            "M7 9h3V7.5A1.5 1.5 0 0 1 11.5 6h4A1.5 1.5 0 0 1 17 7.5V9h8a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2h-7v5h3l2 4H9l2-4h3v-5H7a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2Zm15 2a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7Zm0 1.5a2 2 0 1 1 0 4 2 2 0 0 1 0-4Z");
        Geometry star = Geometry.Parse("M5 3l.6 1.4L7 5l-1.4.6L5 7l-.6-1.4L3 5l1.4-.6L5 3Z");

        Geometry mark = Geometry.Combine(
            Geometry.Combine(orbit, cameraAndPier, GeometryCombineMode.Union, null),
            star,
            GeometryCombineMode.Union,
            null);
        var group = new GeometryGroup();
        group.Children.Add(mark);
        group.Freeze();
        return group;
    }
}
