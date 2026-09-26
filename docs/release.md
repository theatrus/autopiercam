# Releasing AutoPierCam

A release is a version bump merged to `main`, then a `vX.Y.Z` tag on that
commit. The tag runs [`release.yml`](../.github/workflows/release.yml), which
builds, signs and publishes the MSI and the N.I.N.A. plugin. A release is not
complete until the plugin appears on both public N.I.N.A. registries.

## 1. Bump the version

`Cargo.toml` (`[workspace.package] version`) is the source of truth. The
workflow fails if the tag and this version differ. Change every copy of the
old version, keeping dependency versions that happen to match it:

| File | Form |
| --- | --- |
| `Cargo.toml` | `version = "X.Y.Z"` in `[workspace.package]` |
| `Cargo.lock` | refreshed by any `cargo` command (workspace crates only) |
| `apps/AutoPierCam.Viewer/AutoPierCam.Viewer.csproj` | `Version`, `AssemblyVersion`, `FileVersion` (`X.Y.Z.0`) |
| `apps/AutoPierCam.Viewer/app.manifest` | `assemblyIdentity version="X.Y.Z.0"` |
| `integrations/AutoPierCam.NINA/src/AutoPierCam.NINA/AutoPierCam.NINA.csproj` | `Version` (`X.Y.Z.0`) |
| `integrations/AutoPierCam.NINA/build-nina-package.ps1` | default `$Version` (`X.Y.Z.0`) |
| `integrations/AutoPierCam.NINA/tests/AutoPierCam.NINA.Tests/PluginContractTests.cs` | `new Version(X, Y, Z, 0)` |
| `integrations/AutoPierCam.NINA/README.md` | packaging example |
| `README.md` | version line, download link, release link, summary paragraph |
| `docs/architecture.md`, `docs/installation.md` | current version and MSI file names |

### Update the plugin contract test

`AssemblyCarriesPermanentIdentifierAndNinaMinimumVersion` in
`PluginContractTests.cs` asserts the plugin's assembly version:

```csharp
Assert.Equal(new Version(0, 2, 6, 0), assembly.GetName().Version);
```

Change it to the new version. It is written as numbers, not a dotted string,
so a search for the old version misses it. The test only runs on Windows CI,
so a missed update shows up as a failed **Windows capture, sharing and
Viewer** job on the release PR (as it did for 0.2.6).

### Check for strays

Search for both forms of the old version. The second catches the test's
`new Version(...)` form:

```sh
OLD=0.2.5
git grep -n -F "$OLD" -- ':!Cargo.lock' ':!docs/releases' ':!docs/release.md' ':!third-party'
git grep -n -E "$(echo "$OLD" | sed 's/\./,\\s*/g'),\s*0\)" -- ':!docs/release.md'
```

The first search also lists dependencies that share the number (for example
`tracing-appender`) and the README's history of earlier versions; leave those.

## 2. Write the release notes

Add `docs/releases/X.Y.Z.md`. The workflow refuses to run without it and uses
it as the GitHub release text. Follow the previous file: what changed, whether
the update is needed, install and upgrade notes, and the plugin's registry and
minimum N.I.N.A. version.

## 3. Merge through a pull request

Open a PR named `Release AutoPierCam X.Y.Z`. Wait for both CI jobs, including
**Windows capture, sharing and Viewer**, which runs the N.I.N.A. and Viewer
tests. Squash-merge it.

## 4. Tag the merge commit

```sh
git checkout main && git pull --ff-only
git tag vX.Y.Z
git push origin vX.Y.Z
```

The `release` job then:

1. checks the tag against `Cargo.toml` and that the release notes exist;
2. runs the Rust, N.I.N.A., Viewer, FFmpeg and license checks;
3. stages the payload, signs the four programs and the plugin DLL through
   Azure Trusted Signing, packages the MSI and plugin, and signs the MSI;
4. verifies all six signatures chain to **StackFoundry LLC**;
5. installs and tests the signed MSI;
6. creates the GitHub release with the assets and checksums.

The `registry` job then waits up to 30 minutes for both
`https://nina-plugins.psf-guard.com/manifests.json` and
`https://nina-plugins.pulsarfab.com/manifests.json` to list this plugin with
the released URL, checksum and version.

The `Publish AutoPierCam plugin` workflow in
`theatrus/nina-plugins-registry` publishes stable releases on a schedule, to
`manifests/a/AutoPierCam/3.2.0.9001/manifest.json` while that minimum N.I.N.A.
version applies. Dispatch it right after the GitHub release appears to publish
without waiting:

```sh
gh workflow run "Publish AutoPierCam plugin" -R theatrus/nina-plugins-registry
```

## 5. Confirm

- The GitHub release has the MSI, plugin archive, plugin manifest and checksums.
- The `registry` job passed, or both `manifests.json` endpoints show the new
  version and checksum.
- The README download link works.

## When something fails

- **Before the GitHub release exists** (tests, signing, packaging): fix on
  `main` through a PR, delete the tag locally and on GitHub, and tag the new
  commit. Nothing was published.
- **Signing fails at Azure login**: the OIDC subject must match the Entra
  federated credential for `environment: release`. The **Signing smoke test**
  workflow checks this in about two minutes.
- **After the GitHub release exists**: never replace or re-upload its assets,
  and never skip checksum or signature failures. The workflow refuses to
  overwrite an existing release. Ship the fix as a new patch version.
- **The `registry` job times out**: the assets are public. Dispatch the
  registry's publish workflow, check its result, then rerun only the failed
  `registry` job.

`workflow_dispatch` with `publish` off builds and signs without releasing. Use
it to test the pipeline.
