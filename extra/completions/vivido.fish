# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_vivido_global_optspecs
	string join \n print-events ref-test config-file= socket= q v daemon w/window-id= working-directory= hold e/command= T/title= class= o/option= h/help V/version
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
complete -c vivido -n "__fish_vivido_needs_command" -l socket -d 'Path for IPC socket creation' -r -F
complete -c vivido -n "__fish_vivido_needs_command" -s w -l window-id -d 'Stable IPC ID assigned to this window' -r
complete -c vivido -n "__fish_vivido_needs_command" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c vivido -n "__fish_vivido_needs_command" -s e -l command -d 'Command and args to execute (must be last argument)' -r
complete -c vivido -n "__fish_vivido_needs_command" -s T -l title -d 'Defines the window title [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_needs_command" -l class -d 'Defines the Wayland app_id [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_needs_command" -s o -l option -d 'Override configuration file options [example: \'cursor.style="Beam"\']' -r
complete -c vivido -n "__fish_vivido_needs_command" -l print-events -d 'Print all events to STDOUT'
complete -c vivido -n "__fish_vivido_needs_command" -l ref-test -d 'Generates ref test'
complete -c vivido -n "__fish_vivido_needs_command" -s q -d 'Reduces the level of verbosity (the min level is -qq)'
complete -c vivido -n "__fish_vivido_needs_command" -s v -d 'Increases the level of verbosity (the max level is -vvv)'
complete -c vivido -n "__fish_vivido_needs_command" -l daemon -d 'Do not spawn an initial window'
complete -c vivido -n "__fish_vivido_needs_command" -l hold -d 'Remain open after child process exit'
complete -c vivido -n "__fish_vivido_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_needs_command" -s V -l version -d 'Print version'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "msg" -d 'Send a message to the Vivido socket'
complete -c vivido -n "__fish_vivido_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -s s -l socket -d 'IPC socket connection path override' -r -F
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "create-window" -d 'Create a new window in the same Vivido process'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "config" -d 'Update the Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "get-config" -d 'Read runtime Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "typing" -d 'Type literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "get-text" -d 'Read terminal text'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "screenshot" -d 'Capture the last displayed terminal frame'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "capabilities" -d 'Print supported automation methods, events, and limits'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "key" -d 'Send one mode-aware key to a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "paste" -d 'Paste literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "mouse" -d 'Send a mouse action to a terminal or Vivido UI'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "resize" -d 'Resize a terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "focus" -d 'Request real operating-system focus for a window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "signal" -d 'Send an explicit signal to the foreground process group'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "list-windows" -d 'List all windows in deterministic creation order'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "inspect" -d 'Inspect one terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "get-grid" -d 'Read a structured terminal grid snapshot or delta'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "wait" -d 'Wait for terminal state or output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "transcript" -d 'Read retained sanitized PTY output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "subscribe" -d 'Stream automation events until interrupted'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and not __fish_seen_subcommand_from create-window config get-config typing get-text screenshot capabilities key paste mouse resize focus signal list-windows inspect get-grid wait transcript subscribe help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s w -l window-id -d 'Stable IPC ID assigned to this window' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l working-directory -d 'Start the shell in the specified working directory' -r -F
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s e -l command -d 'Command and args to execute (must be last argument)' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s T -l title -d 'Defines the window title [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l class -d 'Defines the Wayland app_id [default: Vivido]' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s o -l option -d 'Override configuration file options [example: \'cursor.style="Beam"\']' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -l hold -d 'Remain open after child process exit'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from create-window" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from config" -s w -l window-id -d 'Window ID for the new config' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from config" -s r -l reset -d 'Clear all runtime configuration changes'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s w -l window-id -d 'Window ID for the config request' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-config" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from typing" -s w -l window-id -d 'Window ID for terminal input' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from typing" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-text" -l rows -d 'Number of latest physical terminal rows to return' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-text" -s w -l window-id -d 'Window ID for terminal text' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from get-text" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from screenshot" -s w -l window-id -d 'Window ID for the screenshot' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from screenshot" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from capabilities" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -l mods -d 'Comma-separated Ctrl, Alt, Shift, and Super modifiers' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -l repeat -d 'Number of key presses to send' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -l route -d 'Input routing mode' -r -f -a "application\t'Bypass Vivido bindings and encode input for the terminal application'
ui\t'Process input through Vivido\'s normal UI input pipeline'"
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from key" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from paste" -l route -d 'Input routing mode' -r -f -a "application\t'Bypass Vivido bindings and encode input for the terminal application'
ui\t'Process input through Vivido\'s normal UI input pipeline'"
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from paste" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from paste" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "move" -d 'Mouse coordinate and modifier arguments'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "click" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "double-click" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "down" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "up" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "drag" -d 'Mouse arguments requiring a button'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "scroll" -d 'Mouse scrolling arguments'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from mouse" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l columns -d 'Exact terminal grid column count' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l rows -d 'Exact terminal grid row count' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l width -d 'Exact physical client width in pixels' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -l height -d 'Exact physical client height in pixels' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from resize" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from focus" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from focus" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from signal" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from signal" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from list-windows" -s h -l help -d 'Print help'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from inspect" -s w -l window-id -d 'Window ID. The focused window is used when this is omitted' -r
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from inspect" -s h -l help -d 'Print help'
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
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "config" -d 'Update the Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-config" -d 'Read runtime Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "typing" -d 'Type literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-text" -d 'Read terminal text'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "screenshot" -d 'Capture the last displayed terminal frame'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "capabilities" -d 'Print supported automation methods, events, and limits'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "key" -d 'Send one mode-aware key to a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "paste" -d 'Paste literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "mouse" -d 'Send a mouse action to a terminal or Vivido UI'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "resize" -d 'Resize a terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "focus" -d 'Request real operating-system focus for a window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "signal" -d 'Send an explicit signal to the foreground process group'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "list-windows" -d 'List all windows in deterministic creation order'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "inspect" -d 'Inspect one terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "get-grid" -d 'Read a structured terminal grid snapshot or delta'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "wait" -d 'Wait for terminal state or output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "transcript" -d 'Read retained sanitized PTY output'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "subscribe" -d 'Stream automation events until interrupted'
complete -c vivido -n "__fish_vivido_using_subcommand msg; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg help" -f -a "msg" -d 'Send a message to the Vivido socket'
complete -c vivido -n "__fish_vivido_using_subcommand help; and not __fish_seen_subcommand_from msg help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "create-window" -d 'Create a new window in the same Vivido process'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "config" -d 'Update the Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-config" -d 'Read runtime Vivido configuration'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "typing" -d 'Type literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-text" -d 'Read terminal text'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "screenshot" -d 'Capture the last displayed terminal frame'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "capabilities" -d 'Print supported automation methods, events, and limits'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "key" -d 'Send one mode-aware key to a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "paste" -d 'Paste literal text into a terminal'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "mouse" -d 'Send a mouse action to a terminal or Vivido UI'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "resize" -d 'Resize a terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "focus" -d 'Request real operating-system focus for a window'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "signal" -d 'Send an explicit signal to the foreground process group'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "list-windows" -d 'List all windows in deterministic creation order'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "inspect" -d 'Inspect one terminal window'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "get-grid" -d 'Read a structured terminal grid snapshot or delta'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "wait" -d 'Wait for terminal state or output'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "transcript" -d 'Read retained sanitized PTY output'
complete -c vivido -n "__fish_vivido_using_subcommand help; and __fish_seen_subcommand_from msg" -f -a "subscribe" -d 'Stream automation events until interrupted'
