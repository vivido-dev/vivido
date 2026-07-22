[CmdletBinding()]
param(
    [string]$SourceRoot = (Split-Path -Parent $PSScriptRoot),
    [string]$OutputDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist\prerequisites')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$release = Get-Content -LiteralPath (Join-Path $SourceRoot 'release.json') -Raw | ConvertFrom-Json
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$destination = Join-Path $OutputDirectory $release.powershell.file

if (-not (Test-Path -LiteralPath $destination -PathType Leaf) -or
    (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ne $release.powershell.sha256) {
    Invoke-WebRequest -Uri $release.powershell.url -OutFile $destination
}
if ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ne $release.powershell.sha256) {
    throw 'PowerShell MSI SHA-256 does not match release.json'
}
$signature = Get-AuthenticodeSignature -LiteralPath $destination
if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'Microsoft') {
    throw "PowerShell MSI does not have the expected valid Microsoft signature: $($signature.Status)"
}
Write-Output $destination

