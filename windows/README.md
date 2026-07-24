# Vivido Suite Windows release

The Windows release produces two x64 artifacts for Windows 11 build 22000 or newer:

- `VividoSetup-X.Y.Z-x64.exe` is the consumer Burn bundle. It installs PowerShell 7 LTS when
  necessary, provisions WSL with Ubuntu, and installs the suite MSI.
- `Vivido-X.Y.Z-x64.msi` is the per-user suite package for managed environments where PowerShell
  7 and WSL are already provisioned.

Both packages install `vivido`, `vivi`, `vvmux`, and `vvssh` below
`%LOCALAPPDATA%\Programs\Vivido`. The MSI adds that directory to the user PATH and creates the
`Vivido PowerShell` (`-e pwsh.exe`) and `Vivido WSL` (`-e wsl.exe`) Start Menu shortcuts. It does
not create desktop shortcuts or taskbar pins.

For a start-to-finish developer-machine procedure, including unsigned builds, installation,
repair, uninstall, bundle prerequisite testing, and signed-artifact verification, see
[`LOCAL-TESTING.md`](LOCAL-TESTING.md).

## Reproducible inputs

`release.json` pins Rust, vcpkg, WiX, and the unmodified Microsoft PowerShell MSI. `vcpkg.json`
selects only FFmpeg `avcodec`, `avformat`, `swresample`, and `swscale` with default features
disabled. Do not add `gpl`, `all-gpl`, `nonfree`, or `fdk-aac` features to the public build.

The release workflow runs Cargo independently in `vivid_protocol`, `vivi`, `vivido`, and `vvmux`;
the repository root is not a Cargo workspace. `scripts/prepare-release.ps1` rejects mismatched
crate/release versions, follows every PE import with `dumpbin`, copies vcpkg and app-local Visual
C++ runtime DLLs, and fails on an unresolved non-system DLL. It also collects vcpkg copyright files
and creates the unsigned staging manifest.

Before building the MSI, `scripts/generate-staging-fragment.ps1` converts that audited directory
into deterministic WiX components. Every staged file uses an HKCU registry key path, which keeps
the package valid for strict per-user installation and lets Windows Installer remove only product
files and component markers on uninstall.

To reproduce an unsigned build in a Visual Studio Developer PowerShell:

```powershell
$env:VCPKG_ROOT = 'C:\src\vcpkg'
$env:VCPKG_DEFAULT_TRIPLET = 'x64-windows'
& $env:VCPKG_ROOT\vcpkg.exe install --triplet x64-windows --x-manifest-root="$PWD\vivido\windows"

foreach ($project in @('vivido', 'vivi', 'vvmux')) {
    Push-Location $project
    cargo build --release --locked
    Pop-Location
}
Push-Location vivido\windows\setup-helper
cargo build --release --locked
Pop-Location

vivido\windows\scripts\prepare-release.ps1 -Version 0.1.2
$powershellMsi = vivido\windows\scripts\download-prerequisites.ps1
$env:WIX_EULA_ID = 'wix7' # Only after the publisher has accepted the WiX 7 OSMF EULA.
vivido\windows\scripts\build-installers.ps1 -Version 0.1.2 -Publisher 'TEST ONLY' -Target Msi
vivido\windows\scripts\build-installers.ps1 -Version 0.1.2 -Publisher 'TEST ONLY' -Target Bundle -PowerShellMsi $powershellMsi
```

Unsigned locally built packages are test inputs only and must never use production filenames on a
public release.

