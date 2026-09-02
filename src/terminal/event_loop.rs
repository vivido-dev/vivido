//! The main event loop which performs I/O on the pseudoterminal.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt::{self, Display, Formatter};
use std::fs::File;
use std::io::{self, ErrorKind, Read, Write};
use std::mem;
use std::num::NonZeroUsize;
use std::sync::Arc;
#[cfg(any(unix, windows))]
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::Instant;

use log::error;
use memchr::memmem;
use polling::{Event as PollingEvent, Events, PollMode, Poller};

#[cfg(any(unix, windows))]
use crate::automation::Transcript;
use crate::osc_notification::{OscMessage, OscNotificationParser};
use crate::terminal::event::{self, Event, EventListener, WindowSize};
use crate::terminal::sync::FairMutex;
use crate::terminal::term::Term;
use crate::terminal::{thread, tty};
use vte::ansi;

use crate::client_fault::{self, ClientFault, ClientFaultClass};

/// Max bytes to read from the PTY before forced terminal synchronization.
pub(crate) const READ_BUFFER_SIZE: usize = 0x10_0000;

/// Max bytes to read from the PTY while the terminal is locked.
const MAX_LOCKED_READ: usize = u16::MAX as usize;

/// Messages that may be sent to the `EventLoop`.
#[derive(Debug)]
pub enum Msg {
    /// Data that should be written to the PTY.
    Input {
        bytes: Cow<'static, [u8]>,
        #[cfg(any(unix, windows))]
        completion: Option<u64>,
    },

    /// Indicates that the `EventLoop` should shut down, as Vivido is shutting down.
    Shutdown,

    /// Instruction to resize the PTY.
    Resize {
        window_size: WindowSize,
        #[cfg(any(unix, windows))]
        completion: Option<u64>,
    },

