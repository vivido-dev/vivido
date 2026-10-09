//! Two native hosted panes must consume output while hidden and present after a focus-free reveal.
//! Run with `cargo test --test macos_background_panes -- --ignored` on a macOS desktop.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    if std::env::args().any(|arg| arg == "--list") {
        println!("macos_background_panes: test");
        return;
    }
    if !std::env::args().any(|arg| arg == "--ignored") {
        println!(
            "macos_background_panes: ignored (requires a macOS desktop and GPU; pass --ignored)"
        );
        return;
    }
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSView, NSWindow, NSWindowOrderingMode};
    use std::time::Duration;
    use vivido::config::{UiConfig, ui_config::Program, window::Decorations};
    use vivido::{Event, EventType, LoopHandle, ParentWindowHandle, Processor, WindowOptions};
    use winit::application::ApplicationHandler;
    use winit::dpi::{PhysicalPosition, PhysicalSize};
    use winit::event::{Event as WinitEvent, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::platform::macos::EventLoopBuilderExtMacOS;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::{Window, WindowId};

    fn native(window: &Window) -> objc2::rc::Retained<NSWindow> {
        let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else {
            panic!()
        };
        // SAFETY: these test windows are live on the event-loop thread.
        unsafe { handle.ns_view.cast::<NSView>().as_ref() }.window().unwrap()
    }
    struct Regression {
        processor: Processor,
        host: Option<Window>,
        panes: Vec<WindowId>,
        phase: usize,
        before: Vec<(u64, u64)>,
        wakeups: Option<EventLoopProxy<Event>>,
    }
    impl ApplicationHandler<Event> for Regression {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if self.host.is_some() {
                return;
            }
            let host = event_loop
                .create_window(
                    Window::default_attributes()
                        .with_active(false)
                        .with_inner_size(PhysicalSize::new(800, 600)),
                )
                .unwrap();
            for index in 0..2 {
                let mut options = WindowOptions::default();
                // SAFETY: host outlives the panes.
                options.parent_window = Some(unsafe {
                    ParentWindowHandle::new(host.window_handle().unwrap().as_raw())
                });
                options.no_activate = true;
                options.terminal_options.set_command(&Program::WithArgs {
                    program: "/bin/sh".into(),
                    args: vec![
                        "-c".into(),
                        "i=0; while :; do echo output-$i; i=$((i+1)); sleep 0.1; done".into(),
                    ],
                });
                let id = self
                    .processor
                    .create_hosted_pane(LoopHandle::Winit(event_loop), options)
                    .unwrap();
                let pane = self.processor.window_mut(id).unwrap();
                let origin = host.inner_position().unwrap();
                pane.display.window.set_geometry(
                    PhysicalPosition::new(origin.x + index * 400, origin.y),
                    PhysicalSize::new(400, 600),
                );
                let RawWindowHandle::AppKit(handle) =
                    pane.display.window.raw_window_handle().unwrap()
                else {
                    panic!()
                };
                // SAFETY: both test windows remain live on the main thread.
                let child = unsafe { handle.ns_view.cast::<NSView>().as_ref() }.window().unwrap();
                // SAFETY: both windows are retained on the main thread; AppKit borrows them for this call.
                unsafe {
                    native(&host).addChildWindow_ordered(&child, NSWindowOrderingMode::Above);
                }
                pane.set_automation_visible(true);
                self.panes.push(id);
            }
            self.host = Some(host);
            // An inactive window initially orders behind the foreground application. Ensure
            // these test panes are actually visible before testing background presentation.
            native(self.host.as_ref().unwrap()).orderFrontRegardless();
            for id in &self.panes {
                self.processor.window(*id).unwrap().display.window.order_front_without_focus();
            }
            let proxy = self.wakeups.take().unwrap();
            std::thread::spawn(move || {
                for _ in 0..3 {
                    std::thread::sleep(Duration::from_secs(4));
                    proxy.send_event(Event::new(EventType::HostWakeup, None)).unwrap();
                }
            });
        }
        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            window_id: WindowId,
            event: WindowEvent,
        ) {
            self.processor
                .handle_winit_event(event_loop, WinitEvent::WindowEvent { window_id, event });
        }
        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
            if matches!(event.payload(), EventType::HostWakeup) {
                let app = NSApplication::sharedApplication(MainThreadMarker::new().unwrap());
                let current = self
                    .panes
                    .iter()
                    .map(|id| {
                        let pane = self.processor.window(*id).unwrap();
                        (
                            pane.automation.frame_sequence,
                            pane.automation_summary()["sequences"]["output"].as_u64().unwrap(),
                        )
                    })
                    .collect::<Vec<_>>();
                assert_eq!(current.len(), 2);
                match self.phase {
                    0 => {
                        for (frames, bytes) in &current {
                            assert!(*frames > 0, "pane never presented its startup frame");
                            assert!(*bytes > 0, "pane produced no output");
                        }
                        native(self.host.as_ref().unwrap()).orderOut(None);
                        app.deactivate();
                    },
                    1 => {
                        assert!(!app.isActive(), "background test took application focus");
                        for ((_, bytes), (_, previous)) in current.iter().zip(&self.before) {
                            assert!(*bytes > *previous, "background PTY stopped consuming output");
                        }
                        native(self.host.as_ref().unwrap()).orderFrontRegardless();
                        for id in &self.panes {
                            self.processor
                                .window(*id)
                                .unwrap()
                                .display
                                .window
                                .order_front_without_focus();
                        }
                    },
                    2 => {
                        assert!(!app.isActive(), "revealing panes stole application focus");
                        for ((frames, bytes), (previous_frame, previous_bytes)) in
                            current.iter().zip(&self.before)
                        {
                            assert!(
                                *frames > *previous_frame,
                                "unfocused pane stopped presenting frames"
                            );
                            assert!(
                                *bytes > *previous_bytes,
                                "unfocused PTY stopped consuming output"
                            );
                        }
                        event_loop.exit();
                    },
                    _ => unreachable!(),
                }
                self.before = current;
                self.phase += 1;
            } else {
                self.processor.handle_winit_event(event_loop, WinitEvent::UserEvent(event));
            }
        }
        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            self.processor.handle_winit_event(event_loop, WinitEvent::AboutToWait);
        }
        fn exiting(&mut self, event_loop: &ActiveEventLoop) {
            self.processor.handle_winit_event(event_loop, WinitEvent::LoopExiting);
        }
    }
    let mut builder = EventLoop::<Event>::with_user_event();
    builder.with_activate_ignoring_other_apps(false);
    let event_loop = builder.build().unwrap();
    let mut config = UiConfig::default();
    config.window.decorations = Decorations::None;
    config.ipc_socket = Some(false);
    let mut options = vivido::cli::Options::default();
    options.daemon = true;
    let _terminfo = vivido::tty::setup_env();
    let processor = Processor::new(config, options, &event_loop);
    let proxy = event_loop.create_proxy();

    event_loop
        .run_app(&mut Regression {
            processor,
            host: None,
            panes: Vec::new(),
            phase: 0,
            before: Vec::new(),
            wakeups: Some(proxy),
        })
        .unwrap();
}
