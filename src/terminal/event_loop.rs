//! The main event loop which performs I/O on the pseudoterminal.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Instant;

use log::error;
use polling::{Event as PollingEvent, Events, PollMode, Poller};

use crate::terminal::event::{self, Event, EventListener, WindowSize};
use crate::terminal::sync::FairMutex;
use crate::terminal::term::Term;
use crate::terminal::{thread, tty};
use vte::ansi;

/// Max bytes to read from the PTY before forced terminal synchronization.
pub(crate) const READ_BUFFER_SIZE: usize = 0x10_0000;

/// Max bytes to read from the PTY while the terminal is locked.
const MAX_LOCKED_READ: usize = u16::MAX as usize;

/// Messages that may be sent to the `EventLoop`.
#[derive(Debug)]
pub enum Msg {
    /// Data that should be written to the PTY.
    Input(Cow<'static, [u8]>),

    /// Indicates that the `EventLoop` should shut down, as Vivido is shutting down.
    Shutdown,

    /// Instruction to resize the PTY.
    Resize(WindowSize),
}

/// The main event loop.
///
/// Handles all the PTY I/O and runs the PTY parser which updates terminal
/// state.
pub struct EventLoop<T: tty::EventedPty, U: EventListener> {
    poll: Arc<Poller>,
    pty: T,
    rx: PeekableReceiver<Msg>,
    tx: Sender<Msg>,
    terminal: Arc<FairMutex<Term<U>>>,
    event_proxy: U,
    drain_on_exit: bool,
    ref_test: bool,
}

impl<T, U> EventLoop<T, U>
where
    T: tty::EventedPty + event::OnResize + Send + 'static,
    U: EventListener + Send + 'static,
{
    /// Create a new event loop.
    pub fn new(
        terminal: Arc<FairMutex<Term<U>>>,
        event_proxy: U,
        pty: T,
        drain_on_exit: bool,
        ref_test: bool,
    ) -> io::Result<EventLoop<T, U>> {
        let (tx, rx) = mpsc::channel();
        let poll = Poller::new()?.into();
        Ok(EventLoop {
            poll,
            pty,
            tx,
            rx: PeekableReceiver::new(rx),
            terminal,
            event_proxy,
            drain_on_exit,
            ref_test,
        })
    }

    pub fn channel(&self) -> EventLoopSender {
        EventLoopSender { sender: self.tx.clone(), poller: self.poll.clone() }
    }

    /// Drain the channel.
    ///
    /// Returns `false` when a shutdown message was received.
    fn drain_recv_channel(&mut self, state: &mut State) -> bool {
        while let Some(msg) = self.rx.recv() {
            match msg {
                Msg::Input(input) => state.write_list.push_back(input),
                Msg::Resize(window_size) => self.pty.on_resize(window_size),
                Msg::Shutdown => return false,
            }
        }

        true
    }

    #[inline]
    fn pty_read<X>(
        &mut self,
        state: &mut State,
        buf: &mut [u8],
        mut writer: Option<&mut X>,
    ) -> io::Result<()>
    where
        X: Write,
    {
        let mut unprocessed = 0;
        let mut processed = 0;

        // Reserve the next terminal lock for PTY reading.
        let _terminal_lease = Some(self.terminal.lease());
        let mut terminal = None;

        loop {
            // Read from the PTY.
            match self.pty.reader().read(&mut buf[unprocessed..]) {
                // This is received on Windows/macOS when no more data is readable from the PTY.
                Ok(0) if unprocessed == 0 => break,
                Ok(got) => unprocessed += got,
                Err(err) => match err.kind() {
                    ErrorKind::Interrupted | ErrorKind::WouldBlock => {
                        // Go back to mio if we're caught up on parsing and the PTY would block.
                        if unprocessed == 0 {
                            break;
                        }
                    },
                    _ => return Err(err),
                },
            }

            // Attempt to lock the terminal.
            let terminal = match &mut terminal {
                Some(terminal) => terminal,
                None => terminal.insert(match self.terminal.try_lock_unfair() {
                    // Force block if we are at the buffer size limit.
                    None if unprocessed >= READ_BUFFER_SIZE => self.terminal.lock_unfair(),
                    None => continue,
                    Some(terminal) => terminal,
                }),
            };

            // Write a copy of the bytes to the ref test file.
            if let Some(writer) = &mut writer {
                writer.write_all(&buf[..unprocessed]).unwrap();
            }

            // Parse the incoming bytes, observing only the bounded authenticated-marker envelope.
            // The complete APC is still passed to VTE, which keeps it zero-width and invisible.
            processed += state.advance(&mut **terminal, &buf[..unprocessed]);
            unprocessed = 0;

            // Assure we're not blocking the terminal too long unnecessarily.
            if processed >= MAX_LOCKED_READ {
                break;
            }
        }

        // Queue terminal redraw unless all processed bytes were synchronized.
        if state.parser.sync_bytes_count() < processed && processed > 0 {
            self.event_proxy.send_event(Event::Wakeup);
        }

        Ok(())
    }

    #[inline]
    fn pty_write(&mut self, state: &mut State) -> io::Result<()> {
        state.ensure_next();

        'write_many: while let Some(mut current) = state.take_current() {
            'write_one: loop {
                match self.pty.writer().write(current.remaining_bytes()) {
                    Ok(0) => {
                        state.set_current(Some(current));
                        break 'write_many;
                    },
                    Ok(n) => {
                        current.advance(n);
                        if current.finished() {
                            state.goto_next();
                            break 'write_one;
                        }
                    },
                    Err(err) => {
                        state.set_current(Some(current));
                        match err.kind() {
                            ErrorKind::Interrupted | ErrorKind::WouldBlock => break 'write_many,
                            _ => return Err(err),
                        }
                    },
                }
            }
        }