    /// Reset parser and client-controlled terminal state, resuming a quarantined pane.
    #[cfg(any(unix, windows))]
    ResetClient { completion: u64 },
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
    #[cfg(any(unix, windows))]
    transcript: Arc<Mutex<Transcript>>,
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
        #[cfg(any(unix, windows))] transcript: Arc<Mutex<Transcript>>,
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
            #[cfg(any(unix, windows))]
            transcript,
        })
    }

    pub fn channel(&self) -> EventLoopSender {
        EventLoopSender { sender: self.tx.clone(), poller: self.poll.clone() }
    }

    /// Drain the channel.
    ///
    /// Returns `false` when a shutdown message was received.
    fn drain_recv_channel(&mut self, state: &mut State, quarantined: &mut bool) -> bool {
        while let Some(msg) = self.rx.recv() {
            match msg {
                Msg::Input {
                    bytes,
                    #[cfg(any(unix, windows))]
                    completion,
                } => {
                    // A quarantined pane retains its last safe frame, but client traffic must not
                    // continue accumulating behind the fault boundary.
                    if !*quarantined {
                        state.write_list.push_back(PendingInput {
                            bytes,
                            #[cfg(any(unix, windows))]
                            completion,
                        });
                    }
                },
                Msg::Resize {
                    window_size,
                    #[cfg(any(unix, windows))]
                    completion,
                } => {
                    self.pty.on_resize(window_size);
                    #[cfg(any(unix, windows))]
                    if let Some(token) = completion {
                        self.event_proxy.send_event(Event::PtyResizeComplete(token));
                    }
                },
                #[cfg(any(unix, windows))]
                Msg::ResetClient { completion } => {
                    state.reset_client_state();
                    self.terminal.lock().reset_client_state();
                    *quarantined = false;
                    self.event_proxy.send_event(Event::ClientResetComplete(completion));
                    self.event_proxy.send_event(Event::Wakeup);
                },
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
            processed += state.advance(
                &mut **terminal,
                &buf[..unprocessed],
                #[cfg(any(unix, windows))]
                &self.transcript,
            );
            #[cfg(any(unix, windows))]
            if let Some((start, end)) = state.take_output_range() {
                self.event_proxy.send_event(Event::PtyOutput { start, end });
            }
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
                            #[cfg(any(unix, windows))]
                            if let Some(token) = current.completion {
                                self.event_proxy.send_event(Event::PtyWriteComplete(token));
                            }
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
            let mut quarantined = false;
            let mut registered = true;

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
                if !self.drain_recv_channel(&mut state, &mut quarantined) {
                    break;
                }

                if !registered && !quarantined {
                    match unsafe { self.pty.register(&self.poll, interest, poll_opts) } {
                        Ok(()) => registered = true,
                        Err(error) => {
                            let fault = ClientFault::new(
                                ClientFaultClass::PtyIo,
                                "failed to resume quarantined PTY",
                            );
                            error!("contained client fault {}: {error}", fault.id);
                            self.event_proxy.send_event(Event::ClientFault(fault));
                            quarantined = true;
                        },
                    }
                }
                if quarantined {
                    continue;
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

                            if event.readable {
                                let read = client_fault::catch(
                                    ClientFaultClass::TerminalParser,
                                    "terminal output parser panicked",
                                    || self.pty_read(&mut state, &mut buf, pipe.as_mut()),
                                );
                                let failure = match read {
                                    Ok(Ok(())) => None,
                                    Ok(Err(err)) => Some((ClientFaultClass::PtyIo, err)),
                                    Err(fault) => {
                                        error!(
                                            "contained client fault {} ({})",
                                            fault.id,
                                            fault.class.as_str()
                                        );
                                        self.event_proxy.send_event(Event::ClientFault(fault));
                                        quarantined = true;
                                        None
                                    },
                                };
                                if quarantined {
                                    let _ = self.pty.deregister(&self.poll);
                                    registered = false;
                                    break;
                                }
                                if let Some((class, err)) = failure {
                                    // On Linux, a `read` on the master side of a PTY can fail
                                    // with `EIO` if the client side hangs up.  In that case,
                                    // just loop back round for the inevitable `Exited` event.
                                    // This sucks, but checking the process is either racy or
                                    // blocking.
                                    #[cfg(target_os = "linux")]
                                    if err.raw_os_error() == Some(libc::EIO) {
                                        continue;
                                    }

                                    let fault = ClientFault::new(class, "terminal PTY read failed");
                                    error!("contained client fault {}: {err}", fault.id);
                                    self.event_proxy.send_event(Event::ClientFault(fault));
                                    quarantined = true;
                                    let _ = self.pty.deregister(&self.poll);
                                    registered = false;
                                    break;
                                }
                            }

                            if event.writable {
                                match client_fault::catch(
                                    ClientFaultClass::PtyIo,
                                    "terminal PTY writer panicked",
                                    || self.pty_write(&mut state),
                                ) {
                                    Ok(Ok(())) => {},
                                    Ok(Err(err)) => {
                                        let fault = ClientFault::new(
                                            ClientFaultClass::PtyIo,
                                            "terminal PTY write failed",
                                        );
                                        error!("contained client fault {}: {err}", fault.id);
                                        self.event_proxy.send_event(Event::ClientFault(fault));
                                        quarantined = true;
                                    },
                                    Err(fault) => {
                                        error!(
                                            "contained client fault {} ({})",
                                            fault.id,
                                            fault.class.as_str()
                                        );
                                        self.event_proxy.send_event(Event::ClientFault(fault));
                                        quarantined = true;
                                    },
                                }
                                if quarantined {
                                    let _ = self.pty.deregister(&self.poll);
                                    registered = false;
                                    break;
                                }
                            }
                        },
                        _ => (),
                    }
                }

                // Register write interest if necessary.
                let needs_write = state.needs_write();
                let write_interest_changed = needs_write != interest.writable;
                if write_interest_changed {
                    interest.writable = needs_write;
                }

                // Windows emulates readiness for its blocking ConPTY pipes by posting an IOCP
                // packet. `pty_read` deliberately stops at `MAX_LOCKED_READ` for fairness, and
                // when that leaves the intermediary pipe nonempty no empty-read occurs to install
                // another waker. Re-registering posts the next packet for the still-readable pipe;
                // native pollers remain level-triggered and only need updates when interest
                // changes.
                if (write_interest_changed || cfg!(windows))
                    && let Err(err) = self.pty.reregister(&self.poll, interest, poll_opts)
                {
                    let fault = ClientFault::new(
                        ClientFaultClass::PtyIo,
                        "terminal PTY registration failed",
                    );
                    error!("contained client fault {}: {err}", fault.id);
                    self.event_proxy.send_event(Event::ClientFault(fault));
                    quarantined = true;
                    let _ = self.pty.deregister(&self.poll);
                    registered = false;
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
    #[cfg(any(unix, windows))]
    completion: Option<u64>,
}

struct PendingInput {
    bytes: Cow<'static, [u8]>,
    #[cfg(any(unix, windows))]
    completion: Option<u64>,
}

pub struct Notifier(pub EventLoopSender);

impl event::Notify for Notifier {
    fn notify<B>(&self, bytes: B) -> Result<(), EventLoopSendError>
    where
        B: Into<Cow<'static, [u8]>>,
    {
        let bytes = bytes.into();
        // Terminal hangs if we send 0 bytes through.
        if bytes.is_empty() {
            return Ok(());
        }

        self.0.send(Msg::Input {
            bytes,
            #[cfg(any(unix, windows))]
            completion: None,
        })
    }
}

impl event::OnResize for Notifier {
    fn on_resize(&mut self, window_size: WindowSize) {
        let _ = self.0.send(Msg::Resize {
            window_size,
            #[cfg(any(unix, windows))]
            completion: None,
        });
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
    write_list: VecDeque<PendingInput>,
    writing: Option<Writing>,
    parser: ansi::Processor,
    osc_notifications: OscNotificationParser,
    vivid_markers: VividMarkerScanner,
    #[cfg(any(unix, windows))]
    output_range: Option<(u64, u64)>,
}

impl State {
    fn reset_client_state(&mut self) {
        self.write_list.clear();
        self.writing = None;
        self.parser = Default::default();
        self.osc_notifications = Default::default();
        self.vivid_markers = Default::default();
        #[cfg(any(unix, windows))]
        {
            self.output_range = None;
        }
    }

    fn advance<T: EventListener>(
        &mut self,
        terminal: &mut Term<T>,
        bytes: &[u8],
        #[cfg(any(unix, windows))] transcript: &Arc<Mutex<Transcript>>,
    ) -> usize {
        for message in self.osc_notifications.advance(bytes) {
            match message {
                OscMessage::Notification(notification) => {
                    terminal.desktop_notification(notification);
                },
                OscMessage::WorkingDirectory(report) => {
                    terminal.working_directory_report(report);
                },
            }
        }

        let mut processed = 0;
        // The scanner moves out of `self` so its spans can borrow the read buffer while the
        // parser and output range are still mutated for each one.
        let mut scanner = mem::take(&mut self.vivid_markers);
        let parser = &mut self.parser;
        #[cfg(any(unix, windows))]
        let output_range = &mut self.output_range;
        scanner.push(bytes, &mut |chunk| match chunk {
            VividChunk::Bytes(bytes) => {
                processed += bytes.len();
                #[cfg(any(unix, windows))]
                {
                    let (start, end) = transcript.lock().unwrap().append(bytes);
                    *output_range = Some(match *output_range {
                        Some((existing, _)) => (existing, end),
                        None => (start, end),
                    });
                }
                terminal.advance(parser, bytes);
            },
            VividChunk::Marker { raw, marker, pass_to_terminal: _pass_to_terminal } => {
                processed += raw.len();
                #[cfg(not(windows))]
                if _pass_to_terminal {
                    terminal.advance(parser, raw);
                }
                terminal.vivid_marker(marker.to_owned());
            },
        });
        self.vivid_markers = scanner;
        processed
    }

    #[cfg(any(unix, windows))]
    fn take_output_range(&mut self) -> Option<(u64, u64)> {
        self.output_range.take()
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

const MAX_VIVID_MARKER_BYTES: usize = vivid_protocol::anchor::MAX_MARKER_BYTES;

#[derive(Clone, Copy)]
struct VividMarkerEnvelope {
    prefix: &'static [u8],
    terminator: &'static [u8],
    payload_skip: usize,
    pass_to_terminal: bool,
}

const APC_VIVID_MARKER: VividMarkerEnvelope = VividMarkerEnvelope {
    prefix: b"\x1b_VIVID;3;",
    terminator: b"\x1b\\",
    payload_skip: 2,
    pass_to_terminal: true,
};
const CONPTY_VIVID_MARKER: VividMarkerEnvelope = VividMarkerEnvelope {
    prefix: b"VIVID;3;",
    terminator: b";VIVID-END",
    payload_skip: 0,
    pass_to_terminal: false,
};
const VIVID_MARKER_ENVELOPES: [VividMarkerEnvelope; 2] = [APC_VIVID_MARKER, CONPTY_VIVID_MARKER];

/// One span of scanned PTY bytes: ordinary terminal data, or one authenticated anchor marker.
///
/// Spans borrow either the caller's read buffer or the scanner's own bounded `pending` buffer, so
/// ordinary output reaches the terminal without being copied.
enum VividChunk<'a> {
    Bytes(&'a [u8]),
    Marker { raw: &'a [u8], marker: &'a str, pass_to_terminal: bool },
}

#[derive(Default)]
struct VividMarkerScanner {
    pending: Vec<u8>,
}

impl VividMarkerScanner {
    /// Separate authenticated anchor markers from ordinary terminal bytes.
    ///
    /// A read holding no marker is forwarded as a borrowed slice of the caller's buffer; only the
    /// tail of a marker split across two reads is ever copied into `pending`, which stays bounded
    /// by [`MAX_VIVID_MARKER_BYTES`].
    fn push<F>(&mut self, bytes: &[u8], emit: &mut F)
    where
        F: FnMut(VividChunk<'_>),
    {
        if self.pending.is_empty() {
            let consumed = scan_markers(bytes, emit);
            self.pending.extend_from_slice(&bytes[consumed..]);
        } else {
            self.pending.extend_from_slice(bytes);
            let consumed = scan_markers(&self.pending, emit);
            self.pending.drain(..consumed);
        }
    }
}

/// Emit every complete chunk in `buf`, returning how many leading bytes were consumed.
///
/// The unconsumed tail is a partial prefix or an unterminated marker still short enough to
/// complete in a later read.
fn scan_markers<F>(buf: &[u8], emit: &mut F) -> usize
where
    F: FnMut(VividChunk<'_>),
{
    let mut cursor = 0;

    loop {
        let Some((relative_start, envelope)) = find_marker_envelope(&buf[cursor..]) else {
            let keep = VIVID_MARKER_ENVELOPES
                .iter()
                .map(|envelope| partial_prefix_len(&buf[cursor..], envelope.prefix))
                .max()
                .unwrap_or(0);
            let end = buf.len().saturating_sub(keep);
            emit_bytes(emit, &buf[cursor..end]);
            return end;
        };
        let start = cursor + relative_start;
        emit_bytes(emit, &buf[cursor..start]);
        let payload_start = start + envelope.payload_skip;
        let terminator_search = start + envelope.prefix.len();

        let Some(relative_end) = find_bytes(&buf[terminator_search..], envelope.terminator) else {
            if buf.len() - start > MAX_VIVID_MARKER_BYTES {
                emit_bytes(emit, &buf[start..start + envelope.prefix.len()]);
                cursor = start + envelope.prefix.len();
                continue;
            }
            return start;
        };

        let terminator = terminator_search + relative_end;
        let end = terminator + envelope.terminator.len();
        if end - start > MAX_VIVID_MARKER_BYTES {
            emit_bytes(emit, &buf[start..start + envelope.prefix.len()]);
            cursor = start + envelope.prefix.len();
            continue;
        }

        match std::str::from_utf8(&buf[payload_start..terminator]) {
            Ok(marker) => emit(VividChunk::Marker {
                raw: &buf[start..end],
                marker,
                pass_to_terminal: envelope.pass_to_terminal,
            }),
            Err(_) => emit_bytes(emit, &buf[start..end]),
        }
        cursor = end;
    }
}

fn find_marker_envelope(bytes: &[u8]) -> Option<(usize, VividMarkerEnvelope)> {
    VIVID_MARKER_ENVELOPES
        .iter()
        .filter_map(|envelope| find_bytes(bytes, envelope.prefix).map(|start| (start, *envelope)))
        .min_by_key(|(start, _)| *start)
}

fn emit_bytes<F>(emit: &mut F, bytes: &[u8])
where
    F: FnMut(VividChunk<'_>),
{
    if !bytes.is_empty() {
        emit(VividChunk::Bytes(bytes));
    }
}

/// Substring search over PTY output.
///
/// `memmem` selects rare bytes and vectorizes; comparing every offset in software instead showed up
/// directly in the throughput of ordinary terminal traffic.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memmem::find(haystack, needle)
}

fn partial_prefix_len(bytes: &[u8], prefix: &[u8]) -> usize {
    (1..prefix.len()).rev().find(|&length| bytes.ends_with(&prefix[..length])).unwrap_or(0)
}

impl Writing {
    #[inline]
    fn new(input: PendingInput) -> Writing {
        Writing {
            source: input.bytes,
            written: 0,
            #[cfg(any(unix, windows))]
            completion: input.completion,
        }
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

    const MARKER_PAYLOAD: &[u8] =
        b"A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA";
    const MARKER_BODY: &str =
        "VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA";

    #[test]
    fn both_marker_envelopes_are_recognized_across_every_read_boundary() {
        assert_marker_envelope(APC_VIVID_MARKER);
        assert_marker_envelope(CONPTY_VIVID_MARKER);
    }

    fn assert_marker_envelope(envelope: VividMarkerEnvelope) {
        let mut input = b"before".to_vec();
        input.extend_from_slice(envelope.prefix);
        input.extend_from_slice(MARKER_PAYLOAD);
        input.extend_from_slice(envelope.terminator);
        input.extend_from_slice(b"after");
        let mut scanner = VividMarkerScanner::default();
        let mut text = Vec::new();
        let mut markers = Vec::new();

        for byte in &input {
            scanner.push(std::slice::from_ref(byte), &mut |chunk| match chunk {
                VividChunk::Bytes(bytes) => text.extend_from_slice(bytes),
                VividChunk::Marker { raw, marker, pass_to_terminal } => {
                    assert!(raw.starts_with(envelope.prefix));
                    markers.push((marker.to_owned(), pass_to_terminal));
                },
            });
        }

        assert_eq!(text, b"beforeafter");
        assert_eq!(markers, [(MARKER_BODY.to_owned(), envelope.pass_to_terminal)]);
        assert!(scanner.pending.is_empty());
    }

    #[test]
    fn printable_conpty_envelope_is_removed_instead_of_reaching_terminal_text() {
        let mut input = CONPTY_VIVID_MARKER.prefix.to_vec();
        input.extend_from_slice(MARKER_PAYLOAD);
        input.extend_from_slice(CONPTY_VIVID_MARKER.terminator);
        let mut printable = 0;
        let mut markers = 0;
        VividMarkerScanner::default().push(&input, &mut |chunk| match chunk {
            VividChunk::Bytes(_) => printable += 1,
            VividChunk::Marker { pass_to_terminal, .. } => {
                assert!(!pass_to_terminal);
                markers += 1;
            },
        });

        assert_eq!((printable, markers), (0, 1));
    }

    #[test]
    fn oversized_candidates_are_left_to_the_terminal_parser() {
        for envelope in VIVID_MARKER_ENVELOPES {
            let mut input = envelope.prefix.to_vec();
            input.extend(std::iter::repeat_n(b'x', MAX_VIVID_MARKER_BYTES));
            input.extend_from_slice(envelope.terminator);
            let mut scanner = VividMarkerScanner::default();
            scanner.push(&input, &mut |chunk| {
                assert!(matches!(chunk, VividChunk::Bytes(_)));
            });
        }
    }

    #[test]
    fn reset_discards_partial_parsers_and_pending_client_input() {
        let mut state = State::default();
        state.vivid_markers.push(b"\x1b_VIVID;3;partial", &mut |_| {});
        state.write_list.push_back(PendingInput {
            bytes: Cow::Borrowed(b"pending"),
            #[cfg(any(unix, windows))]
            completion: Some(7),
        });
        state.writing = Some(Writing::new(PendingInput {
            bytes: Cow::Borrowed(b"writing"),
            #[cfg(any(unix, windows))]
            completion: Some(8),
        }));
        #[cfg(any(unix, windows))]
        {
            state.output_range = Some((1, 2));
        }

        state.reset_client_state();

        assert!(state.vivid_markers.pending.is_empty());
        assert!(state.write_list.is_empty());
        assert!(state.writing.is_none());
        #[cfg(any(unix, windows))]
        assert!(state.output_range.is_none());
    }
}
