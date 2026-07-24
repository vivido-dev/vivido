VIVIDO(1)

# NAME

Vivido - A fast, cross-platform, Vello/wgpu terminal emulator.

# SYNOPSIS

Vivido is a modern terminal emulator that comes with sensible defaults, but
allows for extensive configuration. By integrating with other applications,
rather than reimplementing their functionality, it manages to provide a flexible
set of features with high performance.

# FLAGS

*-h, --help*

	Prints help information.

*--hold*

	Remain open after child process exits.

*--daemon*

	Do not spawn an initial window.

*--print-events*

	Print all events to STDOUT.

*-q*

	Reduces the level of verbosity (the min level is *-qq*).

*--ref-test*

	Generates ref test

*-v*

	Increases the level of verbosity (the max level is *-vvv*).

*-V, --version*

	Prints version information.

# OPTIONS

*--class* _<GENERAL>_ | _<GENERAL>_,_<INSTANCE>_

	Defines the window class hint on Linux.

	When only the general class is passed, instance will be set to the same value.

	On Wayland the general class sets the _app\_id_, while the instance class is ignored.

	Default: _Vivido,Vivido_

*-e, --command* _<COMMAND>..._

	Command and args to execute (must be last argument).

*--config-file* _<CONFIG_FILE>_

	Specify alternative configuration file.

	Vivido doesn't create the config file for you, but it looks for one in the
	following locations on UNIX systems:

		. _$XDG_CONFIG_HOME/vivido/vivido.toml_
		. _$XDG_CONFIG_HOME/vivido.toml_
		. _$HOME/.config/vivido/vivido.toml_
		. _$HOME/.vivido.toml_
		. _/etc/vivido/vivido.toml_

	On Windows, the config file will be looked for in:

		. _%APPDATA%\\vivido\\vivido.toml_

*-o, --option* _<OPTION>..._

	Override configuration file options.

	Example: _vivido -o 'cursor.style="Beam"'_

*--socket* _<SOCKET>_

	Path for IPC socket creation.

*-w, --window-id* _<WINDOW_ID>_

	Assign a stable IPC ID to the initial window. Vivido exports this value as
	_$VIVIDO_WINDOW_ID_ inside the window, and *vivido msg* accepts it for explicit
	window targeting. When omitted, the platform window ID is used.

	A launching shell can choose both IPC environment values before startup:

		export VIVIDO_SOCKET="${TMPDIR:-/tmp}/vivido-$$.sock"
		export VIVIDO_WINDOW_ID="$$"
		vivido --socket "$VIVIDO_SOCKET" --window-id "$VIVIDO_WINDOW_ID" &

*-T, --title* _<TITLE>_

	Defines the window title.

	Default: _Vivido_

*--working-directory* _<WORKING_DIRECTORY>_

	Start the shell in the specified working directory.

# SUBCOMMANDS

*msg*

	Send IPC socket messages (see *vivido-msg*(1)).

# SEE ALSO

*vivido-msg*(1), *vivido*(5), *vivido-bindings*(5), *vivido-escapes*(7)

# BUGS

Found a bug? Please report it at _https://github.com/vivido/vivido/issues_.

# MAINTAINERS

- Christian Duerr <contact@christianduerr.com>
- Kirill Chibisov <contact@kchibisov.com>
