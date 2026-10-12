# Vivido shell integration for PowerShell: the prompt and command lifecycle with OSC 133 (A prompt,
# B input, C command, D finish) and the working directory with OSC 7. Vivido runs this with
# -EncodedCommand after the profiles have loaded, so it wraps the prompt they left behind.
$global:__VividoOriginalPrompt = $function:prompt
$global:__VividoCommandRan = $false
function global:prompt {
    # The status of the command that just finished, read before anything else can change it.
    $succeeded = $global:?
    $exitCode = $global:LASTEXITCODE
    $esc = [char]27
    $bel = [char]7
    $marks = ''
    if ($global:__VividoCommandRan) {
        $global:__VividoCommandRan = $false
        # A failed native program leaves its exit code. A failed cmdlet leaves an error from this
        # history entry and no code of its own, since LASTEXITCODE still holds an older program's.
        $code = 0
        if (-not $succeeded) {
            $entry = Get-History -Count 1
            $cmdletFailed = $global:Error.Count -gt 0 -and $entry -and
                $global:Error[0].InvocationInfo.HistoryId -eq $entry.Id
            $code = if (-not $cmdletFailed -and $exitCode) { $exitCode } else { 1 }
        }
        $marks += "$esc]133;D;$code$bel"
    }
    $directory = $ExecutionContext.SessionState.Path.CurrentFileSystemLocation.ProviderPath
    $uri = [System.Uri]::new($directory).AbsoluteUri
    # A goes out before the existing prompt runs, since a prompt may write text itself.
    [Console]::Write("$marks$esc]7;$uri$bel$esc]133;A$bel")
    # Invoke the existing prompt, preserving its text and theme behavior.
    $text = & $global:__VividoOriginalPrompt
    ($text -join '') + "$esc]133;B$bel"
}
# PSReadLine reads each command line; C marks the moment one is accepted to run.
if (Test-Path Function:\PSConsoleHostReadLine) {
    $global:__VividoOriginalReadLine = $function:PSConsoleHostReadLine
    function global:PSConsoleHostReadLine {
        $line = & $global:__VividoOriginalReadLine
        if (-not [string]::IsNullOrWhiteSpace($line)) {
            $global:__VividoCommandRan = $true
            [Console]::Write("$([char]27)]133;C$([char]7)")
        }
        $line
    }
}
