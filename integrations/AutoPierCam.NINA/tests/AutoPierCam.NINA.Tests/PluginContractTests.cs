using System.ComponentModel.Composition;
using System.ComponentModel.Composition.Hosting;
using System.Reflection;
using System.Runtime.ExceptionServices;
using System.Runtime.InteropServices;
using AutoPierCam.NINA.Dockables;
using AutoPierCam.NINA.Preview;
using NINA.Equipment.Interfaces.ViewModel;
using NINA.Plugin.Interfaces;
using NINA.Profile.Interfaces;

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
        Assert.Equal(
            "AutoPierCam",
            assembly.GetCustomAttribute<AssemblyTitleAttribute>()?.Title);
        Assert.Equal(new Version(0, 1, 0, 0), assembly.GetName().Version);
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
        Assert.Empty(typeof(PierCameraPreviewRuntime).GetCustomAttributes<ExportAttribute>());
        Assert.Null(
            typeof(PierCameraPreviewRuntime)
                .GetCustomAttribute<PartCreationPolicyAttribute>());
    }

    [Fact]
    public void SeparateNinaCompositionScopesUseTheSameProcessRuntime()
    {
        RunInSta(() =>
        {
            Assembly pluginAssembly = typeof(AutoPierCamPlugin).Assembly;
            using var manifestCatalog = new AssemblyCatalog(pluginAssembly);
            using var manifestContainer = new CompositionContainer(manifestCatalog);
            var manifest = Assert.IsType<AutoPierCamPlugin>(
                manifestContainer.GetExportedValue<IPluginManifest>());

            using var dockableCatalog = new AssemblyCatalog(pluginAssembly);
            using var dockableContainer = new CompositionContainer(dockableCatalog);
            dockableContainer.ComposeExportedValue(CreateProfileService());
            var dockable = Assert.IsType<PierCameraDockable>(
                dockableContainer.GetExportedValue<IDockableVM>());

            Assert.Same(PierCameraPreviewProcess.Runtime, manifest.PreviewRuntime);
            Assert.Same(manifest.PreviewRuntime, dockable.Preview);
            Assert.Empty(manifestContainer.GetExports<IPierCameraPreviewRuntime>());
            Assert.Empty(dockableContainer.GetExports<IPierCameraPreviewRuntime>());
        });
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
        Assert.Equal([typeof(IProfileService)], dependencies);
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

    [Fact]
    public async Task ManifestLifecycleControlsTheProcessRuntimeIdempotently()
    {
        var firstManifest = new AutoPierCamPlugin();
        var secondManifest = new AutoPierCamPlugin();
        Assert.Same(firstManifest.PreviewRuntime, secondManifest.PreviewRuntime);

        await firstManifest.Initialize();
        await secondManifest.Initialize();
        await Task.WhenAll(firstManifest.Teardown(), secondManifest.Teardown());
        await firstManifest.Teardown();
    }

    private static void AssertExport<TPart, TContract>()
    {
        ExportAttribute? export = typeof(TPart)
            .GetCustomAttributes<ExportAttribute>()
            .SingleOrDefault(attribute => attribute.ContractType == typeof(TContract));
        Assert.NotNull(export);
    }

    private static IProfileService CreateProfileService() =>
        DispatchProxy.Create<IProfileService, NoOpProfileServiceProxy>();

    private static void RunInSta(Action action)
    {
        Exception? failure = null;
        var thread = new Thread(() =>
        {
            try
            {
                action();
            }
            catch (Exception exception)
            {
                failure = exception;
            }
        })
        {
            IsBackground = true,
        };
        thread.SetApartmentState(ApartmentState.STA);
        thread.Start();

        Assert.True(thread.Join(TimeSpan.FromSeconds(15)), "STA composition test timed out.");
        if (failure is not null)
        {
            ExceptionDispatchInfo.Capture(failure).Throw();
        }
    }

    public class NoOpProfileServiceProxy : DispatchProxy
    {
        protected override object? Invoke(MethodInfo? targetMethod, object?[]? args)
        {
            ArgumentNullException.ThrowIfNull(targetMethod);
            return targetMethod.ReturnType == typeof(void)
                ? null
                : targetMethod.ReturnType.IsValueType
                    ? Activator.CreateInstance(targetMethod.ReturnType)
                    : null;
        }
    }
}
