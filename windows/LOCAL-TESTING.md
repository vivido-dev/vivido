# Build and verify the Vivido Windows installers locally

This guide creates the x64 MSI and consumer EXE on a Windows 11 development machine and verifies
their installation behavior. Run the consumer EXE tests in a disposable Windows VM: that bundle
can install machine-wide PowerShell, enable WSL and virtualization features, install Ubuntu, and
require a reboot. The MSI is per-user, but its tests intentionally install, repair, and uninstall
Vivido for the current account.

Locally built artifacts are unsigned unless you run the protected production signing workflow.
An unsigned MSI or EXE is suitable for local testing only. Do not distribute it under a production
filename or treat a self-signed developer certificate as a public release identity.

## 1. Prepare the Windows machine

Use Windows 11 x64 build 22000 or newer with a DirectX 12-capable GPU. Allow roughly 25 GB for the
repository, Rust outputs, the Visual Studio toolchain, vcpkg, and FFmpeg builds.

Install:

- Git for Windows;
- Visual Studio 2022 Build Tools with **Desktop development with C++**, the MSVC x64 toolset,
  CMake tools, and a Windows 11 SDK;
- PowerShell 7;
- the .NET SDK;
- rustup with the `x86_64-pc-windows-msvc` target; and
- WSL/Ubuntu only if you want to test the WSL shortcut before testing the consumer bundle.

Open **Developer PowerShell for VS 2022**, not an ordinary shell. Confirm that the required tools
resolve:

```powershell
$PSVersionTable.PSVersion
git --version
dotnet --info
rustup --version
Get-Command cl.exe, dumpbin.exe | Format-Table Name, Source
```

If `cl.exe` or `dumpbin.exe` is missing, reopen Developer PowerShell or repair the Visual Studio C++
and Windows SDK installation.

Clone the repository and make its root the current directory:

```powershell
$repositoryUrl = 'https://github.com/OWNER/REPOSITORY.git'
git clone $repositoryUrl C:\src\vivido-private
Set-Location C:\src\vivido-private
$repositoryRoot = (Get-Location).Path
$release = Get-Content vivido\windows\release.json -Raw | ConvertFrom-Json
```

Install the repository-pinned Rust toolchain:

```powershell
rustup toolchain install $release.rust_toolchain --profile minimal --component rustfmt,clippy
rustup target add x86_64-pc-windows-msvc --toolchain $release.rust_toolchain
rustup override set $release.rust_toolchain
```

## 2. Install the pinned vcpkg and FFmpeg inputs

Use a dedicated vcpkg checkout. `prepare-release.ps1` rejects a different commit, even when it is
newer:

```powershell
$vcpkgRoot = 'C:\src\vcpkg-vivido'
git clone --filter=blob:none https://github.com/microsoft/vcpkg.git $vcpkgRoot
git -C $vcpkgRoot checkout $release.vcpkg_commit
& "$vcpkgRoot\bootstrap-vcpkg.bat" -disableMetrics

$env:VCPKG_ROOT = $vcpkgRoot
$env:VCPKG_DEFAULT_TRIPLET = 'x64-windows'
& "$vcpkgRoot\vcpkg.exe" install `
    --triplet x64-windows `
    --x-manifest-root="$repositoryRoot\vivido\windows"

$vcpkgBin = "$vcpkgRoot\installed\x64-windows\bin"
$env:PATH = "$vcpkgBin;$env:PATH"
```

The manifest disables FFmpeg default features and enables only `avcodec`, `avformat`,
`swresample`, and `swscale`. Do not add GPL, nonfree, FDK-AAC, or unrelated features when testing
a prospective public package.

Verify the checkout and resolved package set:

```powershell
git -C $vcpkgRoot rev-parse HEAD
& "$vcpkgRoot\vcpkg.exe" list --triplet x64-windows
```

The first command must equal `release.json`'s `vcpkg_commit`.

## 3. Run the source checks

The repository is a collection of Cargo projects, not a root workspace. Run each command from the
corresponding project directory:

```powershell
foreach ($project in @('vivid_protocol', 'vivi')) {
    Push-Location $project
    cargo fmt --all --check
    cargo test --all-targets --locked
    cargo clippy --all-targets --locked -- -D warnings
    Pop-Location
}

foreach ($project in @('vivido', 'vvmux')) {
    Push-Location $project
    cargo fmt --all --check
    cargo check --workspace --all-targets --locked
    cargo test --workspace --all-targets --locked
    cargo clippy --workspace --all-targets --locked -- -D warnings
    Pop-Location
}

Push-Location vivido\windows\setup-helper
cargo fmt --all --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
Pop-Location
```

Stop if a check fails. Do not package binaries produced from a failing tree.

## 4. Build and stage the suite

Read the suite version from Vivido's Cargo manifest, then build each project independently:

```powershell
$versionMatch = [regex]::Match(
    (Get-Content vivido\Cargo.toml -Raw),
    '(?m)^version\s*=\s*"(?<version>[^"]+)"'
)
if (-not $versionMatch.Success) { throw 'Unable to read the Vivido version' }
$version = $versionMatch.Groups['version'].Value

foreach ($project in @('vivido', 'vivi', 'vvmux')) {
    Push-Location $project
    cargo build --release --locked
    Pop-Location
}

Push-Location vivido\windows\setup-helper
cargo build --release --locked
Pop-Location

vivido\windows\scripts\prepare-release.ps1 -Version $version
```

`prepare-release.ps1` performs more than a file copy. It checks the three suite crate versions,
validates the FFmpeg feature policy, follows the recursive PE DLL dependency closure with
`dumpbin`, copies required vcpkg and app-local MSVC DLLs, collects licenses, and writes
`vivido\windows\dist\staging-manifest.json`.

Inspect the staging directory and manifest:

```powershell
Get-ChildItem vivido\windows\dist\staging -Recurse
Get-Content vivido\windows\dist\staging-manifest.json -Raw | ConvertFrom-Json |
    Select-Object version, architecture, source_revision, signed
```

There must be four commands in the staging root, the setup helper under `installer`, the complete
runtime DLL closure, `LICENSES`, and `THIRD-PARTY-NOTICES.txt`. No non-system DLL may be loaded from
the developer's vcpkg directory or PATH after installation.

## 5. Download and verify the PowerShell prerequisite

The consumer bundle embeds the exact Microsoft MSI pinned in `release.json`:

```powershell
$powershellMsi = vivido\windows\scripts\download-prerequisites.ps1
Get-FileHash -LiteralPath $powershellMsi -Algorithm SHA256
Get-AuthenticodeSignature -LiteralPath $powershellMsi |
    Format-List Status, StatusMessage, SignerCertificate, TimeStamperCertificate
```

The hash must match `release.json`, signature status must be `Valid`, and the signer must be
Microsoft. Do not re-sign this prerequisite.

## 6. Build the MSI and consumer EXE

WiX 7 requires acceptance of its applicable OSMF terms. An authorized person must review and
accept those terms before setting the acknowledgement below. The repository deliberately does not
accept legal terms automatically.

```powershell
$env:WIX_EULA_ID = 'wix7' # Set only after authorized acceptance.

vivido\windows\scripts\build-installers.ps1 `
    -Version $version `
    -Publisher 'Vivido Local Test' `
    -Target Msi

vivido\windows\scripts\build-installers.ps1 `
    -Version $version `
    -Publisher 'Vivido Local Test' `
    -Target Bundle `
    -PowerShellMsi $powershellMsi
```

Expected outputs:

```text
vivido\windows\dist\Vivido-X.Y.Z-x64.msi
vivido\windows\dist\VividoSetup-X.Y.Z-x64.exe
```

Confirm their hashes and expected unsigned state:

```powershell
$msi = (Resolve-Path "vivido\windows\dist\Vivido-$version-x64.msi").Path
$bundle = (Resolve-Path "vivido\windows\dist\VividoSetup-$version-x64.exe").Path
Get-FileHash -Algorithm SHA256 -LiteralPath $msi, $bundle
Get-AuthenticodeSignature -LiteralPath $msi, $bundle |
    Format-Table Path, Status, StatusMessage
```

For an unsigned local build, `NotSigned` is expected. Do not run
`scripts\verify-signatures.ps1` on unsigned artifacts; that script intentionally treats them as a
release failure.

## 7. Verify the MSI automatically

Use a disposable VM or test Windows account. The automated smoke test:

- silently installs the MSI;
- verifies all four commands, config creation, both shortcuts, and the user PATH;
- appends `# installer-smoke-preserve` to the current Vivido config;
- repairs the MSI and verifies the config was not replaced;
- uninstalls the MSI and verifies the config was preserved; and
- verifies binaries and the PATH entry were removed.

Because it deliberately edits the config and uninstalls the product, do not run it against a
personal Vivido installation without first backing up `%USERPROFILE%\vivido\vivido.toml`.

```powershell
$smokeLog = Join-Path $env:TEMP "vivido-msi-$version-smoke.log"
vivido\windows\scripts\test-msi.ps1 -Msi $msi -Log $smokeLog
Get-Content $smokeLog -Tail 100
```

No output from the test script means all assertions passed. The MSI log remains available for
diagnosis. Exit code `3010` is accepted and means Windows requires a reboot.

## 8. Verify an installed MSI manually

For manual inspection, install again with verbose logging:

