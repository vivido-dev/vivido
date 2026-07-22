VIVIDO-MSG(1)

# NAME

vivido-msg - Send messages to Vivido.

# SYNOPSIS

This command communicates with running Vivido instances through a socket,
making it possible to control Vivido without directly accessing it.

The Unix socket uses the versioned newline-delimited JSON Vivido IPC protocol.
Every connection performs a version-1 handshake. See _docs/ipc.md_ in the Vivido
source distribution for the complete wire contract and JSON schemas.

# OPTIONS

*-s, --socket* _<SOCKET>_

	Path for IPC socket communication.

	When omitted, Vivido first uses _$VIVIDO_SOCKET_ and then discovers the newest
	live socket for the current display.

# TARGETING

Targeted commands use an explicit _--window-id_, then _$VIVIDO_WINDOW_ID_, then
the currently focused Vivido window. Use *list-windows* when these environment
variables were not inherited. Missing and unfocused targets return errors.

# MESSAGES

*create-window*

	Create a new window in the same Vivido process.

	*FLAGS*
		*--hold*

			Remain open after child process exits.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Assign a stable IPC ID to the new window. The ID must be unique within
			the Vivido process and is exported to its PTY as _$VIVIDO_WINDOW_ID_.

		*--working-directory* _<WORKING_DIRECTORY>_

			Start the shell in the specified working directory.

		*-T, --title* _<TITLE>_

			Defines the window title.

			Default: _Vivido_

		*--class* _<GENERAL>_ | _<GENERAL>_,_<INSTANCE>_

			Defines the Wayland app_id on Linux.

			Default: _Vivido,Vivido_

		*-o, --option* _<OPTION>..._

			Override configuration file options.

			Example: _vivido msg create-window -o 'cursor.style="Beam"'_

		*-e, --command* _<COMMAND>..._

			Command and args to execute (must be last argument).

*config*

	Update the Vivido configuration.

	*ARGS*
		*<CONFIG_OPTIONS>...*

			Configuration file options.

			Example: _vivido msg config 'cursor.style="Beam"'_

	*FLAGS*
		*-r, --reset*

			Clear all runtime configuration changes.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the new config.

			Use _-1_ to apply this change to all windows.

			Default: _$VIVIDO_WINDOW_ID_

*get-config*

	Read runtime Vivido configuration.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the config request.

			Use _-1_ to get the global config.

			Default: _$VIVIDO_WINDOW_ID_

*typing*

	Write literal UTF-8 text directly to a terminal's PTY input. The text is not
	wrapped as a bracketed paste and no Enter key is appended.

	*ARGS*
		*<TEXT>*

			Text to write, limited to 1 MiB per message.

			Example: _vivido msg typing 'echo hello'_

			To type a command and press Enter in shells which support ANSI-C quoting:
			_vivido msg typing $'echo hello\r'_

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for terminal input. When omitted, the currently focused Vivido
			window is used.

			Default: _$VIVIDO_WINDOW_ID_

*capabilities*

	Print the protocol version, supported methods and events, stable error codes,
	and effective limits as JSON.

*key* _<KEY>_

	Send a mode-aware Unicode or named key. Named keys include Enter, Escape, Tab,
	Backspace, arrows, navigation keys, F1-F35, and keypad keys.

	*OPTIONS*
		*--mods* _<LIST>_

			Comma-separated Ctrl, Alt, Shift, and Super modifiers.

		*--repeat* _<N>_

			Send the key 1 through 1000 times. Default: _1_.

		*--route* _application_ | _ui_

			Application routing bypasses Vivido shortcuts but honors terminal modes.
			UI routing deliberately uses Vivido's normal input processor.

		*-w, --window-id* _<WINDOW_ID>_

			Target window. Default: _$VIVIDO_WINDOW_ID_ or focused window.

*paste* _<TEXT>_

	Paste at most 1 MiB using bracketed-paste filtering and newline normalization.
	Accepts the same _--route_ and _--window-id_ options as *key*.

*mouse* _move_ | _click_ | _double-click_ | _down_ | _up_ | _drag_ | _scroll_

	Send a cell- or physical-pixel-addressed mouse action. Coordinates are
	zero-based and exactly one of _--cell-column/--cell-row_ or _--x/--y_ is
	required. Button actions accept _--button left|middle|right_. Scroll accepts
	_--vertical_ and _--horizontal_. Mouse actions also accept _--mods_, _--route_,
	and _--window-id_. Application routing requires active mouse reporting and the
	live-bottom viewport.

*resize*

	Request either exact grid dimensions with _--columns/--rows_ or exact physical
	client pixels with _--width/--height_. Success waits for the OS and PTY resize;
	the operation fails after five seconds if the actual size does not match.

