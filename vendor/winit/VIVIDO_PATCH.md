# Vivido winit patch

This is winit 0.30.13, vendored from crates.io under its upstream Apache-2.0 license.

Vivido carries one Wayland compatibility change: compositors exposing a `weston_*` registry
global receive the client-side-decoration `xdg_toplevel.move` request during the original button
press. Weston does not advertise `xdg-decoration`, and deferring this request until a later pointer
motion leaves its title bar unable to start an interactive move. Other compositors retain winit's
upstream deferred behavior, including its GNOME double-click workaround.