        Ok(())
    }

    pub fn spawn(mut self) -> JoinHandle<(Self, State)> {
        thread::spawn_named("PTY reader", move || {
            let mut state = State::default();
            let mut buf = [0u8; READ_BUFFER_SIZE];

            let poll_opts = PollMode::Level;
            let mut interest = PollingEvent::readable(0);

            // Register TTY through EventedRW interface.
            if let Err(err) = unsafe { self.pty.register(&self.poll, interest, poll_opts) } {
                error!("Event loop registration error: {err}");
                return (self, state);
            }

            let mut events = Events::with_capacity(NonZeroUsize::new(1024).unwrap());

            let mut pipe = if self.ref_test {
                Some(File::create("./vivido.recording").expect("create vivido recording"))
            } else {
                None
            };

            'event_loop: loop {
                // Wakeup the event loop when a synchronized update timeout was reached.
                let handler = state.parser.sync_timeout();
                let timeout =
                    handler.sync_timeout().map(|st| st.saturating_duration_since(Instant::now()));

                events.clear();
                if let Err(err) = self.poll.wait(&mut events, timeout) {
                    match err.kind() {
                        ErrorKind::Interrupted => continue,
                        _ => {
                            error!("Event loop polling error: {err}");
                            break 'event_loop;
                        },
                    }
                }

                // Handle synchronized update timeout.
                if events.is_empty() && self.rx.peek().is_none() {
                    state.parser.stop_sync(&mut *self.terminal.lock());
                    self.event_proxy.send_event(Event::Wakeup);
                    continue;
                }

                // Handle channel events, if there are any.
                if !self.drain_recv_channel(&mut state) {
                    break;
                }

                for event in events.iter() {
                    match event.key {
                        tty::PTY_CHILD_EVENT_TOKEN => {
                            if let Some(tty::ChildEvent::Exited(status)) =
                                self.pty.next_child_event()
                            {
                                if let Some(status) = status {
                                    self.event_proxy.send_event(Event::ChildExit(status));
                                }
                                if self.drain_on_exit {
                                    let _ = self.pty_read(&mut state, &mut buf, pipe.as_mut());
                                }
                                self.terminal.lock().exit();
                                self.event_proxy.send_event(Event::Wakeup);
                                break 'event_loop;
                            }
                        },

                        tty::PTY_READ_WRITE_TOKEN => {
                            if event.is_interrupt() {
                                // Don't try to do I/O on a dead PTY.
                                continue;
                            }

                            if event.readable
                                && let Err(err) = self.pty_read(&mut state, &mut buf, pipe.as_mut())
                            {
                                // On Linux, a `read` on the master side of a PTY can fail
                                // with `EIO` if the client side hangs up.  In that case,
                                // just loop back round for the inevitable `Exited` event.
                                // This sucks, but checking the process is either racy or
                                // blocking.
                                #[cfg(target_os = "linux")]
                                if err.raw_os_error() == Some(libc::EIO) {
                                    continue;
                                }

                                error!("Error reading from PTY in event loop: {err}");
                                break 'event_loop;
                            }

                            if event.writable
                                && let Err(err) = self.pty_write(&mut state)
                            {
                                error!("Error writing to PTY in event loop: {err}");
                                break 'event_loop;
                            }
                        },
                        _ => (),
                    }
                }

                // Register write interest if necessary.
                let needs_write = state.needs_write();
                if needs_write != interest.writable {
                    interest.writable = needs_write;

                    // Re-register with new interest.
                    self.pty.reregister(&self.poll, interest, poll_opts).unwrap();
                }
            }

            // The evented instances are not dropped here so deregister them explicitly.
            let _ = self.pty.deregister(&self.poll);

            (self, state)
        })
    }
}

/// Helper type which tracks how much of a buffer has been written.
struct Writing {
    source: Cow<'static, [u8]>,
    written: usize,
}

