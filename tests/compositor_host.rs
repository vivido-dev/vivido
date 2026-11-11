//! Exercise compositor input and owner isolation through real headless terminal runtimes.

#![cfg(unix)]

// Rust guideline compliant 2026-10-07
use std::{
    sync::mpsc,
    time::{Duration, Instant},
};
use vivido::config::ui_config::Program;
use vivido::{Event, EventSink, HeadlessLoop, LoopHandle, Processor, UiConfig, WindowOptions};
use winit::{
    dpi::{PhysicalPosition, PhysicalSize},
    event::{DeviceId, WindowEvent},
    window::WindowId,
};

struct Host {
    processor: Processor,
    events: mpsc::Receiver<Event>,
    sink: EventSink,
    headless: HeadlessLoop,
    window: WindowId,
}
impl Host {
    fn new(label: &str) -> Self {
        Self::with_command(
            format!("printf 'ready-{label}\\n'; read value; printf 'got:%s\\n' \"$value\""),
            None,
        )
    }
    fn with_command(command: String, clipboard: Option<Box<dyn vivido::HostClipboard>>) -> Self {
        Self::with_config(command, clipboard, UiConfig::default())
    }
    fn with_config(
        command: String,
        clipboard: Option<Box<dyn vivido::HostClipboard>>,
        config: UiConfig,
    ) -> Self {
        let mut options = vivido::cli::Options::default();
        options.headless = true;
        let (sink, events) = EventSink::headless();
        let mut processor = Processor::new_headless(config, options, sink.clone());
        if let Some(clipboard) = clipboard {
            processor.set_host_clipboard(clipboard).unwrap();
        }
        let headless = HeadlessLoop::new(PhysicalSize::new(640, 480), 1.0);
        let mut window_options = WindowOptions::default();
        window_options.ipc_window_id = Some(7);
        window_options.terminal_options.hold = true;
        window_options.terminal_options.set_command(&Program::WithArgs {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), command],
        });
        let public = processor
            .create_initial_window(LoopHandle::Headless(&headless), window_options)
            .unwrap();
        assert_eq!(public, 7);
        let window = processor.platform_window_id(public).unwrap();
        Self { processor, events, sink, headless, window }
    }
    fn wait_text(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.processor.window(self.window).unwrap().text(None).contains(expected) {
            assert!(Instant::now() < deadline, "terminal did not produce {expected:?}");
            assert!(self.processor.pump_headless(&self.events, &self.headless));
        }
    }
}