```powershell
$installLog = Join-Path $env:TEMP "vivido-msi-$version-install.log"
$process = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
    '/i', "`"$msi`"", '/qn', '/norestart', '/log', "`"$installLog`""
)
if ($process.ExitCode -notin @(0, 3010)) {
    throw "MSI install failed with $($process.ExitCode); see $installLog"
}
```

Open a new PowerShell process after installation so it receives the updated user PATH, then run:

```powershell
$installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\Vivido'
Get-ChildItem $installDirectory

& "$installDirectory\vivido.exe" --version
& "$installDirectory\vivi.exe" --version
& "$installDirectory\vvmux.exe" --version
& "$installDirectory\vvssh.exe" --version

Get-Command vivido, vivi, vvmux, vvssh | Format-Table Name, Source
Get-Content "$env:USERPROFILE\vivido\vivido.toml"
```

Inspect the shortcuts without launching them:

```powershell
$shortcutDirectory = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Vivido'
$shell = New-Object -ComObject WScript.Shell
Get-ChildItem $shortcutDirectory -Filter *.lnk | ForEach-Object {
    $shortcut = $shell.CreateShortcut($_.FullName)
    [pscustomobject]@{
        Name = $_.Name
        Target = $shortcut.TargetPath
        Arguments = $shortcut.Arguments
        WorkingDirectory = $shortcut.WorkingDirectory
    }
} | Format-List
```

The target must be the absolute installed `vivido.exe`. Arguments must be exactly `-e pwsh.exe`
and `-e wsl.exe`, and the working directory must be `%USERPROFILE%`.

Launch **Vivido PowerShell** from the Start Menu and confirm Vivido opens with PowerShell 7. Launch
**Vivido WSL** and confirm the default WSL distribution opens. On Ubuntu's first launch, complete
the required Linux username and password prompt.

For a functional suite smoke test:

```powershell
vvmux new -d -s installer-test
vvmux list
vvmux attach -t installer-test
# Detach with the default Ctrl-b, d binding, then:
vvmux kill-session -t installer-test
```

With Vivido running, use `vivi` to display a known image, then test a video containing linked
audio. Run `vvssh --help`; perform the remote-forwarding test only against a controlled SSH host.

## 9. Verify preservation and refusal behavior

Run these cases only in a disposable VM or test account.

### Legacy config migration

Before installation, ensure the new config does not exist and place a recognizable TOML file at
`%APPDATA%\vivido\vivido.toml`. Install the MSI and compare the two files:

```powershell
$legacyConfig = Join-Path $env:APPDATA 'vivido\vivido.toml'
$newConfig = Join-Path $env:USERPROFILE 'vivido\vivido.toml'
Get-FileHash -Algorithm SHA256 -LiteralPath $legacyConfig, $newConfig
```

The hashes must match. Repair and uninstall must leave the new file unchanged.

### Live vvmux uninstall refusal

Create a detached session, then attempt uninstall:

```powershell
vvmux new -d -s uninstall-guard-test
$blocked = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
    '/x', "`"$msi`"", '/qn', '/norestart', '/log', "`"$env:TEMP\vivido-blocked-uninstall.log`""
)
if ($blocked.ExitCode -eq 0) { throw 'Uninstall unexpectedly succeeded with a live vvmux session' }

