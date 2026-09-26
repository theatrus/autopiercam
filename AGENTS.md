# AutoPierCam project conventions

- Commit as Yann Ramin <github@theatr.us>.
- Every released NINA plugin must be published to `theatrus/nina-plugins-registry`,
  at `manifests/a/AutoPierCam/3.2.0.9001/manifest.json` while that minimum NINA
  version applies. A GitHub ZIP alone is not a complete plugin release.
- That registry's `Publish AutoPierCam plugin` workflow verifies and synchronizes
  stable releases on a schedule. Dispatch it after publishing for prompt delivery.
  Verify the released version and checksum on both public `manifests.json` endpoints:
  `https://nina-plugins.psf-guard.com/` and `https://nina-plugins.pulsarfab.com/`.
- Follow [docs/release.md](docs/release.md) to cut a release, including the
  plugin contract test's `new Version(...)` assertion.
- Do not replace published release assets or suppress checksum/signature failures.
- Camera enumeration and SDK operations belong to the capture thread. IPC and
  Viewer code must not open another camera handle. Never silently select a camera
  when several match; require an explicit operator selection.
- Automated tests must not open physical cameras or send live images/messages.
