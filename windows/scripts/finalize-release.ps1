[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$Version,
    [string]$RepositoryRoot = (Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))),
    [string]$OutputDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$artifacts = @(
    Join-Path $OutputDirectory "Vivido-$Version-x64.msi"
    Join-Path $OutputDirectory "VividoSetup-$Version-x64.exe"
    Join-Path $OutputDirectory 'staging\THIRD-PARTY-NOTICES.txt'
)
foreach ($artifact in $artifacts) {
    if (-not (Test-Path -LiteralPath $artifact -PathType Leaf)) { throw "Release artifact missing: $artifact" }
}

$fileRecords = Get-ChildItem -LiteralPath (Join-Path $OutputDirectory 'staging') -File -Recurse |
    Sort-Object FullName | ForEach-Object {
        [ordered]@{
            fileName = [IO.Path]::GetRelativePath((Join-Path $OutputDirectory 'staging'), $_.FullName).Replace('\', '/')
            SPDXID = "SPDXRef-File-$([guid]::NewGuid().ToString('N'))"
            checksums = @([ordered]@{ algorithm = 'SHA256'; checksumValue = (Get-FileHash $_.FullName -Algorithm SHA256).Hash })
        }
    }
$spdx = [ordered]@{
    spdxVersion = 'SPDX-2.3'
    dataLicense = 'CC0-1.0'
    SPDXID = 'SPDXRef-DOCUMENT'
    name = "Vivido-$Version-Windows-x64"
    documentNamespace = "https://vivido.dev/spdx/windows/$Version/$([guid]::NewGuid())"
    creationInfo = [ordered]@{ created = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ'); creators = @('Tool: vivido-windows-release') }
    files = @($fileRecords)
}
$spdxPath = Join-Path $OutputDirectory "Vivido-$Version-x64.spdx.json"
$spdx | ConvertTo-Json -Depth 7 | Set-Content -LiteralPath $spdxPath -Encoding utf8
$artifacts += $spdxPath

$checksums = foreach ($artifact in $artifacts) {
    "{0}  {1}" -f (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLowerInvariant(), (Split-Path -Leaf $artifact)
}
$checksums | Set-Content -LiteralPath (Join-Path $OutputDirectory 'SHA256SUMS.txt') -Encoding ascii

[ordered]@{
    version = $Version
    architecture = 'x64'
    sourceRevision = (& git -C $RepositoryRoot rev-parse HEAD).Trim()
    generatedUtc = [DateTime]::UtcNow.ToString('o')
    artifacts = @($artifacts | ForEach-Object {
        [ordered]@{ name = Split-Path -Leaf $_; sha256 = (Get-FileHash $_ -Algorithm SHA256).Hash; bytes = (Get-Item $_).Length }
    })
} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $OutputDirectory 'release-manifest.json') -Encoding utf8
