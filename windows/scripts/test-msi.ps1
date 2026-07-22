[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Msi,
    [string]$Log
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ([string]::IsNullOrWhiteSpace($Log)) {
    $Log = Join-Path ([IO.Path]::GetTempPath()) 'vivido-msi-test.log'
}
$process = Start-Process msiexec.exe -ArgumentList @('/i', "`"$Msi`"", '/qn', '/norestart', '/log', "`"$Log`"") -Wait -PassThru
if ($process.ExitCode -notin @(0, 3010)) { throw "MSI install failed with $($process.ExitCode); see $Log" }

$install = Join-Path $env:LOCALAPPDATA 'Programs\Vivido'
foreach ($name in @('vivido.exe', 'vivi.exe', 'vvmux.exe', 'vvssh.exe')) {
    if (-not (Test-Path -LiteralPath (Join-Path $install $name) -PathType Leaf)) { throw "Installed file missing: $name" }
}
$config = Join-Path $env:USERPROFILE 'vivido\vivido.toml'
if (-not (Test-Path -LiteralPath $config -PathType Leaf)) { throw 'User config was not initialized' }
$before = (Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash

$programs = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Vivido'
$expectedShortcuts = @{
    'Vivido PowerShell.lnk' = '-e pwsh.exe'
    'Vivido WSL.lnk' = '-e wsl.exe'
}
$shell = New-Object -ComObject WScript.Shell
foreach ($shortcut in $expectedShortcuts.GetEnumerator()) {
    $shortcutPath = Join-Path $programs $shortcut.Key
    if (-not (Test-Path -LiteralPath $shortcutPath -PathType Leaf)) { throw "Shortcut missing: $($shortcut.Key)" }
    $link = $shell.CreateShortcut($shortcutPath)
    if ([IO.Path]::GetFullPath($link.TargetPath) -ne [IO.Path]::GetFullPath((Join-Path $install 'vivido.exe'))) {
        throw "Shortcut target is incorrect: $($shortcut.Key)"
    }
    if ($link.Arguments -cne $shortcut.Value) { throw "Shortcut arguments are incorrect: $($shortcut.Key)" }
    if ([IO.Path]::GetFullPath($link.WorkingDirectory) -ne [IO.Path]::GetFullPath($env:USERPROFILE)) {
        throw "Shortcut working directory is incorrect: $($shortcut.Key)"
    }
}

$normalizedInstall = [IO.Path]::GetFullPath($install).TrimEnd('\')
$pathEntries = [Environment]::GetEnvironmentVariable('PATH', 'User').Split(';', [StringSplitOptions]::RemoveEmptyEntries) |
    ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).Trim().TrimEnd('\') } |
    Where-Object { $_ -and [IO.Path]::GetFullPath($_) -eq $normalizedInstall }
if (@($pathEntries).Count -ne 1) { throw 'The Vivido user PATH entry was not installed exactly once' }

# Repair must not replace a user-edited configuration file.
Add-Content -LiteralPath $config -Value '# installer-smoke-preserve'
$before = (Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash
$process = Start-Process msiexec.exe -ArgumentList @('/fa', "`"$Msi`"", '/qn', '/norestart', '/log', "`"$Log.repair`"") -Wait -PassThru
if ($process.ExitCode -notin @(0, 3010)) { throw "MSI repair failed with $($process.ExitCode)" }
if ((Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash -ne $before) { throw 'Repair changed user config' }

$process = Start-Process msiexec.exe -ArgumentList @('/x', "`"$Msi`"", '/qn', '/norestart', '/log', "`"$Log.uninstall`"") -Wait -PassThru
if ($process.ExitCode -notin @(0, 3010)) { throw "MSI uninstall failed with $($process.ExitCode)" }
if ((Get-FileHash -LiteralPath $config -Algorithm SHA256).Hash -ne $before) { throw 'Uninstall changed user config' }
if (Test-Path -LiteralPath (Join-Path $install 'vivido.exe')) { throw 'Uninstall left vivido.exe behind' }
$remainingPathEntries = [Environment]::GetEnvironmentVariable('PATH', 'User').Split(';', [StringSplitOptions]::RemoveEmptyEntries) |
    ForEach-Object { [Environment]::ExpandEnvironmentVariables($_).Trim().TrimEnd('\') } |
    Where-Object { $_ -and [IO.Path]::GetFullPath($_) -eq $normalizedInstall }
if (@($remainingPathEntries).Count -ne 0) { throw 'Uninstall left the Vivido user PATH entry behind' }
