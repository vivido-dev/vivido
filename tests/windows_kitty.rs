//! Kitty graphics must survive the actual Windows console transport.

#![cfg(windows)]

// Rust guideline compliant 2026-10-07
use std::io::{ErrorKind, Read};
use std::time::{Duration, Instant};

use vivido::terminal::event::WindowSize;
use vivido::terminal::tty::{self, EventedReadWrite, Options, Shell};

fn probe_pty() -> tty::Pty {
    let options = Options {
        shell: Some(Shell::new(
            "powershell.exe".into(),
            vec![
                "-NoProfile".into(),
                "-Command".into(),
                "[Console]::Write(([char]27 + '_Ga=T,i=7,f=32,s=1,v=1,C=1,q=2;/////w==' + [char]27 + '\\')); [Console]::Write('KITTY-PROBE-DONE')".into(),
            ],
        )),
        escape_args: true,
        ..Options::default()
    };
    let size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 10, cell_height: 20 };
    tty::new(&options, size, 0).expect("create test pseudoconsole")
}

#[test]
fn kitty_apc_survives_conpty_and_another_pty_closing() {
    // Both clients reuse image id 7. Closing one owner must leave the other owner
    // and its loaded console runtime intact.
    let first = probe_pty();
    let mut pty = probe_pty();
    drop(first);
    let mut received = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut buffer = [0_u8; 4096];
    while Instant::now() < deadline {
        match pty.reader().read(&mut buffer) {
            Ok(0) => std::thread::sleep(Duration::from_millis(10)),
            Ok(count) => {
                received.extend_from_slice(&buffer[..count]);
                if received.windows(16).any(|bytes| bytes == b"KITTY-PROBE-DONE") {
                    break;
                }
            },
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            },
            Err(error) if error.kind() == ErrorKind::BrokenPipe => break,
            Err(error) => panic!("PTY read failed: {error}"),
        }
    }
    assert!(received.windows(16).any(|bytes| bytes == b"KITTY-PROBE-DONE"), "child did not finish");
    assert!(
        received.windows(b"\x1b_Ga=T,i=7,".len()).any(|bytes| bytes == b"\x1b_Ga=T,i=7,"),
        "ConPTY discarded the Kitty APC before Vivido could parse it"
    );
}
