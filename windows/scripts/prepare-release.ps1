[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$Version,
    [string]$RepositoryRoot = (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))),
    [string]$VcpkgRoot = $env:VCPKG_ROOT,
    [string]$OutputDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($VcpkgRoot)) {
    throw 'VCPKG_ROOT is required'
}

$vcpkgManifestPath = Join-Path $RepositoryRoot 'vivido\windows\vcpkg.json'
$vcpkgManifest = Get-Content -LiteralPath $vcpkgManifestPath -Raw | ConvertFrom-Json
$release = Get-Content -LiteralPath (Join-Path $RepositoryRoot 'vivido\windows\release.json') -Raw | ConvertFrom-Json
if ($release.vcpkg_commit -cne $vcpkgManifest.'builtin-baseline') {
    throw 'release.json vcpkg commit and vcpkg.json builtin baseline do not match'
}
$actualVcpkgCommit = (& git -C $VcpkgRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $actualVcpkgCommit -cne $release.vcpkg_commit) {
    throw "VCPKG_ROOT is not checked out at the pinned commit $($release.vcpkg_commit)"
}
$ffmpegDependency = @($vcpkgManifest.dependencies | Where-Object name -eq 'ffmpeg')
if ($ffmpegDependency.Count -ne 1 -or $ffmpegDependency[0].'default-features' -ne $false) {
    throw 'vcpkg.json must contain exactly one FFmpeg dependency with default features disabled'
}
$expectedFfmpegFeatures = @('avcodec', 'avformat', 'swresample', 'swscale')
$actualFfmpegFeatures = @($ffmpegDependency[0].features | Sort-Object)
if (Compare-Object $expectedFfmpegFeatures $actualFfmpegFeatures) {
    throw 'FFmpeg features must be exactly avcodec, avformat, swresample, and swscale'
}
$forbiddenFfmpegFeatures = @('gpl', 'all-gpl', 'nonfree', 'fdk-aac')
if ($actualFfmpegFeatures | Where-Object { $_ -in $forbiddenFfmpegFeatures }) {
    throw 'A prohibited FFmpeg feature is enabled'
}

function Get-CargoPackageVersion([string]$Manifest) {
    $inPackage = $false
    foreach ($line in Get-Content -LiteralPath $Manifest) {
        if ($line -match '^\s*\[package\]\s*$') { $inPackage = $true; continue }
        if ($inPackage -and $line -match '^\s*\[') { break }
        if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
    }
    throw "Package version not found in $Manifest"
}

foreach ($project in @('vivido', 'vivi', 'vvmux')) {
    $manifest = Join-Path $RepositoryRoot "$project\Cargo.toml"
    $crateVersion = Get-CargoPackageVersion $manifest
    if ($crateVersion -ne $Version) {
        throw "$project version $crateVersion does not match release $Version"
    }
}

$stage = Join-Path $OutputDirectory 'staging'
if (Test-Path -LiteralPath $stage) {
    Remove-Item -LiteralPath $stage -Recurse
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stage 'installer') -Force | Out-Null
New-Item -ItemType Directory -Path (Join-Path $stage 'LICENSES') -Force | Out-Null

$binarySources = @{
    'vivido.exe' = Join-Path $RepositoryRoot 'vivido\target\release\vivido.exe'
    'vvssh.exe' = Join-Path $RepositoryRoot 'vivido\target\release\vvssh.exe'
    'vivi.exe' = Join-Path $RepositoryRoot 'vivi\target\release\vivi.exe'
    'vvmux.exe' = Join-Path $RepositoryRoot 'vvmux\target\release\vvmux.exe'
}
foreach ($entry in $binarySources.GetEnumerator()) {
    if (-not (Test-Path -LiteralPath $entry.Value -PathType Leaf)) {
        throw "Release binary not found: $($entry.Value)"
    }
    Copy-Item -LiteralPath $entry.Value -Destination (Join-Path $stage $entry.Key)
}

$helper = Join-Path $RepositoryRoot 'vivido\windows\setup-helper\target\release\vivido-windows-setup.exe'
if (-not (Test-Path -LiteralPath $helper -PathType Leaf)) {
    throw "Setup helper not found: $helper"
}
Copy-Item -LiteralPath $helper -Destination (Join-Path $stage 'installer\vivido-windows-setup.exe')

