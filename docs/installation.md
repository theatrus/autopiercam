# Installing AutoPierCam on Windows

AutoPierCam's x64 MSI is a current-user installation. It does not install a
Windows service or require a machine-wide application directory. The camera is
owned by the notification-area process running in the signed-in user's desktop
session.

## Before installing

- Use 64-bit Windows 10 version 1809 or newer.
- Install the ZWO Windows camera driver and connect the ASI camera. The MSI
  includes the reviewed `ASICamera2.dll` SDK runtime, but hardware drivers are
  supplied separately by ZWO.
- Close any developer build of AutoPierCam that is already using the camera.

Open `AutoPierCam-<version>-x64.msi` and review the Apache-2.0 license. The
feature page includes **Start AutoPierCam when I sign in (recommended)**,
selected by default. Clear it if capture should only run when manually started.
The choice can be changed later with **Modify** from Windows Installed apps.

The installer starts AutoPierCam when setup finishes. The tray menu can open
the Viewer, pause capture, capture immediately, or stop the application. The
Start menu also contains **AutoPierCam Viewer** and **Start AutoPierCam**.

## What is installed

Application files are installed for the current user at:

```text
%LOCALAPPDATA%\Programs\AutoPierCam
```

`autopiercam.exe`, `autopiercam-tray.exe`, and the pinned ZWO
`ASICamera2.dll` are adjacent in that directory. The complete self-contained
WinUI application is under `Viewer\`; no separate .NET runtime installation is
required. Apache and third-party license material is included with the payload.

The optional sign-in feature writes this current-user startup command:

```text
"...\autopiercam-tray.exe" --config "%LOCALAPPDATA%\AutoPierCam\autopiercam.toml" --sdk "...\ASICamera2.dll"
```

It uses `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`; no other Windows
account is affected.

## User data and logs

On first start, AutoPierCam creates and validates its configuration at:

```text
%LOCALAPPDATA%\AutoPierCam\autopiercam.toml
```

Relative capture paths are resolved beside that configuration, so the default
capture directory is `%LOCALAPPDATA%\AutoPierCam\captures`. The durable upload
ledger is kept beside the configuration as `autopiercam.upload.sqlite3`.

The tray writes persistent logs to:

```text
%LOCALAPPDATA%\AutoPierCam\logs\autopiercam.YYYY-MM-DD.log
```

The date in each filename is UTC. AutoPierCam retains the newest 14 daily log
files. The tray icon and Viewer show live state; the log files are the place to
look for startup, SDK, camera reconnect, capture, retention, and upload detail.

The installer never owns or removes `%LOCALAPPDATA%\AutoPierCam`. Settings,
captures, upload state, and logs therefore survive repair, upgrade, and
uninstall. After uninstalling, delete that exact directory manually in Explorer
only if its retained contents are no longer wanted.

## Silent installation and diagnostics

Install with the default sign-in behavior and a verbose MSI log:

```powershell
msiexec.exe /i .\AutoPierCam-0.1.0-x64.msi /qn /norestart /l*v .\autopiercam-install.log
```

Install without the optional sign-in feature:

```powershell
msiexec.exe /i .\AutoPierCam-0.1.0-x64.msi /qn /norestart ADDLOCAL=MainApplication /l*v .\autopiercam-install.log
```

For a normal uninstall, use Windows Installed apps. Administrators and support
staff can also pass the MSI product code to `msiexec.exe /x`; user data remains
untouched in either case.

Report problems at <https://github.com/theatrus/autopiercam/issues>. Include the
AutoPierCam version, camera model, relevant log excerpt, and whether the ZWO
camera appears in Device Manager. Do not include upload bearer tokens.

## Building and signing the MSI

The packaging pipeline requires Rust, the .NET SDK, and WiX Toolset 6 with the
UI and Util extensions. A normal local build stages, packages, validates,
administratively extracts, and inspects the MSI:

```powershell
.\scripts\Build-Installer.ps1
```

Release builds preserve an explicit signing gap:

```powershell
.\scripts\Build-Installer.ps1 -StageOnly
# Authenticode-sign first-party files under artifacts\installer\stage.
.\scripts\Build-Installer.ps1 -PackageOnly
# Authenticode-sign the resulting MSI.
.\scripts\Test-InstallerPackage.ps1
```

Do not replace or sign the vendored `ASICamera2.dll`. Both the build and package
test require its reviewed SHA-256:

```text
0c8778c3cce2012961b079e3c7d0d8348a8b3823939335d9e98148cb5d5dc34a
```

`-PackageOnly` never restages files, so signatures applied to `autopiercam.exe`,
`autopiercam-tray.exe`, `Viewer\AutoPierCam.Viewer.exe`, and first-party managed
assemblies are retained. The MSI is emitted at
`artifacts\installer\output\AutoPierCam-<version>-x64.msi`.

Brand artwork is intentionally not part of this first installer slice. MSI and
shortcut icon fields use executable defaults until the project icon is chosen.
