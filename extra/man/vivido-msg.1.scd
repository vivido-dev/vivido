VIVIDO-MSG(1)

# NAME

vivido-msg - Send messages to Vivido.

# SYNOPSIS

This command communicates with running Vivido instances through a socket,
making it possible to control Vivido without directly accessing it.

# OPTIONS

*-s, --socket* _<SOCKET>_

	Path for IPC socket communication.

# MESSAGES

*create-window*

	Create a new window in the same Vivido process.

	*FLAGS*
		*--hold*

			Remain open after child process exits.

	*OPTIONS*
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

# SEE ALSO

*vivido*(1), *vivido*(5), *vivido-bindings*(5), *vivido-escapes*(7)

# BUGS

Found a bug? Please report it at _https://github.com/vivido/vivido/issues_.

# MAINTAINERS

- Christian Duerr <contact@christianduerr.com>
- Kirill Chibisov <contact@kchibisov.com>
