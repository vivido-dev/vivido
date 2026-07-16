VIVIDO(5)

# NAME

vivido - Vivido configuration file

# DESCRIPTION

Vivido reads TOML configuration from _vivido.toml_. The default locations are
_$XDG_CONFIG_HOME/vivido/vivido.toml_ on Linux,
_$HOME/.config/vivido/vivido.toml_ on macOS, and
_%APPDATA%\vivido\vivido.toml_ on Windows.

Configuration keys and values are compatible with the corresponding Vivido terminal
settings except for features deliberately removed by Vivido: X11-only window settings, vi
mode, vi search, vi actions, vi cursor styling, semantic/line selection, and selection
expansion.

# SEARCH

The *search* table configures normal incremental regex search. There is no vi search mode.

# SELECTION

Mouse dragging creates a simple selection. Control-drag creates a block selection. Selection
expansion modes and actions are not supported.

# GRAPHICS

The Vivid side channel supports composited raster images and decoded video. Sixel and Kitty
escape commands are recognized as extension points but are not rendered yet.

# SEE ALSO

*vivido*(1), *vivido-bindings*(5), *vivido-escapes*(7)
