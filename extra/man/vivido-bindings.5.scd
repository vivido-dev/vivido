VIVIDO-BINDINGS(5)

# NAME

vivido-bindings - Default Vivido bindings

# SEARCH

*Control+Shift+F* (Linux/Windows), *Command+F* (macOS)
: Start forward incremental regex search.

*Control+Shift+B* (Linux/Windows), *Command+B* (macOS)
: Start backward incremental regex search.

*F3* / *Shift+F3*
: Focus the next / previous match while search is active.

*Enter*
: Confirm the focused match as a simple selection and leave search.

*Escape*
: Leave search.

# SELECTION

Primary-button drag creates a simple mouse selection. Control-drag creates a block selection.
Middle-click pastes the primary selection on platforms that provide it. Vivido has no
selection-expansion binding or vi selection mode.

# COMMON ACTIONS

*Control+Shift+C* / *Control+Shift+V* (Linux/Windows)
: Copy / paste.

*Command+C* / *Command+V* (macOS)
: Copy / paste.

Bindings can be customized through the *keyboard.bindings* and *mouse.bindings* configuration
arrays. Removed vi and expansion action names are rejected during configuration parsing.

# SEE ALSO

*vivido*(1), *vivido*(5)