#[derive(Debug)]
struct RevokedClipboard;
impl vivido::HostClipboard for RevokedClipboard {
    fn has_selection(&self) -> bool {
        false
    }
    fn load(&mut self, _: vivido::terminal::term::ClipboardType) -> std::io::Result<String> {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "clipboard permission revoked",
        ))
    }
    fn store(
        &mut self,
        _: vivido::terminal::term::ClipboardType,
        _: String,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
#[ignore = "requires native GPU and PTYs; run explicitly for hosted clipboard acceptance"]
fn revoked_clipboard_reads_emit_no_paste_markers_and_preserve_other_owners() {
    let mut first = Host::with_command(r"stty raw -echo; printf '\033[?2004hready-clipboard'; dd bs=1 count=2 2>/dev/null | od -An -v -tx1; printf '\nfinished\n'".into(), Some(Box::new(RevokedClipboard)));
    let mut second = Host::new("second");
    first.wait_text("ready-clipboard");
    second.wait_text("ready-second");
    assert!(first.processor.set_host_clipboard(Box::new(RevokedClipboard)).is_err());
    let modifiers = if cfg!(target_os = "macos") {
        vec!["Super".into()]
    } else {
        vec!["Ctrl".into(), "Shift".into()]
    };
    first.processor.host_key_input(first.window, "v", &modifiers, false).unwrap();
    for key in ["o", "k"] {
        first.processor.host_key_input(first.window, key, &[], false).unwrap();
    }
    first.wait_text("finished");
    let text = first.processor.window(first.window).unwrap().text(None);
    let compact: String = text.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(
        compact.contains("6f6b"),
        "cancelled clipboard read emitted bytes before subsequent keys: {text}"
    );
    assert!(!second.processor.window(second.window).unwrap().text(None).contains("6f 6b"));
    drop(first);
    for key in ["z", "Enter"] {
        second.processor.host_key_input(second.window, key, &[], false).unwrap();
    }
    second.wait_text("got:z");
}

#[test]
#[ignore = "requires native GPU and PTYs; run explicitly for hosted input ordering"]
fn hosted_keys_and_pastes_follow_staged_pointer_events_without_touching_other_owners() {
    let mut config = UiConfig::default();
    config.mouse.hide_when_typing = true;
    let mut first = Host::with_config(
        "stty raw -echo; printf 'ready-order'; exec /bin/cat".into(),
        None,
        config,
    );
    let mut second = Host::new("second");
    first.wait_text("ready-order");
    second.wait_text("ready-second");
    for paste in [false, true] {
        first.processor.handle_embedded_window_event(
            first.window,
            WindowEvent::CursorMoved {
                device_id: DeviceId::dummy(),
                position: PhysicalPosition::new(20., 20.),
            },
        );
        if paste {
            first.processor.host_clipboard_paste(first.window, "paste-order").unwrap();
        } else {
            first.processor.host_key_input(first.window, "x", &[], false).unwrap();
        }
        assert!(!first.processor.window(first.window).unwrap().display.window.mouse_visible());
        assert!(first.processor.pump_headless(&first.events, &first.headless));
        assert!(
            !first.processor.window(first.window).unwrap().display.window.mouse_visible(),
            "older pointer event ran after hosted input"
        );
        assert!(second.processor.window(second.window).unwrap().display.window.mouse_visible());
    }
    first.wait_text("xpaste-order");
    assert!(!second.processor.window(second.window).unwrap().text(None).contains("paste-order"));
    drop(first);
    for key in ["z", "Enter"] {
        second.processor.host_key_input(second.window, key, &[], false).unwrap();
    }
    second.wait_text("got:z");
}
impl Drop for Host {
    fn drop(&mut self) {
        self.processor.finish_headless();
    }
}

#[test]
#[ignore = "requires a native GPU and PTY; run explicitly for compositor acceptance"]
fn hosted_keys_preserve_owner_and_terminal_encoding() {
    let mut first = Host::new("first");
    let mut second = Host::new("second");
    first.wait_text("ready-first");
    second.wait_text("ready-second");
    for key in ["a", "b", "c", "Enter"] {
        first.processor.host_key_input(first.window, key, &[], false).unwrap();
    }
    first.wait_text("got:abc");
    assert!(!second.processor.window(second.window).unwrap().text(None).contains("got:abc"));
    assert!(first.processor.host_key_input(first.window, &"x".repeat(129), &[], false).is_err());
    assert!(first.processor.host_key_input(first.window, "a", &["x".repeat(9)], false).is_err());
    let display = &mut first.processor.window_mut(first.window).unwrap();
    display.set_host_scale_factor(2.0).unwrap();
    assert_eq!(display.display.window.scale_factor, 2.0);
    assert!(display.set_host_scale_factor(f64::NAN).is_err());
    assert!(display.set_host_scale_factor(0.0).is_err());
    assert_eq!(display.display.window.scale_factor, 2.0);
    assert_eq!(second.processor.window(second.window).unwrap().display.window.scale_factor, 1.0);
    drop(first);
    for key in ["z", "Enter"] {
        second.processor.host_key_input(second.window, key, &[], false).unwrap();
    }
    second.wait_text("got:z");
    assert!(second.processor.window(second.window).unwrap().presented_frame_sequence() > 0);
    // Retain the sink through cleanup so the host loop cannot disconnect prematurely.
    drop(second.sink.clone());
}
