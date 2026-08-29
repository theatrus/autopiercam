# Rust third-party notice generation

`Rust-Third-Party-Licenses.md` is a deterministic license bundle for the
locked workspace's `x86_64-pc-windows-msvc` normal dependency graph. It
excludes development dependencies, build-only tools, and dependencies selected
only for other targets.

Install the pinned generator once:

```powershell
cargo install --locked --version 0.9.1 --features cli cargo-about
```

Generate the report from the repository root:

```powershell
pwsh -File .\third-party\rust\Generate-Notices.ps1
```

Verify that the committed report is current without changing it:

```powershell
pwsh -File .\third-party\rust\Generate-Notices.ps1 -Check
```

The generator runs `cargo-about` with `--frozen`, the Windows target filter,
and build/development dependency exclusions from `about.toml`. It independently
compares the selected package set with this Cargo graph:

```powershell
cargo tree --frozen --workspace --target x86_64-pc-windows-msvc --edges normal
```

It also rejects generic-license fallbacks for registry crates and fails if a
selected crate packages a NOTICE, COPYRIGHT, AUTHORS, or SPDX attribution file
that is absent from the report. Checksum-backed clarifications in `about.toml`
make ambiguous upstream license layouts fail closed after dependency updates.