vvmux kill-session -t uninstall-guard-test
```

The first uninstall must fail and its log must instruct the user to list and kill live sessions.
After killing the session, uninstall should succeed.

### Upgrade and downgrade

Use signed or unsigned MSI artifacts built from two real release tags; do not edit an MSI's version
table manually. Install the older version, edit the user config, then install the newer version.
Confirm the config and vvmux data remain unchanged. Attempt to reinstall the older MSI and confirm
Windows Installer rejects the downgrade. A same-version package with a different ProductCode must
also be rejected rather than silently replacing the installed product.

## 10. Test the consumer EXE

Take a VM snapshot first. To exercise the intended consumer path, start from Windows 11 with no
PowerShell 7, no enabled WSL feature, and no installed distribution:

```powershell
$bundleLog = Join-Path $env:TEMP "vivido-bundle-$version.log"
$process = Start-Process -FilePath $bundle -Wait -PassThru -ArgumentList @(
    '/quiet', '/norestart', '/log', "`"$bundleLog`""
)
$process.ExitCode
```

Expected results:

- `0`: setup completed and no reboot is required;
- `3010`: setup enabled WSL-related features and a reboot is required; or
- any other code: setup failed—inspect the Burn log and its package-specific companion logs.

If the result is `3010`, reboot the VM. Burn should resume, finish Ubuntu provisioning without
launching it prematurely, and install the suite. At the successful finish page of an interactive
run, use the launch option to complete Ubuntu's first-run username setup inside Vivido.

Repeat from VM snapshots for:

- supported PowerShell 7 already installed;
- WSL with an existing default distribution;
- WSL enabled with no distribution;
- network disconnected;
- virtualization disabled or blocked by policy; and
- cancelled elevation.

An existing supported PowerShell installation and existing default WSL distribution must remain
unchanged. Uninstalling Vivido must not remove PowerShell, WSL, Ubuntu, Vivido config, or vvmux
data.

## 11. Verify production-signed artifacts locally

Production signing is performed by the protected GitHub workflow through Microsoft Artifact
Signing. Copy the resulting MSI and EXE to the test machine; do not substitute an exported PFX.

Set the exact publisher string used by the validated certificate profile:

```powershell
$msi = (Resolve-Path 'C:\release\Vivido-X.Y.Z-x64.msi').Path
$bundle = (Resolve-Path 'C:\release\VividoSetup-X.Y.Z-x64.exe').Path
$expectedPublisher = '<VALIDATED LEGAL PUBLISHER>'
vivido\windows\scripts\verify-signatures.ps1 `
    -Path @($msi, $bundle) `
    -ExpectedPublisher $expectedPublisher
```

After installing the signed MSI, verify every shipped PE file as well:

```powershell
$installDirectory = Join-Path $env:LOCALAPPDATA 'Programs\Vivido'
vivido\windows\scripts\verify-signatures.ps1 `
    -Path $installDirectory `
    -ExpectedPublisher $expectedPublisher
```

The script requires a valid Authenticode chain, matching publisher, trusted timestamp, and a clean
`signtool.exe verify /pa /all /v` result. Also inspect the signatures directly:

```powershell
Get-AuthenticodeSignature -LiteralPath $msi, $bundle |
    Format-List Path, Status, StatusMessage, SignerCertificate, TimeStamperCertificate
```

Disconnect the VM from the network after Windows has cached the public certificate chain and repeat
verification. Valid RFC 3161 timestamps must keep the signatures valid after certificate expiry.

Finally, compare the downloaded artifacts with the published checksum file:

```powershell
Get-FileHash -Algorithm SHA256 -LiteralPath $msi, $bundle
Get-Content .\SHA256SUMS.txt
```

Do not approve a release if any publisher, timestamp, hash, or inner PE signature differs from the
release candidate.

## 12. Uninstall and final cleanup verification

Close all Vivido windows and kill every vvmux session before uninstalling:

```powershell
vvmux list
# For every listed session:
vvmux kill-session -t NAME

$uninstallLog = Join-Path $env:TEMP "vivido-msi-$version-uninstall.log"
$process = Start-Process msiexec.exe -Wait -PassThru -ArgumentList @(
    '/x', "`"$msi`"", '/qn', '/norestart', '/log', "`"$uninstallLog`""
)
if ($process.ExitCode -notin @(0, 3010)) {
    throw "MSI uninstall failed with $($process.ExitCode); see $uninstallLog"
}
```

Confirm:

- `%LOCALAPPDATA%\Programs\Vivido` no longer contains product binaries or DLLs;
- the Vivido Start Menu folder is gone;
- a newly opened shell no longer resolves the four commands through the Vivido PATH entry;
- `%USERPROFILE%\vivido\vivido.toml` remains unchanged;
- `%APPDATA%\vvmux` and other vvmux user data remain; and
- PowerShell, WSL, and Ubuntu remain installed after uninstalling the consumer bundle.

## Troubleshooting

- **`WIX7015` or an OSMF EULA error:** an authorized publisher has not completed the WiX 7 legal
  acceptance or `WIX_EULA_ID` is missing. Do not bypass this check.
- **`dumpbin.exe` not found:** use Developer PowerShell for Visual Studio and install the Windows
  SDK plus MSVC x64 tools.
- **Pinned vcpkg error:** check `git -C $env:VCPKG_ROOT rev-parse HEAD` against `release.json`.
- **Missing DLL at runtime:** rerun `prepare-release.ps1`; do not fix the package by adding a
  developer vcpkg directory to the installed machine's PATH.
- **PowerShell MSI hash/signature failure:** delete only the cached prerequisite in
  `vivido\windows\dist\prerequisites` and rerun `download-prerequisites.ps1`. Never weaken the
  validation.
- **MSI exit code `1603`:** inspect the verbose MSI log. A live vvmux session is an intentional
  uninstall/upgrade blocker.
- **Exit code `3010`:** reboot is required. With `/norestart`, this is success-with-reboot, not a
  complete ready state.
- **Commands not found after install:** open a new shell; existing processes do not receive user
  PATH changes retroactively.
- **Unsigned/SmartScreen warning:** expected for a local build. Production candidates must come
  from the protected signing workflow and pass `verify-signatures.ps1`.
- **WSL setup failure:** inspect `wsl --status`, `wsl --list --verbose`, Windows optional features,
  virtualization firmware settings, policy restrictions, network access, and the Burn logs.
