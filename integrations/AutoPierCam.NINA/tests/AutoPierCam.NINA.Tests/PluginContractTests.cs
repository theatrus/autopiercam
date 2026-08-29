using System.ComponentModel.Composition;
using System.Reflection;
using System.Runtime.InteropServices;
using AutoPierCam.NINA.Dockables;
using AutoPierCam.NINA.Preview;
using NINA.Equipment.Interfaces.ViewModel;
using NINA.Plugin.Interfaces;

namespace AutoPierCam.NINA.Tests;

public sealed class PluginContractTests
{
    [Fact]
    public void AssemblyCarriesPermanentIdentifierAndNinaMinimumVersion()
    {
        Assembly assembly = typeof(AutoPierCamPlugin).Assembly;

        Assert.Equal(
            "cb626d89-4f49-454f-8d42-01153902d12b",
            assembly.GetCustomAttribute<GuidAttribute>()?.Value);
        Assert.Contains(
            assembly.GetCustomAttributes<AssemblyMetadataAttribute>(),
            item => item.Key == "MinimumApplicationVersion" && item.Value == "3.2.0.9001");
        Assert.Contains(
            assembly.GetCustomAttributes<AssemblyMetadataAttribute>(),
            item => item.Key == "License" && item.Value == "Apache-2.0");
    }

    [Fact]
    public void ManifestAndDockableUseNinaMefContracts()
    {
        AssertExport<AutoPierCamPlugin, IPluginManifest>();
        AssertExport<PierCameraDockable, IDockableVM>();
        AssertExport<PierCameraPreviewRuntime, IPierCameraPreviewRuntime>();

        PartCreationPolicyAttribute? lifecycle =
            typeof(PierCameraPreviewRuntime).GetCustomAttribute<PartCreationPolicyAttribute>();
        Assert.NotNull(lifecycle);
        Assert.Equal(CreationPolicy.Shared, lifecycle.CreationPolicy);
    }

    [Fact]
    public void DockableHasStableIdentityAndDoesNotImportCameraServices()
    {
        Assert.Equal("AutoPierCam.PierCamera", PierCameraDockable.StableContentId);

        Type[] dependencies = typeof(PierCameraDockable)
            .GetConstructors()
            .Single()
            .GetParameters()
            .Select(parameter => parameter.ParameterType)
            .ToArray();
        Assert.Contains(typeof(IPierCameraPreviewRuntime), dependencies);
        Assert.DoesNotContain(
            dependencies,
            type => type.FullName?.Contains("CameraMediator", StringComparison.OrdinalIgnoreCase) == true);
    }

    [Fact]
    public async Task ProcessRuntimeStartAndStopAreIdempotent()
    {
        var runtime = new PierCameraPreviewRuntime();

        runtime.Start();
        runtime.Start();
        await Task.WhenAll(runtime.StopAsync(), runtime.StopAsync());
        await runtime.StopAsync();
    }

    private static void AssertExport<TPart, TContract>()
    {
        ExportAttribute? export = typeof(TPart)
            .GetCustomAttributes<ExportAttribute>()
            .SingleOrDefault(attribute => attribute.ContractType == typeof(TContract));
        Assert.NotNull(export);
    }
}
