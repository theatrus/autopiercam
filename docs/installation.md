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

The installer starts AutoPierCam when setup finishes, independently of the
start-at-sign-in choice. The tray menu can open the Viewer, captures, and logs;
pause capture; capture immediately; or stop the application. The Start menu
also contains **AutoPierCam Viewer** and **Start AutoPierCam**.

## What is installed

Application files are installed for the current user at:

```text
%LOCALAPPDATA%\Programs\AutoPierCam
```

`autopiercam.exe`, `autopiercam-tray.exe`, and the pinned ZWO
`ASICamera2.dll` are adjacent in that directory. The complete self-contained
WinUI application is under `Viewer\`; no separate .NET runtime installation is
required. Apache and third-party license material is included with the payload.
The generated Rust dependency report and the Rust standard-library copyright
collection are installed as `licenses\Rust-Third-Party-Licenses.md` and
`licenses\Rust-Standard-Library-COPYRIGHT.html` alongside the ZWO, .NET, and
Windows App SDK license files.

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

## Viewer and N.I.N.A.

AutoPierCam accepts up to four independent local preview clients. The Viewer
and the separately distributed N.I.N.A. plugin can therefore remain open at the
same time, and a slow client cannot stall camera capture or the other viewers.
A fifth client waits and retries when a slot becomes available.

The N.I.N.A. plugin is not part of the MSI. With N.I.N.A. closed, extract its
archive to `%LOCALAPPDATA%\NINA\Plugins\3.0.0\AutoPierCam`; see
`integrations\AutoPierCam.NINA\README.md` in the source tree for details. It
adds a read-only **Pier Camera** panel to Imaging and never opens the camera.

## Upgrade and shutdown

Repair, upgrade, and uninstall ask the running tray to shut down through its
same-user control pipe and wait up to 30 seconds for that exact process. Windows
Installer and Restart Manager then handle any remaining locks on installed
files; setup never finds or terminates processes merely by executable name. If
setup reports files in use, stop AutoPierCam from its tray menu or run:

```powershell
autopiercam shutdown-agent --if-running --timeout-seconds 30
```

## Silent installation and diagnostics

Before installing a downloaded release, compare its published SHA-256 with:

```powershell
Get-FileHash .\AutoPierCam-0.1.0-x64.msi -Algorithm SHA256
```

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

On a clean disposable Windows account or release-test host, exercise the real
per-user lifecycle as well:

```powershell
.\scripts\Test-InstalledAutoPierCam.ps1
```

This fail-closed test refuses to touch pre-existing AutoPierCam state. It runs
default and startup-opt-out installs, verifies the tray and installed feature
states, tests both explicit and MSI-driven graceful shutdown, uninstalls, and
proves the application-created configuration survives unchanged. Test-created
user data and verbose MSI logs are atomically preserved under
`artifacts\installer\lifecycle-test-*`. Run this host-level check from an
ordinary user PowerShell rather than a layered or virtualized development
shell, so Windows Installer and the harness observe the same Local AppData
namespace.

Before a release, verify that the committed Windows-target Rust license report
matches the locked dependency graph. This check requires `cargo-about 0.9.1`:

```powershell
.\third-party\rust\Generate-Notices.ps1 -Check
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

The Apps & Features entry, both Start Menu shortcuts, the notification-area
host, and the Viewer use the same AutoPierCam camera-on-pier icon. The Viewer
keeps a local icon copy so its title bar and taskbar identity remain correct in
the unpackaged, self-contained deployment.
