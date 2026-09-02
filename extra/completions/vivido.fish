# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_vivido_global_optspecs
	string join \n print-events ref-test config-file= s/socket= headless session= automation-name= foreground headless-size= q v daemon vivid-target= w/window-id= no-activate working-directory= hold e/command= T/title= class= o/option= h/help V/version
end

function __fish_vivido_needs_command
	# Figure out if the current invocation already has a command.
	set -l cmd (commandline -opc)
	set -e cmd[1]
	argparse -s (__fish_vivido_global_optspecs) -- $cmd 2>/dev/null
	or return
	if set -q argv[1]
		# Also print the command, so this can be used to figure out what it is.
		echo $argv[1]
		return 1
	end
	return 0
end

function __fish_vivido_using_subcommand
	set -l cmd (__fish_vivido_needs_command)
	test -z "$cmd"
	and return 1
	contains -- $cmd[1] $argv
end

complete -c vivido -n "__fish_vivido_needs_command" -l config-file -d 'Specify alternative configuration file [default: $XDG_CONFIG_HOME/vivido/vivido.toml]' -r -F
complete -c vivido -n "__fish_vivido_needs_command" -s s -l socket -d 'Local IPC endpoint (a filesystem path on Unix, a named-pipe path on Windows)' -r -F
complete -c vivido -n "__fish_vivido_needs_command" -l session -d 'Name of the headless session, for `--target` on `msg` [default: derived from the PID]' -r
complete -c vivido -n "__fish_vivido_needs_command" -l automation-name -d 'Stable same-user automation name for a headed instance [default: vivido-<pid>]' -r
complete -c vivido -n "__fish_vivido_needs_command" -l headless-size -d 'Size of the headless window, as COLUMNSxLINES or WIDTHxHEIGHTpx' -r
complete -c vivido -n "__fish_vivido_needs_command" -l vivid-target -d 'Present a `desktop-surface-v1` Vivid target instead of the terminal target' -r -f -a "terminal\t'`terminal-surface-v1`: a grid of cells with a text plane and anchors'
desktop\t'`desktop-surface-v1`: a virtual desktop in logical pixels, with no grid and no anchors'"
complete -c vivido -n "__fish_vivido_needs_command" -s w -l window-id -d 'Stable IPC ID assigned to this window' -r
complete -c vivido -n "__fish_vivido_needs_command" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c vivido -n "__fish_vivido_needs_command" -s e -l command -d 'Command and args to execute (must be last argument)' -r
complete -c vivido -n "__fish_vivido_needs_command" -s T -l title -d 'Defines the window title [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_needs_command" -l class -d 'Defines the Wayland app_id [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_needs_command" -s o -l option -d 'Override configuration file options [example: \'cursor.style="Beam"\']' -r
complete -c vivido -n "__fish_vivido_needs_command" -l print-events -d 'Print all events to STDOUT'
complete -c vivido -n "__fish_vivido_needs_command" -l ref-test -d 'Generates ref test'
complete -c vivido -n "__fish_vivido_needs_command" -l headless -d 'Run with no window and no compositor, serving IPC in the background'
complete -c vivido -n "__fish_vivido_needs_command" -l foreground -d 'Keep a headless instance attached to this terminal instead of detaching'
complete -c vivido -n "__fish_vivido_needs_command" -s q -d 'Reduces the level of verbosity (the min level is -qq)'
complete -c vivido -n "__fish_vivido_needs_command" -s v -d 'Increases the level of verbosity (the max level is -vvv)'
complete -c vivido -n "__fish_vivido_needs_command" -l daemon -d 'Do not spawn an initial window'
complete -c vivido -n "__fish_vivido_needs_command" -l no-activate -d 'Map the window without taking keyboard focus from the active application'
complete -c vivido -n "__fish_vivido_needs_command" -l hold -d 'Remain open after child process exit'
complete -c vivido -n "__fish_vivido_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_needs_command" -s V -l version -d 'Print version'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "msg" -d 'Send a message to the Vivido socket'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "list" -d 'List running headless sessions'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "doctor" -d 'Check discovery, IPC, rendering, and presenter health'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "debug-bundle" -d 'Write a bounded, versioned diagnostic ZIP bundle'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "kill-session" -d 'Shut down a headless session'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -s s -l socket -d 'IPC endpoint override (a filesystem path on Unix, a named-pipe path on Windows)' -r -F
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -s t -l target -d 'Name of the headless session to talk to [default: $VIVIDO_SESSION, else the only session]' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "create-window" -d 'Create a new window in the same Vivido process'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "quit" -d 'Shut down the Vivido instance, closing every window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "ping" -d 'Check IPC liveness'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "config" -d 'Update the Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "get-config" -d 'Read runtime Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "typing" -d 'Type literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "get-text" -d 'Read terminal text'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "screenshot" -d 'Capture the last displayed terminal frame'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "capabilities" -d 'Print supported automation methods, events, and limits'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "run-plan" -d 'Execute a bounded JSON automation plan over one IPC connection'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "capture" -d 'Activate, settle, and capture a window in one client operation'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "key" -d 'Send one mode-aware key to a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "paste" -d 'Paste literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "mouse" -d 'Send a mouse action to a terminal or Vivido UI'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "resize" -d 'Resize a terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "set-geometry" -d 'Move and optionally resize a window\'s outer frame'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "set-visible" -d 'Map or unmap a window without destroying it'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "set-level" -d 'Set a window\'s stacking level'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "focus" -d 'Request real operating-system focus for a window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "signal" -d 'Send an explicit signal to the foreground process group'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "list-windows" -d 'List all windows in deterministic creation order'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "inspect" -d 'Inspect one terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "diagnose" -d 'Capture one correlated, metadata-only diagnostic snapshot'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "vivid" -d 'Inspect or trace the Vivid presenter'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "get-grid" -d 'Read a structured terminal grid snapshot or delta'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "wait" -d 'Wait for terminal state or output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "transcript" -d 'Read retained sanitized PTY output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "subscribe" -d 'Stream automation events until interrupted'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window quit ping config get-config typing get-text screenshot capabilities run-plan capture key paste mouse resize set-geometry set-visible set-level focus signal list-windows inspect diagnose vivid get-grid wait transcript subscribe help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l vivid-target -d 'Present a `desktop-surface-v1` Vivid target instead of the terminal target' -r -f -a "terminal\t'`terminal-surface-v1`: a grid of cells with a text plane and anchors'
desktop\t'`desktop-surface-v1`: a virtual desktop in logical pixels, with no grid and no anchors'"
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s w -l window-id -d 'Stable IPC ID assigned to this window' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s e -l command -d 'Command and args to execute (must be last argument)' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s T -l title -d 'Defines the window title [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l class -d 'Defines the Wayland app_id [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s o -l option -d 'Override configuration file options [example: \'cursor.style="Beam"\']' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l no-activate -d 'Map the window without taking keyboard focus from the active application'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l hold -d 'Remain open after child process exit'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from quit" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from ping" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from config" -s w -l window-id -d 'Window ID for the new config' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from config" -s r -l reset -d 'Clear all runtime configuration changes'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s w -l window-id -d 'Window ID for the config request' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from typing" -s w -l window-id -d 'Window ID for terminal input' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from typing" -l report -d 'Print the tagged PTY-write completion as JSON'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from typing" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-text" -l rows -d 'Number of latest physical terminal rows to return' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-text" -s w -l window-id -d 'Window ID for terminal text' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-text" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from screenshot" -s w -l window-id -d 'Window ID for the screenshot' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from screenshot" -l json -d 'Print capture metadata together with the private PNG path'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from screenshot" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capabilities" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from run-plan" -l file -d 'JSON plan file. Omit this option or pass `-` to read standard input' -r -F
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from run-plan" -l dry-run -d 'Validate the plan and advertised methods without executing any step'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from run-plan" -l preflight -d 'Execute observation steps only and report mutating steps as skipped'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from run-plan" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capture" -s w -l window-id -d 'Window ID for the capture' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capture" -l after-frame -d 'Require a frame newer than this sequence before capture' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capture" -l stable -d 'Wait for the terminal screen to remain unchanged; defaults to 250 ms when present' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capture" -l timeout -d 'Maximum wait time' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capture" -l activate -d 'Select and reveal the pane through an advertised host activation method'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capture" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -l mods -d 'Comma-separated Ctrl, Alt, Shift, and Super modifiers' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -l repeat -d 'Number of key presses to send' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -l route -d 'Input routing mode' -r -f -a "application\t'Bypass Vivido bindings and encode input for the terminal application'
ui\t'Process input through Vivido\'s normal UI input pipeline'"
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -l report -d 'Print the tagged PTY-write completion as JSON'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from paste" -l route -d 'Input routing mode' -r -f -a "application\t'Bypass Vivido bindings and encode input for the terminal application'
ui\t'Process input through Vivido\'s normal UI input pipeline'"
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from paste" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from paste" -l report -d 'Print the tagged PTY-write completion as JSON'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from paste" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "move" -d 'Mouse coordinate and modifier arguments'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "click" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "double-click" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "down" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "up" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "drag" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "path" -d 'Draw one bounded press/move/release gesture'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "scroll" -d 'Mouse scrolling arguments'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l columns -d 'Exact terminal grid column count' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l rows -d 'Exact terminal grid row count' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l width -d 'Exact physical client width in pixels' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l height -d 'Exact physical client height in pixels' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-geometry" -l x -d 'Physical-pixel X coordinate of the outer frame\'s top-left corner' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-geometry" -l y -d 'Physical-pixel Y coordinate of the outer frame\'s top-left corner' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-geometry" -l width -d 'Exact physical client width in pixels' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-geometry" -l height -d 'Exact physical client height in pixels' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-geometry" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-geometry" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-visible" -l visible -d 'Map the window when true, unmap it when false' -r -f -a "true\t''
false\t''"
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-visible" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-visible" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-level" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from set-level" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from focus" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from focus" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from signal" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from signal" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from list-windows" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from inspect" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from inspect" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from diagnose" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from diagnose" -l trace-limit -d 'Maximum recent Vivid trace events to include' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from diagnose" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "sessions" -d 'Common target selection for IPC commands'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "surfaces" -d 'Common target selection for IPC commands'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "surface-status" -d 'Common target selection for IPC commands'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "tracks" -d 'Common target selection for IPC commands'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "track-status" -d 'Common target selection for IPC commands'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "scene-status" -d 'Common target selection for IPC commands'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "trace" -d 'Parameters for a bounded Vivid trace query or follow loop'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from vivid" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-grid" -l start-line -d 'First signed physical grid line in retained scrollback/live-screen coordinates' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-grid" -l row-count -d 'Number of physical rows to return' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-grid" -l since-screen -d 'Return current viewport row replacements changed after this screen sequence' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-grid" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-grid" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "text" -d 'Text wait parameters'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "output" -d 'Output wait parameters'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "screen-change" -d 'Screen/frame sequence wait parameters'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "screen-stable" -d 'Screen stability wait parameters'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "frame" -d 'Frame wait parameters'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "vivid-track" -d 'Parameters for a generation-scoped Vivid track wait'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "exit" -d 'Common timeout for wait commands, represented as milliseconds on the wire'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from wait" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from transcript" -l after-offset -d 'First retained byte offset. Omit to request the newest bytes' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from transcript" -l max-bytes -d 'Maximum returned byte count' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from transcript" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from transcript" -l raw -d 'Write exact decoded bytes instead of JSON metadata'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from transcript" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from subscribe" -s w -l window-id -d 'Window ID. The focused window is used when omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from subscribe" -l events -d 'Comma-separated event kinds. Omit for all kinds' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from subscribe" -l since-event -d 'Replay matching events newer than this global event sequence' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from subscribe" -l all -d 'Subscribe to every window and process lifecycle event'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from subscribe" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "create-window" -d 'Create a new window in the same Vivido process'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "quit" -d 'Shut down the Vivido instance, closing every window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "ping" -d 'Check IPC liveness'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "config" -d 'Update the Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-config" -d 'Read runtime Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "typing" -d 'Type literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-text" -d 'Read terminal text'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "screenshot" -d 'Capture the last displayed terminal frame'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "capabilities" -d 'Print supported automation methods, events, and limits'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "run-plan" -d 'Execute a bounded JSON automation plan over one IPC connection'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "capture" -d 'Activate, settle, and capture a window in one client operation'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "key" -d 'Send one mode-aware key to a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "paste" -d 'Paste literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "mouse" -d 'Send a mouse action to a terminal or Vivido UI'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "resize" -d 'Resize a terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "set-geometry" -d 'Move and optionally resize a window\'s outer frame'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "set-visible" -d 'Map or unmap a window without destroying it'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "set-level" -d 'Set a window\'s stacking level'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "focus" -d 'Request real operating-system focus for a window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "signal" -d 'Send an explicit signal to the foreground process group'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "list-windows" -d 'List all windows in deterministic creation order'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "inspect" -d 'Inspect one terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "diagnose" -d 'Capture one correlated, metadata-only diagnostic snapshot'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "vivid" -d 'Inspect or trace the Vivid presenter'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-grid" -d 'Read a structured terminal grid snapshot or delta'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "wait" -d 'Wait for terminal state or output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "transcript" -d 'Read retained sanitized PTY output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "subscribe" -d 'Stream automation events until interrupted'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand list" -l all -d 'Include headed instances in addition to headless sessions'
complete -c vivido -n "__fish_vivido_using_subcommand list" -l json -d 'Emit one bounded JSON document instead of the legacy text format'
complete -c vivido -n "__fish_vivido_using_subcommand list" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand doctor" -s t -l target -d 'Exact registered automation/session name' -r
complete -c vivido -n "__fish_vivido_using_subcommand doctor" -l json -d 'Emit structured JSON. Reserved for a future human renderer when omitted'
complete -c vivido -n "__fish_vivido_using_subcommand doctor" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand debug-bundle" -s t -l target -d 'Exact registered automation/session name' -r
complete -c vivido -n "__fish_vivido_using_subcommand debug-bundle" -l output -d 'Destination ZIP path. The file is created atomically and never written to stdout' -r -F
complete -c vivido -n "__fish_vivido_using_subcommand debug-bundle" -l include-screenshot -d 'Include a rendered screenshot; potentially sensitive'
complete -c vivido -n "__fish_vivido_using_subcommand debug-bundle" -l include-grid -d 'Include the structured terminal grid; potentially sensitive'
complete -c vivido -n "__fish_vivido_using_subcommand debug-bundle" -l include-transcript -d 'Include the retained terminal transcript; potentially sensitive'
complete -c vivido -n "__fish_vivido_using_subcommand debug-bundle" -l include-log -d 'Include a bounded tail of Vivido\'s own log; potentially sensitive'
complete -c vivido -n "__fish_vivido_using_subcommand debug-bundle" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand kill-session" -s t -l target -d 'Name of the session to terminate' -r
complete -c vivido -n "__fish_vivido_using_subcommand kill-session" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg list doctor debug-bundle kill-session help" -f -a "msg" -d 'Send a message to the Vivido socket'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg list doctor debug-bundle kill-session help" -f -a "list" -d 'List running headless sessions'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg list doctor debug-bundle kill-session help" -f -a "doctor" -d 'Check discovery, IPC, rendering, and presenter health'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg list doctor debug-bundle kill-session help" -f -a "debug-bundle" -d 'Write a bounded, versioned diagnostic ZIP bundle'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg list doctor debug-bundle kill-session help" -f -a "kill-session" -d 'Shut down a headless session'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg list doctor debug-bundle kill-session help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "create-window" -d 'Create a new window in the same Vivido process'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "quit" -d 'Shut down the Vivido instance, closing every window'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "ping" -d 'Check IPC liveness'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "config" -d 'Update the Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-config" -d 'Read runtime Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "typing" -d 'Type literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-text" -d 'Read terminal text'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "screenshot" -d 'Capture the last displayed terminal frame'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "capabilities" -d 'Print supported automation methods, events, and limits'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "run-plan" -d 'Execute a bounded JSON automation plan over one IPC connection'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "capture" -d 'Activate, settle, and capture a window in one client operation'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "key" -d 'Send one mode-aware key to a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "paste" -d 'Paste literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "mouse" -d 'Send a mouse action to a terminal or Vivido UI'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "resize" -d 'Resize a terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "set-geometry" -d 'Move and optionally resize a window\'s outer frame'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "set-visible" -d 'Map or unmap a window without destroying it'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "set-level" -d 'Set a window\'s stacking level'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "focus" -d 'Request real operating-system focus for a window'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "signal" -d 'Send an explicit signal to the foreground process group'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "list-windows" -d 'List all windows in deterministic creation order'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "inspect" -d 'Inspect one terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "diagnose" -d 'Capture one correlated, metadata-only diagnostic snapshot'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "vivid" -d 'Inspect or trace the Vivid presenter'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-grid" -d 'Read a structured terminal grid snapshot or delta'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "wait" -d 'Wait for terminal state or output'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "transcript" -d 'Read retained sanitized PTY output'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "subscribe" -d 'Stream automation events until interrupted'