WiX 7 enforces its Open Source Maintenance Fee EULA at build time. An authorized publisher must
review the [WiX OSMF terms](https://www.firegiant.com/wixtoolset/osmf/) and complete any required
acceptance or payment. Only then set the protected `windows-release` environment variable
`WIX_EULA_ID` to the exact value `wix7`. The workflow and local build script fail closed when this
acknowledgement is absent; the repository does not record acceptance automatically.

## Configuration ownership and upgrades

Vivido now looks first for `%USERPROFILE%\vivido\vivido.toml`, then falls back to the former
`%APPDATA%\vivido` TOML location. On a first MSI install the signed setup helper:

1. leaves an existing new-path TOML untouched;
2. copies the legacy roaming TOML when only that file exists; or
3. writes the default PowerShell configuration when neither exists.

The config is user-owned rather than an MSI file component, so repair, major upgrade, and uninstall
do not remove or overwrite it. Uninstall and upgrade also call `vvmux list` and refuse to continue
while a live session exists. The user must run `vvmux kill-session -t NAME`; setup never kills a
session silently.

MSI examples:

```powershell
msiexec.exe /i Vivido-X.Y.Z-x64.msi /quiet /norestart /log vivido-install.log
msiexec.exe /x Vivido-X.Y.Z-x64.msi /quiet /norestart /log vivido-uninstall.log
```

Exit code `3010` means a reboot is required. The Burn bundle resumes after reboot. Ubuntu still
requires its normal first-launch Linux username creation; the bundle's finish-page launch option
opens `vivido.exe -e wsl.exe` for that step.

## Production signing enrollment

The workflow supports Microsoft Artifact Signing Public Trust only; it deliberately has no PFX
fallback. Before the first production run:

1. Confirm that the publisher is eligible for Public Trust and has a paid Azure subscription.
   Azure billing legal name and address must match the desired Authenticode publisher.
2. Assign the responsible administrator `Artifact Signing Identity Verifier`, create the Artifact
   Signing account, submit public individual/organization validation in the Azure portal, and wait
   for approval.
3. Create a Public Trust certificate profile. Use its validated legal subject as
   `WINDOWS_SIGNING_PUBLISHER`; WiX Manufacturer and Authenticode validation use the same value.
4. Create a dedicated Entra application or user-assigned identity. Give it only
   `Artifact Signing Certificate Profile Signer` at the signing account/profile scope.
5. Add a GitHub OIDC federated credential limited to this repository's protected
   `windows-release` environment. Require manual reviewers for that environment.
6. Define these environment variables as GitHub environment variables, not private-key secrets:
   `WINDOWS_SIGNING_PUBLISHER`, `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`,
   `AZURE_SUBSCRIPTION_ID`, `ARTIFACT_SIGNING_ENDPOINT`, `ARTIFACT_SIGNING_ACCOUNT`, and
   `ARTIFACT_SIGNING_PROFILE`. Set `WIX_EULA_ID=wix7` only after the authorized WiX acceptance
   described above.
7. Add a GitHub ruleset for `refs/tags/vivido-v*` that restricts tag creation, update, and deletion
   to release maintainers. The release workflow also requires the tag to point to the checked-out
   commit and rejects any tag or crate-version mismatch.

If Public Trust eligibility is unavailable, use an OV code-signing certificate backed by a
compliant cloud HSM/signing service and adapt only the signing steps. Do not export a modern public
code-signing key into a GitHub PFX secret. A self-signed or private-trust package is not a public
release candidate.

## Signing and release order

The protected workflow uses GitHub OIDC and the official Artifact Signing action in this order:

1. sign all staged project EXEs, the setup helper, and redistributed DLLs;
2. verify each inner signature and timestamp;
3. build and sign the MSI with its embedded cabinet;
4. build the Burn EXE, detach and sign its cached engine, reattach it, and sign the final EXE;
5. verify both installers with Authenticode and `signtool /pa /all /v`;
6. install/uninstall-smoke-test the signed MSI;
7. produce SHA-256 sums, SPDX file SBOM, release manifest, and GitHub provenance; and
8. create a draft GitHub release for human approval.

The PowerShell MSI retains its Microsoft signature and is hash-checked against `release.json`; it
is never re-signed. Authenticode embeds the public signing certificate information. Never bundle a
private key, PFX, signing password, private root, or `.cer` installer action.

## Release acceptance

Before publishing the draft, test on clean Windows 11 x64 VMs rather than relying only on the
Windows Server CI runner:

- no Rust/vcpkg installation and no external runtime DLL directory on PATH;
- PowerShell/WSL absent, WSL present without a distro, and an existing default distro;
- reboot/resume, offline failure, virtualization disabled, and elevation cancellation;
- both shortcuts and exact `-e` arguments;
- DirectX 12 Vivido startup, Vivi image/video/linked-audio playback, vvmux lifecycle, and a
  controlled vvssh forwarding test;
- install, repair, upgrade, downgrade rejection, live-vvmux refusal, and uninstall preservation;
- valid publisher/timestamps for every shipped PE, MSI, cached Burn engine, and final bundle.

Signed new releases can still receive early SmartScreen reputation prompts. Keep the validated
publisher stable and submit false positives through Microsoft Security Intelligence; do not switch
identities or remove signing to work around reputation.

The `Windows signing identity smoke test` workflow runs on the first day of every month and signs a
disposable helper binary through the same OIDC identity and certificate profile. Treat failures as
release blockers and renew identity validation, role assignments, or certificate-profile access
before the next release.
