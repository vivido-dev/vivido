//! Exercise Vim's bidirectional startup probes through the real PTY and terminal parser.

#![cfg(unix)]

// Rust guideline compliant 2026-10-07
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use polling::{Event as PollEvent, Events, PollMode, Poller};
use vivido::terminal::event::{Event, EventListener, WindowSize};
use vivido::terminal::term::{Config, Term, test::TermSize};
use vivido::terminal::tty::{self, Options, Shell};
use vivido::terminal::vvte::ansi::Processor;

#[derive(Clone, Default)]
struct Replies(Arc<Mutex<Vec<String>>>);

impl EventListener for Replies {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(reply) = event {
            self.0.lock().unwrap().push(reply);
        }
    }
}

#[test]
fn vim_startup_preserves_empty_and_existing_buffers() {
    let vim = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|directory| directory.join("vim"))
        .find(|path| path.is_file());
    let Some(vim) = vim else {
        eprintln!("skipping Vim startup regression: install vim to exercise terminal probes");
        return;
    };

    for initial in [None, Some("existing first line")] {
        let directory = tempfile::tempdir().unwrap();
        let report = directory.path().join("report");
        let mut args: Vec<String> = ["-Nu", "NONE", "-n", "-i", "NONE", "-T", "xterm-256color"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        if let Some(initial) = initial {
            let path = directory.path().join("input");
            std::fs::write(&path, format!("{initial}\n")).unwrap();
            args.push(path.to_str().unwrap().to_owned());
        }
        let options = Options {
            shell: Some(Shell::new(vim.to_str().unwrap().to_owned(), args)),
            working_directory: Some(directory.path().to_owned()),
            env: [("TERM".to_owned(), "xterm-256color".to_owned())].into(),
            ..Options::default()
        };
        let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
        let pty = tty::new(&options, size, 0).expect("start Vim on a PTY");
        let mut file = pty.file().try_clone().unwrap();
        let poller = Poller::new().unwrap();
        // SAFETY: `file` remains open until after `poller` is dropped; only this thread reads it.
        unsafe { poller.add_with_mode(&file, PollEvent::readable(0), PollMode::Level) }.unwrap();
        let mut events = Events::new();
        let replies = Replies::default();
        let mut term = Term::new(Config::default(), &TermSize::new(80, 24), replies.clone());
        let mut parser = Processor::new();
        let mut command_sent = false;
        let mut cursor_reply = None;
        let mut output = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < deadline, "Vim startup stalled: {:?}", output.escape_ascii());
            let mut chunk = [0; 16 * 1024];
            match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    assert!(
                        output.len() <= 1024 * 1024 - read,
                        "Vim startup output exceeded 1 MiB"
                    );
                    output.extend_from_slice(&chunk[..read]);
                    term.advance(&mut parser, &chunk[..read]);
                    let replies: Vec<_> = replies.0.lock().unwrap().drain(..).collect();
                    for reply in replies {
                        if reply.starts_with("\x1bP1$r") && reply.ends_with(" q\x1b\\") {
                            cursor_reply = Some(reply.clone());
                        }
                        file.write_all(reply.as_bytes()).unwrap();
                        // This is Vim's query immediately following DECRQSS. Queue the assertion
                        // command after its reply, so Vim first has to consume the startup replies.
                        if !command_sent && reply.starts_with("\x1b[?12;") {
                            let path = report.to_str().unwrap().replace('\'', "''");
                            let command = format!(
                                "\x1b:call writefile([getline(1), string(&modified), v:errmsg, \
                                 get(v:, 'termstyleresp', '')], '{path}') | qa!\r"
                            );
                            file.write_all(command.as_bytes()).unwrap();
                            command_sent = true;
                        }
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    events.clear();
                    poller
                        .wait(&mut events, Some(deadline.saturating_duration_since(Instant::now())))
                        .unwrap();
                },
                // Linux reports EIO when the final slave closes; macOS returns EOF above.
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => panic!("read Vim PTY: {error}"),
            }
        }
        let report = std::fs::read_to_string(report).expect("Vim must execute the report command");
        let lines: Vec<_> = report.lines().collect();
        assert_eq!(lines.first().copied(), Some(initial.unwrap_or("")), "{report:?}");
        assert_eq!(lines.get(1).copied(), Some("0"), "startup must not modify the buffer");
        assert_eq!(lines.get(2).copied(), Some(""), "startup must not execute erroneous commands");
        assert_eq!(lines.get(3).copied(), cursor_reply.as_deref(), "Vim must consume DECSCUSR");
        assert!(cursor_reply.is_some(), "Vim must exercise its cursor-style startup probe");
        drop(poller);
    }
}