*focus*

	Request real operating-system focus. Success requires a real focused event
	within two seconds; Vivido never synthesizes terminal focus state.

*signal* _INT|TERM|HUP|QUIT|TSTP|CONT|WINCH|KILL|STOP_

	Send only the explicitly named signal to the terminal foreground process group,
	falling back to the PTY child group. KILL and STOP must be named explicitly.

*list-windows*

	Print deterministic JSON window discovery data including titles, focus/hold/
	occlusion state, dimensions, process state, and automation sequences.

*inspect*

	Print detailed secret-free JSON state for one window: dimensions, scrollback,
	screen and terminal modes, cursor/selection, process metadata, exit state, and
	automation limits. Process arguments, environments, and Vivid credentials are
	never returned.

*get-grid*

	Print a structured physical-cell grid snapshot as JSON. The default is the
	current viewport. _--start-line LINE --row-count N_ selects up to 1000 retained
	physical rows. _--since-screen SEQUENCE_ returns current viewport row
	replacements since a screen sequence, with full/gap metadata when necessary.

	Rows preserve soft wraps, tabs, styled blanks, wide-character spacers,
	combining characters, hyperlinks, and resolved RGBA styles. Replies are limited
	to 16 MiB and are never truncated.

*wait text* _<TEXT>_

	Wait until visible terminal text matches. _--regex_ enables linear-time regular
	expressions and _--after-screen_ requires a newer semantic screen sequence.

*wait output* _<PATTERN>_

	Wait for sanitized PTY bytes. Use _--regex_ or _--base64_, and optionally
	_--after-offset_. Without an offset only future output is searched.

*wait screen-change*

	Wait for a semantic screen sequence newer than _--after-screen_, or the next
	change when it is omitted.

*wait screen-stable*

	Wait until the semantic terminal screen has not changed for _--quiet DURATION_.
	Default quiet period: _250ms_.

*wait frame*

	Wait for a successfully presented frame newer than _--after-frame_, or the next
	frame when it is omitted.

*wait exit*

	Wait for the terminal child exit status. Held windows return retained status
	immediately.

	All waits accept _--timeout DURATION_ and _--window-id_. Timeout defaults to
	_30s_ and accepts _1ms_ through _24h_.

*transcript*

	Read the 1 MiB sanitized per-window PTY byte ring. JSON output contains monotonic
	offsets, truncation state, and base64 data. _--after-offset_ starts at an exact
	offset and reports an eviction gap rather than silently skipping. _--max-bytes_
	defaults to 65536. _--raw_ writes exact decoded bytes without a newline.

*subscribe*

	Stream one JSON event per line until the connection closes or Ctrl-C. By default
	the current target is used; _--all_ receives all windows. _--events LIST_ filters
	event kinds and _--since-event SEQUENCE_ atomically replays retained history
	before live events. Slow subscribers receive an overflow range and can recover
	with *inspect* and *get-grid*.

*get-text*

	Write plain Unicode terminal grid text to standard output without adding a
	trailing newline. Styling, cursor state, Vivid media, and Vivido overlays are
	not included.

	*OPTIONS*
		*--rows* _<ROWS>_

			Return the latest physical terminal rows ending at the live bottom,
			including retained scrollback and ignoring the current scroll position.
			Soft-wrapped rows are joined as logical text.

			Valid values: _1..=1000_

			When omitted, return the logical text in the currently visible viewport.

		*-w, --window-id* _<WINDOW_ID>_

			Window ID for terminal text. When omitted, the currently focused Vivido
			window is used.

			Default: _$VIVIDO_WINDOW_ID_

*screenshot*

	Capture the last successfully displayed Vivido client-area frame and write it
	to a private temporary PNG. The returned absolute filename is written to
	standard output. The caller is responsible for deleting the file.

	The PNG includes terminal rendering, cursor, selection, overlays, and Vivid
	media. It preserves transparency and excludes operating-system window
	decorations and desktop content.

	*OPTIONS*
		*-w, --window-id* _<WINDOW_ID>_

			Window ID for the screenshot. When omitted, the currently focused Vivido
			window is used.

			Default: _$VIVIDO_WINDOW_ID_

# OUTPUT

Structured observations and waits print JSON. Controls are silent on success.
*create-window* prints the numeric ID, *get-text* writes exact text without adding
a newline, *screenshot* prints one pathname and newline, and *transcript --raw*
writes exact retained bytes. Errors exit nonzero.

# SEE ALSO

*vivido*(1), *vivido*(5), *vivido-bindings*(5), *vivido-escapes*(7)

# BUGS

Found a bug? Please report it at _https://github.com/vivido/vivido/issues_.

# MAINTAINERS

- Christian Duerr <contact@christianduerr.com>
- Kirill Chibisov <contact@kchibisov.com>
