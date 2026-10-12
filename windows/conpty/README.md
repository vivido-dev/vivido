# Microsoft ConPTY runtime

Unmodified native files from Microsoft's MIT-licensed
[Microsoft.Windows.Console.ConPTY 1.25.260930003](https://www.nuget.org/packages/Microsoft.Windows.Console.ConPTY/1.25.260930003).

Package URL:
`https://api.nuget.org/v3-flatcontainer/microsoft.windows.console.conpty/1.25.260930003/microsoft.windows.console.conpty.1.25.260930003.nupkg`

Package SHA-256:
`02b07b349af66d801159bdf9e440d4a1ce78bb951f37fc8609731665afdae7ee`

For each architecture, `conpty.dll` is from `runtimes/win-ARCH/native/` and
`OpenConsole.exe` from `build/native/runtimes/ARCH/`. Individual hashes are in
`SHA256SUMS`. Keep the DLL and helper from the same package when updating.

The Windows system console host can silently discard Kitty APC commands. This
version forwards them and supports enhanced keyboard input. Vivido's build stages
the matching architecture's pair beside executables and test binaries, including
embedding hosts such as Vivida. The installer ships both files and this license.
Vivido loads the DLL by absolute path from its executable directory and retains
it through every pseudoconsole close. No console host is discovered through PATH.
Keep both files beside the executable when copying a source build.
