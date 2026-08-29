use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub fn compile(
    assembly_name: &str,
    description: &str,
    original_filename: &str,
    graphical_shell: bool,
) {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    println!(
        "cargo:rerun-if-changed={}",
        manifest_dir.join("../windows_resources.rs").display()
    );
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    let (major, minor, patch) = parse_version(&version);
    let processor_architecture = match env::var("CARGO_CFG_TARGET_ARCH")
        .expect("Cargo target architecture")
        .as_str()
    {
        "x86_64" => "amd64",
        "x86" => "x86",
        "aarch64" => "arm64",
        architecture => panic!("unsupported Windows manifest architecture: {architecture}"),
    };

    let output_dir = PathBuf::from(env::var_os("OUT_DIR").expect("build output directory"));
    let manifest = output_dir.join(format!("{assembly_name}.manifest"));
    fs::write(
        &manifest,
        application_manifest(
            assembly_name,
            description,
            &version,
            processor_architecture,
            major,
            minor,
            patch,
            graphical_shell,
        ),
    )
    .expect("write generated AutoPierCam application manifest");

    let resource = output_dir.join(format!("{assembly_name}-version.rc"));
    fs::write(
        &resource,
        version_resource(
            &manifest,
            description,
            original_filename,
            &version,
            major,
            minor,
            patch,
        ),
    )
    .expect("write generated AutoPierCam Windows resources");

    embed_resource::compile(&resource, embed_resource::NONE)
        .manifest_required()
        .expect("AutoPierCam version resource and application manifest should compile");
}

fn parse_version(version: &str) -> (u16, u16, u16) {
    let mut components = version.split('.').map(|part| {
        part.parse::<u16>()
            .expect("AutoPierCam version components must be numeric and fit in 16 bits")
    });
    let major = components.next().expect("major version");
    let minor = components.next().expect("minor version");
    let patch = components.next().expect("patch version");
    assert!(
        components.next().is_none(),
        "AutoPierCam version must have exactly three parts"
    );
    (major, minor, patch)
}

#[allow(clippy::too_many_arguments)]
fn application_manifest(
    assembly_name: &str,
    description: &str,
    version: &str,
    processor_architecture: &str,
    major: u16,
    minor: u16,
    patch: u16,
    graphical_shell: bool,
) -> String {
    let graphical_settings = if graphical_shell {
        r#"
            <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
            <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2, PerMonitor</dpiAwareness>"#
    } else {
        ""
    };
    let common_controls = if graphical_shell {
        r#"
    <dependency>
        <dependentAssembly>
            <assemblyIdentity
                type="win32"
                name="Microsoft.Windows.Common-Controls"
                version="6.0.0.0"
                processorArchitecture="*"
                publicKeyToken="6595b64144ccf1df"
                language="*" />
        </dependentAssembly>
    </dependency>"#
    } else {
        ""
    };

    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly manifestVersion="1.0" xmlns="urn:schemas-microsoft-com:asm.v1">
    <assemblyIdentity
        type="win32"
        name="{assembly_name}"
        version="{major}.{minor}.{patch}.0"
        processorArchitecture="{processor_architecture}" />
    <description>{description} {version}</description>
    <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
        <security>
            <requestedPrivileges>
                <requestedExecutionLevel level="asInvoker" uiAccess="false" />
            </requestedPrivileges>
        </security>
    </trustInfo>
    <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
        <application>
            <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}" />
        </application>
    </compatibility>
    <application xmlns="urn:schemas-microsoft-com:asm.v3">
        <windowsSettings>{graphical_settings}
            <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
        </windowsSettings>
    </application>{common_controls}
</assembly>
"#,
    )
}

#[allow(clippy::too_many_arguments)]
fn version_resource(
    manifest: &Path,
    description: &str,
    original_filename: &str,
    version: &str,
    major: u16,
    minor: u16,
    patch: u16,
) -> String {
    let manifest = rc_path(manifest);
    format!(
        r#"1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3fL
FILEFLAGS 0x0L
FILEOS 0x40004L
FILETYPE 0x1L
FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "040904B0"
        BEGIN
            VALUE "CompanyName", "Yann Ramin\0"
            VALUE "FileDescription", "{description}\0"
            VALUE "FileVersion", "{version}.0\0"
            VALUE "InternalName", "{original_filename}\0"
            VALUE "LegalCopyright", "Copyright (c) 2026 Yann Ramin. Licensed under Apache-2.0.\0"
            VALUE "OriginalFilename", "{original_filename}\0"
            VALUE "ProductName", "AutoPierCam\0"
            VALUE "ProductVersion", "{version}\0"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x409, 1200
    END
END
"#
    )
}

fn rc_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    assert!(!path.contains('"'), "resource path cannot contain a quote");
    path
}
