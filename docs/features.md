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
On Windows, right-click pastes the system clipboard when terminal mouse reporting is inactive;
hold Shift to use this terminal-side paste while an application has enabled mouse reporting.

## Hints

Regex hints remain available for opening links and launching configured commands. Hints are
activated through configured keys or the mouse and do not depend on a vi cursor.

## Graphics and media

The Vivid side channel transfers, places, plays, and deletes raster and video media. Over a
`vvssh` session with the remote `vvreceive` helper, dropping a local file — or pasting one
(`Control+Shift+V`) when the clipboard holds a copied file or an image — copies it into the remote
shell's current directory in one gesture. Vivido
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
