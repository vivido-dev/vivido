//! Native AppKit regression; requires a desktop session, but no GPU renderer or terminal PTY.
//! Run with `cargo test --test macos_tabs -- --ignored`.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    use vivido::cli::WindowOptions;
    use vivido::config::UiConfig;
    use vivido::display::window::Window;
    use winit::application::ApplicationHandler;
    use winit::event::WindowEvent;
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::platform::macos::WindowExtMacOS;
    use winit::raw_window_handle::RawWindowHandle;
    use winit::window::WindowId;

    if !std::env::args().any(|arg| arg == "--ignored") {
        println!("macos_tabs: ignored (requires a macOS desktop; pass --ignored)");
        return;
    }

    #[derive(Default)]
    struct TabRegression {
        tab_counts: Option<(usize, usize)>,
    }

    impl ApplicationHandler for TabRegression {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            let config: UiConfig = toml::from_str("[window]\nopacity = 0.5\n").unwrap();
            let mut source = Window::new(
                event_loop,
                &config,
                &config.window.identity,
                &mut WindowOptions::default(),
            )
            .unwrap();
            source.set_visible(true);
            // The first rendered frame applies the terminal's native titlebar appearance.
            source.set_titlebar_appearance(
                config.colors.primary.background,
                config.window_opacity(),
                config.window.theme(),
            );
            source.did_present();

            let mut options = WindowOptions::default();
            options.window_tabbing_id = Some(source.tabbing_id());
            let tab =
                Window::new(event_loop, &config, &config.window.identity, &mut options).unwrap();
            let RawWindowHandle::AppKit(handle) = tab.raw_window_handle().unwrap() else {
                panic!("expected AppKit tab");
            };
            // SAFETY: the tab owns this live view, and the event loop runs on the main thread.
            let native = unsafe { handle.ns_view.cast::<objc2_app_kit::NSView>().as_ref() }
                .window()
                .unwrap();
            assert_eq!(native.backgroundColor().alphaComponent(), 0.5);
            tab.set_visible(true);
            tab.did_present();
            assert_eq!(native.backgroundColor().alphaComponent(), 0.);

            let vivido::display::window::RenderSource::Surface(source_window) =
                source.render_source()
            else {
                panic!("expected native source window");
            };
            let vivido::display::window::RenderSource::Surface(tab_window) = tab.render_source()
            else {
                panic!("expected native tab window");
            };
            self.tab_counts = Some((source_window.num_tabs(), tab_window.num_tabs()));
            event_loop.exit();
        }

        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    let mut regression = TabRegression::default();
    EventLoop::new().unwrap().run_app(&mut regression).unwrap();
    assert_eq!(regression.tab_counts, Some((2, 2)), "new tab must join the source window");
}
