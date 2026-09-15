$global:__VividoOriginalPrompt = $function:prompt
function global:prompt {
    # Invoke the existing prompt first, preserving its text and theme behavior.
    $text = & $global:__VividoOriginalPrompt
    $directory = $ExecutionContext.SessionState.Path.CurrentFileSystemLocation.ProviderPath
    $uri = [System.Uri]::new($directory).AbsoluteUri
    [Console]::Write("$([char]27)]7;$uri$([char]7)")
    $text
}
