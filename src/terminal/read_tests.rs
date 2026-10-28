//! Bounded nonblocking PTY batches must retain their tail on every read outcome.

// Rust guideline compliant 2026-10-07

use super::*;
use crate::terminal::event::VoidListener;
use crate::terminal::index::{Column, Line};
use crate::terminal::term::{Config, test::TermSize};
use crate::terminal::tty::EventedReadWrite;
use std::io::Cursor;

struct ScriptedPty {
    input: Cursor<Vec<u8>>,
    chunk: usize,
    end: Option<ErrorKind>,
    output: Vec<u8>,
}

impl Read for ScriptedPty {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.input.position() as usize == self.input.get_ref().len() {
            return self.end.map_or(Ok(0), |kind| Err(kind.into()));
        }
        let len = out.len().min(self.chunk);
        self.input.read(&mut out[..len])
    }
}

impl EventedReadWrite for ScriptedPty {
    type Reader = Self;
    type Writer = Vec<u8>;
    unsafe fn register(&mut self, _: &Arc<Poller>, _: PollingEvent, _: PollMode) -> io::Result<()> {
        unreachable!("scripted reads have no poll source")
    }
    fn reregister(&mut self, _: &Arc<Poller>, _: PollingEvent, _: PollMode) -> io::Result<()> {
        unreachable!("scripted reads have no poll source")
    }
    fn deregister(&mut self, _: &Arc<Poller>) -> io::Result<()> {
        Ok(())
    }
    fn reader(&mut self) -> &mut Self {
        self
    }
    fn writer(&mut self) -> &mut Vec<u8> {
        &mut self.output
    }
}
impl tty::EventedPty for ScriptedPty {
    fn next_child_event(&mut self) -> Option<tty::ChildEvent> {
        None
    }
}
impl event::OnResize for ScriptedPty {
    fn on_resize(&mut self, _: WindowSize) {}
}

fn worker(
    input: Vec<u8>,
    chunk: usize,
    end: Option<ErrorKind>,
) -> EventLoop<ScriptedPty, VoidListener> {
    let terminal = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);
    EventLoop::new(
        Arc::new(FairMutex::new(terminal)),
        VoidListener,
        ScriptedPty { input: Cursor::new(input), chunk, end, output: Vec::new() },
        true,
        false,
        Arc::new(Mutex::new(Transcript::default())),
    )
    .unwrap()
}

#[test]
fn burst_tail_is_parsed_before_would_block_eof_or_error() {
    let input = b"before\x1b[31m!\x1b[Hlast";
    for end in [None, Some(ErrorKind::WouldBlock), Some(ErrorKind::BrokenPipe)] {
        for chunk in [1, 7, 1024] {
            let mut worker = worker(input.to_vec(), chunk, end);
            let mut state = State::default();
            let mut buffer = vec![0; READ_BUFFER_SIZE];
            let mut observed = Vec::new();
            let result = worker.pty_read(&mut state, &mut buffer, Some(&mut observed));
            if end == Some(ErrorKind::BrokenPipe) {
                assert_eq!(result.unwrap_err().kind(), ErrorKind::BrokenPipe);
            } else {
                assert_eq!(result.unwrap(), input.len());
            }
            assert_eq!(observed, input);
            let terminal = worker.terminal.lock();
            for (column, c) in "last".chars().enumerate() {
                assert_eq!(terminal.grid()[Line(0)][Column(column)].c, c);
            }
        }
    }
}

#[test]
fn saturated_reads_stop_at_the_batch_budget_and_resume_in_order() {
    let mut input = vec![b'a'; MAX_LOCKED_READ];
    input.extend_from_slice(b"\r\ntail");
    let mut worker = worker(input.clone(), 997, Some(ErrorKind::WouldBlock));
    let mut state = State::default();
    let mut buffer = vec![0; READ_BUFFER_SIZE];
    let mut observed = Vec::new();
    assert_eq!(
        worker.pty_read(&mut state, &mut buffer, Some(&mut observed)).unwrap(),
        MAX_LOCKED_READ
    );
    assert_eq!(worker.pty.input.position() as usize, MAX_LOCKED_READ);
    assert_eq!(observed, input[..MAX_LOCKED_READ]);
    assert_eq!(worker.pty_read(&mut state, &mut buffer, Some(&mut observed)).unwrap(), 6);
    assert_eq!(observed, input);
    assert_eq!(worker.pty_read::<Vec<u8>>(&mut state, &mut buffer, None).unwrap(), 0);
}

#[test]
fn pending_osc_hint_follows_bel_and_split_string_terminators() {
    let mut parser = OscNotificationParser::default();
    assert!(!parser.has_pending_osc());
    parser.advance(b"\x1b]2;title");
    assert!(parser.has_pending_osc());
    parser.advance(b"\x1b");
    assert!(parser.has_pending_osc());
    parser.advance(b"\\");
    assert!(!parser.has_pending_osc());
    parser.advance(b"\x1b]6;discarded body");
    assert!(parser.has_pending_osc());
    parser.advance(b"\x07ordinary text\x1b[31m");
    assert!(!parser.has_pending_osc());
    parser.advance(b"\x1b_Gq=2;AAAA");
    assert!(!parser.has_pending_osc(), "APC input does not use OSC coalescing");
}

#[test]
fn coalesced_osc_tail_is_dispatched_before_eof_or_read_error() {
    let mut input = b"\x1b]6;".to_vec();
    input.extend(std::iter::repeat_n(b'x', 16 * 1024));
    input.extend_from_slice(b"\x07done");
    for end in [None, Some(ErrorKind::WouldBlock), Some(ErrorKind::BrokenPipe)] {
        let mut worker = worker(input.clone(), MIN_OSC_READ_BATCH, end);
        let mut state = State::default();
        let mut buffer = vec![0; READ_BUFFER_SIZE];
        let mut observed = Vec::new();
        let result = worker.pty_read(&mut state, &mut buffer, Some(&mut observed));
        if end == Some(ErrorKind::BrokenPipe) {
            assert_eq!(result.unwrap_err().kind(), ErrorKind::BrokenPipe);
        } else {
            assert_eq!(result.unwrap(), input.len());
        }
        assert_eq!(observed, input);
        assert!(!state.osc_notifications.has_pending_osc());
        let terminal = worker.terminal.lock();
        for (column, c) in "done".chars().enumerate() {
            assert_eq!(terminal.grid()[Line(0)][Column(column)].c, c);
        }
    }
}
