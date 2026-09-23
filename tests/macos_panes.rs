//! Native pane geometry regression, without a renderer or PTY.
//! Run with `cargo test --test macos_panes -- --ignored` in a macOS desktop session.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSView, NSWindow, NSWindowNumberListOptions, NSWindowOrderingMode};
    use vivido::ParentWindowHandle;
    use vivido::cli::WindowOptions;
    use vivido::config::{UiConfig, window::Decorations};
    use vivido::display::window::Window;
    use winit::application::ApplicationHandler;
    use winit::dpi::{PhysicalPosition, PhysicalSize};
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::platform::macos::{EventLoopBuilderExtMacOS, WindowAttributesExtMacOS};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window as HostWindow, WindowId};

    if !std::env::args().any(|arg| arg == "--ignored") {
        println!("macos_panes: ignored (requires a macOS desktop; pass --ignored)");
        return;
    }

    fn native(raw: RawWindowHandle) -> objc2::rc::Retained<NSWindow> {
        assert!(MainThreadMarker::new().is_some());
        let RawWindowHandle::AppKit(handle) = raw else { panic!("expected AppKit") };
        // SAFETY: called on the main thread while the owning winit window is live.
        unsafe { handle.ns_view.cast::<NSView>().as_ref() }.window().unwrap()
    }

    struct Regression;
    impl ApplicationHandler for Regression {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let host = event_loop
                .create_window(
                    HostWindow::default_attributes()
                        .with_visible(false)
                        .with_active(false)
                        .with_fullsize_content_view(true)
                        .with_inner_size(PhysicalSize::new(800, 600)),
                )
                .unwrap();
            let host_native = native(host.window_handle().unwrap().as_raw());
            let cover = event_loop
                .create_window(
                    HostWindow::default_attributes().with_visible(false).with_active(false),
                )
                .unwrap();
            let cover_native = native(cover.window_handle().unwrap().as_raw());
            let mut config = UiConfig::default();
            config.window.decorations = Decorations::None;
            let mut panes = Vec::new();
            for _ in 0..2 {
                let mut options = WindowOptions::default();
                // SAFETY: host outlives both panes; all calls run on the event-loop thread.
                options.parent_window = Some(unsafe {
                    ParentWindowHandle::new(host.window_handle().unwrap().as_raw())
                });
                options.no_activate = true;
                let pane = Window::new(event_loop, &config, &config.window.identity, &mut options)
                    .unwrap();
                pane.set_resizable(false);
                host_native.removeChildWindow(&native(pane.raw_window_handle().unwrap()));
                pane.set_visible(false);
                panes.push(pane);
            }

            // Changing height must not move the top edge into the shell's 35-point tab strip.
            // Exercise a second tab, detached while hidden, across a host move as well.
            for index in 0..2 {
                let pane = &panes[index];
                let pane_native = native(pane.raw_window_handle().unwrap());
                host_native.removeChildWindow(&pane_native);
                pane.set_visible(false);
                host.set_outer_position(PhysicalPosition::new(160 + index as i32 * 40, 180));
                let origin = host.inner_position().unwrap();
                let position = PhysicalPosition::new(
                    origin.x + 80,
                    origin.y + (35.0 * host.scale_factor()).round() as i32,
                );
                let other_frame = native(panes[1 - index].raw_window_handle().unwrap()).frame();
                for height in [240, 310, 270, 310] {
                    let size = PhysicalSize::new(400, height);
                    pane.set_geometry(position, size);
                    assert_eq!(pane.outer_position(), Some(position), "height {height}");
                    assert_eq!(pane.inner_size(), size);
                }
                // SAFETY: both native windows remain live on the main thread.
                unsafe {
                    host_native.addChildWindow_ordered(&pane_native, NSWindowOrderingMode::Above);
                }
                host_native.orderFrontRegardless();
                cover_native.orderFrontRegardless();
                for _ in 0..2 {
                    pane.order_front_without_focus();
                    let numbers = NSWindow::windowNumbersWithOptions(
                        NSWindowNumberListOptions::empty(),
                        MainThreadMarker::new().unwrap(),
                    )
                    .unwrap();
                    let order = |window: &NSWindow| {
                        numbers
                            .iter()
                            .position(|number| number.integerValue() == window.windowNumber())
                            .unwrap()
                    };
                    assert!(
                        order(&cover_native) < order(&pane_native),
                        "reveal raised pane above another window"
                    );
                    assert!(
                        order(&pane_native) < order(&host_native),
                        "pane must stay above its host"
                    );
                }
                assert_eq!(pane.outer_position(), Some(position));
                assert_eq!(
                    native(panes[1 - index].raw_window_handle().unwrap()).frame(),
                    other_frame
                );
                assert!(!pane_native.isKeyWindow(), "revealing a tab must not take focus");
                host_native.removeChildWindow(&pane_native);
                pane.set_visible(false);
            }
            cover_native.orderOut(None);
            host_native.orderOut(None);
            event_loop.exit();
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    let mut builder = EventLoop::builder();
    builder.with_activate_ignoring_other_apps(false);
    builder.build().unwrap().run_app(&mut Regression).unwrap();
}
