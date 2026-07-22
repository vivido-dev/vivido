[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+$')]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [ValidateNotNullOrEmpty()]
    [string]$Publisher,
    [ValidateSet('Msi', 'Bundle', 'All')]
    [string]$Target = 'All',
    [string]$SourceRoot = (Split-Path -Parent $PSScriptRoot),
    [string]$OutputDirectory = (Join-Path (Split-Path -Parent $PSScriptRoot) 'dist'),
    [string]$PowerShellMsi,
    [string]$WixEulaId = $env:WIX_EULA_ID
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$stage = Join-Path $OutputDirectory 'staging'
$helper = Join-Path $stage 'installer\vivido-windows-setup.exe'
$msi = Join-Path $OutputDirectory "Vivido-$Version-x64.msi"
$stagingFragment = Join-Path $OutputDirectory 'staging-files.generated.wxs'

if ($WixEulaId -cne 'wix7') {
    throw 'WiX 7 requires publisher acceptance of the OSMF EULA. After authorized acceptance, set WIX_EULA_ID=wix7.'
}

if ($Target -in @('Msi', 'All')) {
    & (Join-Path $PSScriptRoot 'generate-staging-fragment.ps1') `
        -StageDirectory $stage -Destination $stagingFragment
    dotnet build (Join-Path $SourceRoot 'wix\VividoPackage.wixproj') -c Release `
        -p:VividoVersion=$Version -p:Publisher="$Publisher" `
        -p:SourceRoot="$SourceRoot" -p:StageDir="$stage" -p:ArtifactsDir="$OutputDirectory\" `
        -p:StagingFragment="$stagingFragment" `
        -p:WixEulaId=$WixEulaId
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $msi -PathType Leaf)) {
        throw 'WiX MSI build failed'
    }
}

if ($Target -in @('Bundle', 'All')) {
    if ([string]::IsNullOrWhiteSpace($PowerShellMsi)) { throw 'PowerShellMsi is required for Bundle' }
    foreach ($required in @($helper, $msi, $PowerShellMsi)) {
        if (-not (Test-Path -LiteralPath $required -PathType Leaf)) { throw "Bundle input missing: $required" }
    }
    dotnet build (Join-Path $SourceRoot 'wix\VividoBundle.wixproj') -c Release `
        -p:VividoVersion=$Version -p:Publisher="$Publisher" `
        -p:SourceRoot="$SourceRoot" -p:PowerShellMsi="$PowerShellMsi" `
        -p:SetupHelper="$helper" -p:SuiteMsi="$msi" -p:ArtifactsDir="$OutputDirectory\" `
        -p:WixEulaId=$WixEulaId
    if ($LASTEXITCODE -ne 0 -or
        -not (Test-Path -LiteralPath (Join-Path $OutputDirectory "VividoSetup-$Version-x64.exe") -PathType Leaf)) {
        throw 'WiX Burn build failed'
    }
}
