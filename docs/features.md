# Vivido features

## Search

Vivido has one incremental regex search mode. Start a forward search with
`Control+Shift+F` on Linux/Windows or `Command+F` on macOS. Start a backward search with
`Control+Shift+B` or `Command+B`. `F3` advances, `Shift+F3` moves to the previous match,
`Enter` confirms the focused match as a simple selection, and `Escape` cancels.

Vi mode and vi-specific search commands are not part of Vivido.

## Mouse selection

Drag the primary mouse button to select characters; hold Control while dragging for a
rectangular block. Double-click selects the word under the pointer and triple-click its whole
logical line, soft-wrapped rows included; keep the button down and drag to extend by words or
lines. Shift-click moves the end of an existing selection to the pointer, keeping where it started
and whether it runs by characters, words, or lines. Words end at the characters in
[`selection.semantic_escape_chars`](configuration.md#selection).

While an application has enabled mouse reporting, clicks go to it; hold Shift to select instead.
On Windows, right-click pastes the system clipboard when terminal mouse reporting is inactive;
hold Shift to use this terminal-side paste while an application has enabled mouse reporting.

## Paste protection and clipboard prompts

A program that has not enabled bracketed paste cannot tell a paste from typing, so pasting text
with a line break presses Enter, and other control characters act as the keys that produce them.
Before such a paste Vivido asks, showing the question, how many line breaks and control characters
the text holds, and its first lines with control characters made visible (`␛` for Escape). The
refusing choice starts highlighted, so a reflexive `Enter` pastes nothing. `P` or `Y` pastes; `C`,
`N`, `Escape`, or `Control+C` cancels; `Tab`, `Left`, or `Right` moves the highlight and `Enter`
takes it. Shells with bracketed paste — bash 5.1 and later, zsh, fish — receive pastes as plain
text with Escape removed, so they paste at once. Set `terminal.paste_protection = false` to never
ask.

Programs can also read and replace the clipboard with OSC 52. Replacing it is allowed by default,
which is how editors copy over SSH; reading it asks, since the clipboard can hold a password. That
prompt shows the text the program would receive, with `Allow` (`A`) and `Deny` (`D`). Set either
direction of `terminal.osc52` to `allow`, `ask`, or `deny`. Only a focused terminal's requests
count, and one that arrives while a prompt is waiting is dropped. Resetting or restarting the
terminal client withdraws a waiting prompt, so its answer never reaches another program.

The prompt is drawn in the terminal surface, like the command palette, so it looks and works the
same on Linux, macOS, and Windows, and inside Vivida panes. While it is open no keystroke or paste
reaches the terminal. Automation pastes (`vivido msg paste`) never ask; IPC `inspect` reports an
open prompt as `clipboard_prompt`.

## Command palette

`Control+Shift+P` (`Command+Shift+P` on macOS) opens a searchable list of everything Vivido can
do: every built-in action, each bound hint, and every `command` binding in the configuration,
each shown with the shortcut that runs it. Type to filter — each word must appear in order, and
letters that start words rank higher, so `inc font` finds "Increase font size" — then `Enter`
runs the highlighted entry. `Up`/`Down`, `Tab`/`Shift+Tab`, `Control+P`/`Control+N`, and
`PageUp`/`PageDown` move the highlight; `Backspace`, `Control+W`, and `Control+U` edit the query;
`Escape`, `Control+C`, or the palette's shortcut closes it. While it is open no keystroke or paste
reaches the terminal.

The palette is drawn in the terminal surface, so it works the same inside Vivida panes. Rebind it
with the `ToggleCommandPalette` action.

## Shell integration

Vivido starts bash, zsh, fish, and PowerShell with a small integration loaded, so the shell reports
where each prompt starts, where your input starts, when a command runs, and how it finished (OSC
133), along with its working directory (OSC 7). That is what lets `vivido msg wait prompt` and
`wait command-finish` work, times commands for
[command-finish notifications](#command-finish-notifications), keeps
[closing terminals](#closing-terminals) quiet at an idle Windows prompt, and opens new windows
where you are. Your startup files are never edited, and every one of them still runs:

- **bash** 4.4 or newer starts in POSIX mode with `ENV` naming Vivido's script, which leaves POSIX
  mode, reads the startup files bash would have read (honoring `-l`, `--norc`, `--noprofile`, and
  `--rcfile`), and then hooks `PROMPT_COMMAND` and `PS0`. macOS's own `/bin/bash` is 3.2 and is left
  alone; so is bash running a script or `-c` command.
- **zsh** reads Vivido's `.zshenv` through `ZDOTDIR`, which puts your `ZDOTDIR` back and runs your
  `.zshenv`; the hooks go in at the first prompt, after `.zshrc` and its themes. An `/etc/zshenv`
  that sets `ZDOTDIR` itself bypasses this.
- **fish** finds a `vendor_conf.d` file through `XDG_DATA_DIRS`, which restores the variable. fish
  4 reports all of this itself, so the file only acts on fish 3.
- **PowerShell** on Windows wraps the prompt your profile left behind, and PSReadLine's line reader
  for the command mark.

A shell Vivido did not start, such as one inside `ssh` or a bash you run by hand, can load the same
scripts from `$VIVIDO_SHELL_INTEGRATION_DIR`:

```sh
# ~/.bashrc
[ -n "$VIVIDO_SHELL_INTEGRATION_DIR" ] && . "$VIVIDO_SHELL_INTEGRATION_DIR/bash/vivido.bash"
# ~/.zshrc
[[ -n $VIVIDO_SHELL_INTEGRATION_DIR ]] && source "$VIVIDO_SHELL_INTEGRATION_DIR/zsh/vivido-integration.zsh"
```

IPC `inspect` names the integrated shell as `shell_integration`, or `null` when Vivido loaded none,
so automation can tell "no prompt yet" from "no prompt will ever be reported". Turn it off with
`terminal.shell_integration = false`. Vivido does not load it into WSL, `cmd.exe`, nushell (which
reports prompts itself), or elvish.

## Closing terminals

Closing a terminal that is sitting at its shell prompt happens at once. Closing one that is still
running a program — `vim`, a build, an agent — first asks, naming what would be stopped; the
default answer keeps it running. This covers the window's close button, the `Quit` binding, and
closing a tab in the Windows/Linux tab strip, where quitting asks once for every tab with a
running program. Vivida asks the same way when closing a pane (`Control+W`), a workspace, or the
whole app, and no longer asks when nothing is running.

On Linux and macOS a program is running when the terminal's foreground process group is not the
shell's. Windows has no foreground group, so a child process of the shell counts, unless OSC 133
shell integration reports the shell at its prompt. IPC `inspect` reports the same answer as
`running_program`. Automation closes (`close-window`, `close-pane`, and the like) never ask. Linux
has no native dialog under Wayland, so there closes still happen without asking.

## Command-finish notifications

A command that runs longer than `terminal.notify_on_command_finish_after` (5 seconds) can announce
its finish when you are looking elsewhere. Set `terminal.notify_on_command_finish` to `Unfocused`
or `Always`, and choose the channel with `terminal.notify_on_command_finish_action`: the normal
bell, a desktop notification, or both. The notification names the outcome — "Command Succeeded",
"Command Failed" — with the runtime and exit code, and clicking it focuses the terminal.

This needs OSC 133 integration markers, which [shell integration](#shell-integration) provides in
bash, zsh, fish, and PowerShell: the runtime runs from the marker that starts the command (`C`) to
the finish marker (`D`), so time spent typing at the prompt does not count. A finish without a start
never signals, and neither does a command that finishes within the threshold. The bell channel is
the ordinary bell, so `bell.command` still runs and automation still hears a `bell` event; the
notification channel is your own configuration rather than a program request, so it works even with
`terminal.osc_notifications = false`.

## Hints

Regex hints remain available for opening links and launching configured commands. Hints are
activated through configured keys or the mouse and do not depend on a vi cursor.

## Graphics and media

The Vivid side channel transfers, places, plays, and deletes raster and video media. Over a
`vvssh` session with the remote `vvreceive` helper, dropping a local file — or pasting one
(`Control+Shift+V`) when the clipboard holds a copied file or an image — copies it into the remote
shell's current directory in one gesture, then types the file's committed absolute remote path at
the prompt, the way a local drag types a local path. An AI-agent CLI running on the remote host
therefore sees an image path and attaches it, exactly as it would locally. Set
`[file_drop] paste_remote_path = false` to copy the file and type nothing. Vivido
decodes frames independently of the renderer, uploads visible sources through wgpu, and
composites them between terminal backgrounds and glyphs. Other escape-sequence media commands are
recognized as extension points but are not rendered yet.

## Presentation targets

A window presents one Vivid 1.5 target for its lifetime. `--vivid-target terminal` is the default
grid-and-anchors terminal surface. `--vivid-target desktop` presents a virtual desktop in logical
pixels: no grid, no cell metrics, no anchors, no shell, and `desktop-input-v1` available for
injected keyboard and pointer input over the authenticated interactive lane. The two describe
different coordinate truths, so a window never switches between them.

## Headless mode

`vivido --headless` runs the complete runtime with no window and no compositor, serving automation
IPC in the background and rendering offscreen. Sessions are named with `--session`, listed with
`vivido list`, addressed with `vivido msg --target`, and stopped with `vivido msg quit` or
`vivido kill-session`. `--foreground` keeps the process attached to the calling terminal, and
`--headless-size` fixes the geometry in cells or pixels. Screenshots and frame waits behave exactly
as they do in a window. See [Headless Vivido and named sessions](headless.md).

## Agent automation

`vivido msg` is the owner-only version-2 automation service, available on Linux, macOS, and Windows
over Unix sockets or owner-only named pipes. See [Agent automation IPC](ipc.md).

## Linux display backend

Linux uses Wayland and Vulkan exclusively. Vivido does not compile an X11, Xlib, XCB, GLX,
OpenGL, or GLES backend.