pub struct Notifier(pub EventLoopSender);

impl event::Notify for Notifier {
    fn notify<B>(&self, bytes: B)
    where
        B: Into<Cow<'static, [u8]>>,
    {
        let bytes = bytes.into();
        // Terminal hangs if we send 0 bytes through.
        if bytes.is_empty() {
            return;
        }

        let _ = self.0.send(Msg::Input(bytes));
    }
}

impl event::OnResize for Notifier {
    fn on_resize(&mut self, window_size: WindowSize) {
        let _ = self.0.send(Msg::Resize(window_size));
    }
}

#[derive(Debug)]
pub enum EventLoopSendError {
    /// Error polling the event loop.
    Io(io::Error),

    /// Error sending a message to the event loop.
    Send(mpsc::SendError<Msg>),
}

impl Display for EventLoopSendError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            EventLoopSendError::Io(err) => err.fmt(f),
            EventLoopSendError::Send(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for EventLoopSendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            EventLoopSendError::Io(err) => err.source(),
            EventLoopSendError::Send(err) => err.source(),
        }
    }
}

#[derive(Clone)]
pub struct EventLoopSender {
    sender: Sender<Msg>,
    poller: Arc<Poller>,
}

impl EventLoopSender {
    pub fn send(&self, msg: Msg) -> Result<(), EventLoopSendError> {
        self.sender.send(msg).map_err(EventLoopSendError::Send)?;
        self.poller.notify().map_err(EventLoopSendError::Io)
    }
}

/// All of the mutable state needed to run the event loop.
///
/// Contains list of items to write, current write state, etc. Anything that
/// would otherwise be mutated on the `EventLoop` goes here.
#[derive(Default)]
pub struct State {
    write_list: VecDeque<Cow<'static, [u8]>>,
    writing: Option<Writing>,
    parser: ansi::Processor,
    vivid_markers: VividMarkerScanner,
}

impl State {
    fn advance<T: EventListener>(&mut self, terminal: &mut Term<T>, bytes: &[u8]) -> usize {
        let mut processed = 0;
        for chunk in self.vivid_markers.push(bytes) {
            match chunk {
                VividChunk::Bytes(bytes) => {
                    processed += bytes.len();
                    self.parser.advance(terminal, &bytes);
                },
                VividChunk::Marker { raw, marker } => {
                    processed += raw.len();
                    #[cfg(not(windows))]
                    self.parser.advance(terminal, &raw);
                    terminal.vivid_marker(marker);
                },
            }
        }
        processed
    }

    #[inline]
    fn ensure_next(&mut self) {
        if self.writing.is_none() {
            self.goto_next();
        }
    }

    #[inline]
    fn goto_next(&mut self) {
        self.writing = self.write_list.pop_front().map(Writing::new);
    }

    #[inline]
    fn take_current(&mut self) -> Option<Writing> {
        self.writing.take()
    }

    #[inline]
    fn needs_write(&self) -> bool {
        self.writing.is_some() || !self.write_list.is_empty()
    }

    #[inline]
    fn set_current(&mut self, new: Option<Writing>) {
        self.writing = new;
    }
}

#[cfg(not(windows))]
const VIVID_MARKER_PREFIX: &[u8] = b"\x1b_VIVID;2;";
#[cfg(windows)]
const VIVID_MARKER_PREFIX: &[u8] = b"VIVID;2;";
#[cfg(not(windows))]
const VIVID_MARKER_TERMINATOR: &[u8] = b"\x1b\\";
#[cfg(windows)]
const VIVID_MARKER_TERMINATOR: &[u8] = b";VIVID-END";
#[cfg(not(windows))]
const VIVID_MARKER_PAYLOAD_SKIP: usize = 2;
#[cfg(windows)]
const VIVID_MARKER_PAYLOAD_SKIP: usize = 0;
#[cfg(not(windows))]
const MAX_VIVID_MARKER_BYTES: usize = 128;
#[cfg(windows)]
const MAX_VIVID_MARKER_BYTES: usize = 160;

enum VividChunk {
    Bytes(Vec<u8>),
    Marker { raw: Vec<u8>, marker: String },
}

#[derive(Default)]
struct VividMarkerScanner {
    pending: Vec<u8>,
}