foreach ($project in @('vivido', 'vivi', 'vvmux')) {
    Copy-Item -LiteralPath (Join-Path $RepositoryRoot "$project\LICENSE") `
        -Destination (Join-Path $stage "LICENSES\$project-LICENSE.txt")
}

$vcpkgBin = Join-Path $VcpkgRoot 'installed\x64-windows\bin'
if (-not (Test-Path -LiteralPath $vcpkgBin -PathType Container)) {
    throw "vcpkg runtime directory not found: $vcpkgBin"
}
$system32 = Join-Path $env:SystemRoot 'System32'
$seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$queue = [Collections.Generic.Queue[string]]::new()
foreach ($name in $binarySources.Keys) { $queue.Enqueue((Join-Path $stage $name)) }
$queue.Enqueue((Join-Path $stage 'installer\vivido-windows-setup.exe'))

function Find-VcRuntime([string]$Name) {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere -PathType Leaf)) { return $null }
    $installation = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if (-not $installation) { return $null }
    return Get-ChildItem -LiteralPath (Join-Path $installation 'VC\Redist\MSVC') -Filter $Name -File -Recurse |
        Where-Object { $_.FullName -match '\\x64\\Microsoft\.VC[^\\]+\.CRT\\' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1 -ExpandProperty FullName
}

while ($queue.Count -ne 0) {
    $pe = $queue.Dequeue()
    $key = [IO.Path]::GetFullPath($pe)
    if (-not $seen.Add($key)) { continue }

    $dump = & dumpbin.exe /nologo /dependents $pe 2>&1
    if ($LASTEXITCODE -ne 0) { throw "dumpbin failed for ${pe}: $dump" }
    $dependencies = $dump | ForEach-Object {
        if ($_ -match '^\s+([A-Za-z0-9._+-]+\.dll)\s*$') { $Matches[1] }
    } | Sort-Object -Unique

    foreach ($dependency in $dependencies) {
        $stagedDependency = Join-Path $stage $dependency
        if (Test-Path -LiteralPath $stagedDependency -PathType Leaf) {
            $queue.Enqueue($stagedDependency)
            continue
        }

        $vcpkgDependency = Join-Path $vcpkgBin $dependency
        if (Test-Path -LiteralPath $vcpkgDependency -PathType Leaf) {
            Copy-Item -LiteralPath $vcpkgDependency -Destination $stagedDependency
            $queue.Enqueue($stagedDependency)
            continue
        }

        if ($dependency -match '^(?i:VCRUNTIME|MSVCP|CONCRT).*\.dll$') {
            $runtime = Find-VcRuntime $dependency
            if (-not $runtime) { throw "Visual C++ app-local runtime not found: $dependency" }
            Copy-Item -LiteralPath $runtime -Destination $stagedDependency
            $queue.Enqueue($stagedDependency)
            continue
        }

        if ($dependency -match '^(?i:api-ms-win-|ext-ms-win-)') { continue }
        if (Test-Path -LiteralPath (Join-Path $system32 $dependency) -PathType Leaf) { continue }
        throw "Non-system dependency could not be staged for ${pe}: $dependency"
    }
}

$notice = Join-Path $stage 'THIRD-PARTY-NOTICES.txt'
Copy-Item -LiteralPath (Join-Path $RepositoryRoot 'vivido\windows\THIRD-PARTY-NOTICES.header.txt') -Destination $notice
$vcpkgExe = Join-Path $VcpkgRoot 'vcpkg.exe'
$resolvedPackages = & $vcpkgExe list --triplet x64-windows
if ($LASTEXITCODE -ne 0) { throw 'Unable to record the resolved vcpkg packages' }
Add-Content -LiteralPath $notice -Value @"

--- Windows release provenance ---
vcpkg baseline: $($vcpkgManifest.'builtin-baseline')
triplet: x64-windows (dynamic libraries)
FFmpeg source repository: https://github.com/FFmpeg/FFmpeg
FFmpeg enabled components: $($actualFfmpegFeatures -join ', ')

Resolved vcpkg packages:
$($resolvedPackages -join "`r`n")
"@

$ffmpegPortDirectory = Join-Path $VcpkgRoot 'ports\ffmpeg'
$ffmpegPortfile = Join-Path $ffmpegPortDirectory 'portfile.cmake'
if (-not (Test-Path -LiteralPath $ffmpegPortfile -PathType Leaf)) {
    throw 'Pinned FFmpeg vcpkg port recipe is missing'
}
Add-Content -LiteralPath $notice -Value "`r`nFFmpeg pinned vcpkg port recipe (source ref, archive hash, and configuration):`r`n"
Get-Content -LiteralPath $ffmpegPortfile | Add-Content -LiteralPath $notice
$patches = Get-ChildItem -LiteralPath $ffmpegPortDirectory -Filter '*.patch' -File | Sort-Object Name
Add-Content -LiteralPath $notice -Value "`r`nFFmpeg vcpkg patches and SHA-256 digests:`r`n"
foreach ($patch in $patches) {
    Add-Content -LiteralPath $notice -Value "$($patch.Name)  $((Get-FileHash -LiteralPath $patch.FullName -Algorithm SHA256).Hash)"
}

$share = Join-Path $VcpkgRoot 'installed\x64-windows\share'
foreach ($copyright in Get-ChildItem -LiteralPath $share -Filter copyright -File -Recurse | Sort-Object FullName) {
    Add-Content -LiteralPath $notice -Value "`r`n--- $($copyright.Directory.Name) ---`r`n"
    Get-Content -LiteralPath $copyright.FullName | Add-Content -LiteralPath $notice
}

$files = Get-ChildItem -LiteralPath $stage -File -Recurse | Sort-Object FullName | ForEach-Object {
    [ordered]@{
        path = [IO.Path]::GetRelativePath($stage, $_.FullName).Replace('\', '/')
        sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash
        bytes = $_.Length
    }
}
[ordered]@{
    version = $Version
    architecture = 'x64'
    source_revision = (& git -C $RepositoryRoot rev-parse HEAD).Trim()
    signed = $false
    files = @($files)
} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'staging-manifest.json') -Encoding utf8

Write-Output $stage
