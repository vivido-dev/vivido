[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string[]]$Path,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$ExpectedPublisher
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Resolve-SignToolPath {
    $command = Get-Command signtool.exe -CommandType Application -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    $sdkBinRoots = @(
        [Environment]::GetEnvironmentVariable('WindowsSdkVerBinPath'),
        (Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFilesX86)) 'Windows Kits\10\bin'),
        (Join-Path ([Environment]::GetFolderPath([Environment+SpecialFolder]::ProgramFiles)) 'Windows Kits\10\bin')
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_ -PathType Container) } | Select-Object -Unique

    $candidates = foreach ($sdkBinRoot in $sdkBinRoots) {
        $directCandidate = Join-Path $sdkBinRoot 'x64\signtool.exe'
        if (Test-Path -LiteralPath $directCandidate -PathType Leaf) {
            [pscustomobject]@{ Version = [version]'0.0'; Path = $directCandidate }
        }

        foreach ($versionDirectory in Get-ChildItem -LiteralPath $sdkBinRoot -Directory -ErrorAction SilentlyContinue) {
            $version = $null
            if (-not [version]::TryParse($versionDirectory.Name, [ref]$version)) { continue }

            $candidate = Join-Path $versionDirectory.FullName 'x64\signtool.exe'
            if (Test-Path -LiteralPath $candidate -PathType Leaf) {
                [pscustomobject]@{ Version = $version; Path = $candidate }
            }
        }
    }

    $selected = $candidates | Sort-Object Version -Descending | Select-Object -First 1
    if (-not $selected) {
        throw 'signtool.exe was not found on PATH or in an installed x64 Windows 10 SDK bin directory'
    }
    return $selected.Path
}

$signtool = Resolve-SignToolPath
foreach ($item in $Path) {
    $files = if (Test-Path -LiteralPath $item -PathType Container) {
        Get-ChildItem -LiteralPath $item -File -Recurse | Where-Object Extension -in @('.exe', '.dll', '.msi')
    } else {
        Get-Item -LiteralPath $item
    }
    foreach ($file in $files) {
        $signature = Get-AuthenticodeSignature -LiteralPath $file.FullName
        if ($signature.Status -ne 'Valid') { throw "Invalid signature on $($file.FullName): $($signature.Status)" }
        if (-not $signature.TimeStamperCertificate) { throw "Signature has no trusted timestamp: $($file.FullName)" }
        if ($signature.SignerCertificate.Subject -notmatch [regex]::Escape($ExpectedPublisher)) {
            throw "Unexpected publisher on $($file.FullName): $($signature.SignerCertificate.Subject)"
        }
        & $signtool verify /pa /all /v $file.FullName | Out-Host
        if ($LASTEXITCODE -ne 0) { throw "SignTool verification failed: $($file.FullName)" }
    }
}
