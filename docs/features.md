# Vivido features

## Search

Vivido has one incremental regex search mode. Start a forward search with
`Control+Shift+F` on Linux/Windows or `Command+F` on macOS. Start a backward search with
`Control+Shift+B` or `Command+B`. `F3` advances, `Shift+F3` moves to the previous match,
`Enter` confirms the focused match as a simple selection, and `Escape` cancels.

Vi mode and vi-specific search commands are not part of Vivido.

## Mouse selection

Drag the primary mouse button to create a simple character selection. Hold Control while
dragging for a rectangular block selection. Double/triple-click semantic or line selection,
right-click expansion, and all other selection-expansion modes are intentionally absent.

## Hints

Regex hints remain available for opening links and launching configured commands. Hints are
activated through configured keys or the mouse and do not depend on a vi cursor.

## Graphics and media

The Vivid side channel transfers, places, plays, and deletes raster and video media. Vivido
decodes frames independently of the renderer, uploads visible sources through wgpu, and
composites them between terminal backgrounds and glyphs. Other escape-sequence media commands are
recognized as extension points but are not rendered yet.

## Linux display backend

Linux uses Wayland and Vulkan exclusively. Vivido does not compile an X11, Xlib, XCB, GLX,
OpenGL, or GLES backend.