impl VividMarkerScanner {
    fn push(&mut self, bytes: &[u8]) -> Vec<VividChunk> {
        self.pending.extend_from_slice(bytes);
        let mut chunks = Vec::new();
        let mut cursor = 0;

        loop {
            let Some(relative_start) = find_bytes(&self.pending[cursor..], VIVID_MARKER_PREFIX)
            else {
                let keep = partial_prefix_len(&self.pending[cursor..], VIVID_MARKER_PREFIX);
                let end = self.pending.len().saturating_sub(keep);
                push_bytes(&mut chunks, &self.pending[cursor..end]);
                cursor = end;
                break;
            };
            let start = cursor + relative_start;
            push_bytes(&mut chunks, &self.pending[cursor..start]);
            let payload_start = start + VIVID_MARKER_PAYLOAD_SKIP;
            let terminator_search = start + VIVID_MARKER_PREFIX.len();

            let Some(relative_end) =
                find_bytes(&self.pending[terminator_search..], VIVID_MARKER_TERMINATOR)
            else {
                if self.pending.len() - start > MAX_VIVID_MARKER_BYTES {
                    push_bytes(
                        &mut chunks,
                        &self.pending[start..start + VIVID_MARKER_PREFIX.len()],
                    );
                    cursor = start + VIVID_MARKER_PREFIX.len();
                    continue;
                }
                cursor = start;
                break;
            };

            let terminator = terminator_search + relative_end;
            let end = terminator + VIVID_MARKER_TERMINATOR.len();
            if end - start > MAX_VIVID_MARKER_BYTES {
                push_bytes(&mut chunks, &self.pending[start..start + VIVID_MARKER_PREFIX.len()]);
                cursor = start + VIVID_MARKER_PREFIX.len();
                continue;
            }

            let raw = self.pending[start..end].to_vec();
            match std::str::from_utf8(&self.pending[payload_start..terminator]) {
                Ok(marker) => chunks.push(VividChunk::Marker { raw, marker: marker.to_owned() }),
                Err(_) => push_bytes(&mut chunks, &raw),
            }
            cursor = end;
        }

        self.pending.drain(..cursor);
        chunks
    }
}

fn push_bytes(chunks: &mut Vec<VividChunk>, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    if let Some(VividChunk::Bytes(previous)) = chunks.last_mut() {
        previous.extend_from_slice(bytes);
    } else {
        chunks.push(VividChunk::Bytes(bytes.to_vec()));
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

fn partial_prefix_len(bytes: &[u8], prefix: &[u8]) -> usize {
    (1..prefix.len()).rev().find(|&length| bytes.ends_with(&prefix[..length])).unwrap_or(0)
}

impl Writing {
    #[inline]
    fn new(c: Cow<'static, [u8]>) -> Writing {
        Writing { source: c, written: 0 }
    }

    #[inline]
    fn advance(&mut self, n: usize) {
        self.written += n;
    }

    #[inline]
    fn remaining_bytes(&self) -> &[u8] {
        &self.source[self.written..]
    }

    #[inline]
    fn finished(&self) -> bool {
        self.written >= self.source.len()
    }
}

struct PeekableReceiver<T> {
    rx: Receiver<T>,
    peeked: Option<T>,
}

impl<T> PeekableReceiver<T> {
    fn new(rx: Receiver<T>) -> Self {
        Self { rx, peeked: None }
    }

    fn peek(&mut self) -> Option<&T> {
        if self.peeked.is_none() {
            self.peeked = self.rx.try_recv().ok();
        }

        self.peeked.as_ref()
    }

    fn recv(&mut self) -> Option<T> {
        if self.peeked.is_some() {
            self.peeked.take()
        } else {
            match self.rx.try_recv() {
                Err(TryRecvError::Disconnected) => panic!("event loop channel closed"),
                res => res.ok(),
            }
        }
    }
}

#[cfg(test)]
mod vivid_marker_tests {
    use super::*;

    #[test]
    fn marker_is_recognized_across_every_read_boundary() {
        #[cfg(not(windows))]
        let input = b"before\x1b_VIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA\x1b\\after";
        #[cfg(windows)]
        let input = b"beforeVIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA;VIVID-ENDafter";
        let mut scanner = VividMarkerScanner::default();
        let mut text = Vec::new();
        let mut markers = Vec::new();

        for byte in input {
            for chunk in scanner.push(std::slice::from_ref(byte)) {
                match chunk {
                    VividChunk::Bytes(bytes) => text.extend(bytes),
                    VividChunk::Marker { raw, marker } => {
                        assert!(raw.starts_with(VIVID_MARKER_PREFIX));
                        markers.push(marker);
                    },
                }
            }
        }

        assert_eq!(text, b"beforeafter");
        assert_eq!(
            markers,
            ["VIVID;2;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA"]
        );
        assert!(scanner.pending.is_empty());
    }

    #[test]
    fn oversized_candidate_is_left_to_the_terminal_parser() {
        let mut input = VIVID_MARKER_PREFIX.to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_VIVID_MARKER_BYTES));
        input.extend_from_slice(VIVID_MARKER_TERMINATOR);
        let mut scanner = VividMarkerScanner::default();
        let chunks = scanner.push(&input);
        assert!(chunks.iter().all(|chunk| matches!(chunk, VividChunk::Bytes(_))));
    }
}
