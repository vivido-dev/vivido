//! PTY wakeups must present visible output even when native redraw/idle callbacks are starved.
//! Run with `cargo test --test macos_output_frames -- --ignored` on a macOS desktop.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    if std::env::args().any(|arg| arg == "--list") {
        println!("macos_output_frames: test");
        return;
    }
    if !std::env::args().any(|arg| arg == "--ignored") {
        println!("macos_output_frames: ignored (requires a macOS desktop and GPU; pass --ignored)");
        return;
    }

    use std::time::Duration;
    use vivido::config::{UiConfig, ui_config::Program};
    use vivido::terminal::event::Event as TerminalEvent;
    use vivido::{Event, EventType, LoopHandle, Processor, WindowOptions};
    use winit::application::ApplicationHandler;
    use winit::event::{Event as WinitEvent, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::platform::macos::EventLoopBuilderExtMacOS;
    use winit::window::WindowId;

    struct Regression {
        processor: Processor,
        panes: Vec<WindowId>,
        proxy: Option<EventLoopProxy<Event>>,
        output_wakes: usize,
    }

    impl ApplicationHandler<Event> for Regression {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            if !self.panes.is_empty() {
                return;
            }
            for visible in [true, false] {
                let mut options = WindowOptions::default();
                options.no_activate = true;
                options.terminal_options.set_command(&Program::WithArgs {
                    program: "/bin/cat".into(),
                    args: Vec::new(),
                });
                let public_id =
                    self.processor.create_window(LoopHandle::Winit(event_loop), options).unwrap();
                let id = self.processor.platform_window_id(public_id).unwrap();
                let pane = self.processor.window_mut(id).unwrap();
                pane.display.window.set_visible(visible);
                pane.set_automation_visible(visible);
                if visible {
                    pane.display.window.order_front_without_focus();
                }
                self.panes.push(id);
            }

            let proxy = self.proxy.take().unwrap();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(250));
                for _ in 0..64 {
                    if proxy.send_event(Event::new(EventType::HostWakeup, None)).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // Leave the last dirty update to the active timer, without an idle callback.
                std::thread::sleep(Duration::from_millis(100));
                proxy.send_event(Event::new(EventType::Shutdown, None)).unwrap();
            });
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
            match event.payload() {
                EventType::HostWakeup => {
                    let before =
                        self.processor.window(self.panes[0]).unwrap().automation.frame_sequence;
                    self.output_wakes += 1;
                    for id in &self.panes {
                        self.processor.handle_winit_event(
                            event_loop,
                            WinitEvent::UserEvent(Event::new(
                                EventType::Terminal(TerminalEvent::Wakeup),
                                *id,
                            )),
                        );
                    }
                    if self.output_wakes == 1 {
                        assert!(
                            self.processor.window(self.panes[0]).unwrap().automation.frame_sequence
                                > before,
                            "the first output update must not wait for native redraw or idle"
                        );
                        let frame =
                            self.processor.window(self.panes[0]).unwrap().automation.frame_sequence;
                        self.processor.handle_winit_event(
                            event_loop,
                            WinitEvent::WindowEvent {
                                window_id: self.panes[0],
                                event: WindowEvent::RedrawRequested,
                            },
                        );
                        assert_eq!(
                            self.processor.window(self.panes[0]).unwrap().automation.frame_sequence,
                            frame,
                            "an already queued native redraw must not duplicate a direct frame"
                        );
                    }
                    // A regular frame token can arrive behind the same burst. It must share
                    // the output limiter and leave deferred work to the active tail timer.
                    self.processor.handle_winit_event(
                        event_loop,
                        WinitEvent::UserEvent(Event::new(EventType::Frame, self.panes[0])),
                    );
                    assert!(
                        !self
                            .processor
                            .window(self.panes[0])
                            .unwrap()
                            .display
                            .window
                            .requested_redraw,
                        "a deferred frame must not bypass the limiter through native redraw"
                    );
                },
                EventType::Shutdown => {
                    let visible = self.processor.window(self.panes[0]).unwrap();
                    assert!(visible.automation.frame_sequence > 1, "output must keep presenting");
                    assert!(!visible.dirty, "the active timer must present the final update");
                    assert_eq!(
                        self.processor.window(self.panes[1]).unwrap().automation.frame_sequence,
                        0,
                        "hidden output must not perform presentation work"
                    );
                    event_loop.exit();
                },
                _ => self.processor.handle_winit_event(event_loop, WinitEvent::UserEvent(event)),
            }
        }

        // Allow startup, then withhold native redraws and AboutToWait during output. This models
        // winit draining a continuous stream of PTY events before it returns to those callbacks.
        fn window_event(
            &mut self,
            event_loop: &ActiveEventLoop,
            window_id: WindowId,
            event: WindowEvent,
        ) {
            if self.output_wakes == 0 {
                self.processor
                    .handle_winit_event(event_loop, WinitEvent::WindowEvent { window_id, event });
            }
        }

        fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
            if self.output_wakes == 0 {
                self.processor.handle_winit_event(event_loop, WinitEvent::AboutToWait);
            }
        }

        fn exiting(&mut self, event_loop: &ActiveEventLoop) {
            self.processor.handle_winit_event(event_loop, WinitEvent::LoopExiting);
        }
    }

    let mut builder = EventLoop::<Event>::with_user_event();
    builder.with_activate_ignoring_other_apps(false);
    let event_loop = builder.build().unwrap();
    let mut config = UiConfig::default();
    config.ipc_socket = Some(false);
    let mut options = vivido::cli::Options::default();
    options.daemon = true;
    let _terminfo = vivido::tty::setup_env();
    let mut regression = Regression {
        processor: Processor::new(config, options, &event_loop),
        panes: Vec::new(),
        proxy: Some(event_loop.create_proxy()),
        output_wakes: 0,
    };
    event_loop.run_app(&mut regression).unwrap();
}
