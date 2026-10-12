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
use crate::terminal::graphics::QueuedJob;
use crate::terminal::sync::FairMutex;
use crate::terminal::term::Term;
use crate::terminal::{thread, tty};
use vvte::ansi;

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
        /// Payload bytes or the declared payload byte count.
        bytes: Cow<'static, [u8]>,
        #[cfg(any(unix, windows))]
        /// Optional completion token echoed after the operation finishes.
        completion: Option<u64>,
    },

    /// Indicates that the `EventLoop` should shut down, as Vivido is shutting down.
    Shutdown,

    /// Instruction to resize the PTY.
    Resize {
        /// Requested terminal grid and cell geometry.
        window_size: WindowSize,
        #[cfg(any(unix, windows))]
        /// Optional completion token echoed after the operation finishes.
        completion: Option<u64>,
    },

    /// Reset parser and client-controlled terminal state in a healthy pane.
    #[cfg(any(unix, windows))]
    ResetClient {
        /// Optional completion token echoed after the operation finishes.
        completion: u64,
    },
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
    ///
    /// # Errors
    ///
    /// Returns an I/O error if poller creation or PTY registration fails.
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

    /// Clone the PTY-worker command sender.
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
                    if *quarantined {
                        continue;
                    }
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
        // Bytes of `processed` that were image data still accumulating, which show nothing yet.
        let mut quiet = 0;
        let mut graphics_changed = false;

        // Reserve the next terminal lock for PTY reading.
        let mut _terminal_lease = Some(self.terminal.lease());
        let mut terminal_slot = None;

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
            let terminal = match &mut terminal_slot {
                Some(terminal) => terminal,
                None => terminal_slot.insert(match self.terminal.try_lock_unfair() {
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
            let synchronized = state.parser.sync_bytes_count() != 0;
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

            let (quiet_bytes, changed) = terminal.take_graphics_activity();
            graphics_changed |= changed;
            // The count is exact only when every byte it covers arrived in this call. A
            // synchronized update replays input from earlier calls, so then it is not used and
            // the wakeup below falls back to treating everything as visible.
            if !synchronized && state.parser.sync_bytes_count() == 0 {
                quiet += quiet_bytes;
            }

            if terminal.has_graphics_jobs() {
                let jobs = terminal.take_graphics_jobs();
                // Decoding a compressed image is the one slow step in receiving it, and needs
                // nothing from the terminal. Give the terminal up, lease included, so the UI can
                // keep drawing: it would otherwise wait out the decode on its next frame.
                terminal_slot = None;
                _terminal_lease = None;
                let finished = jobs.into_iter().map(QueuedJob::run).collect();
                _terminal_lease = Some(self.terminal.lease());
                terminal_slot.insert(self.terminal.lock_unfair()).finish_graphics_jobs(finished);
                graphics_changed = true;
            }

            // Assure we're not blocking the terminal too long unnecessarily.
            if processed >= MAX_LOCKED_READ {
                break;
            }
        }

        if wakeup_needed(processed, state.parser.sync_bytes_count(), quiet, graphics_changed) {
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

    /// Start the owned PTY polling worker.
    ///
    /// # Panics
    ///
    /// Panics if the operating system cannot spawn the PTY worker thread.
    pub fn spawn(mut self) -> JoinHandle<(Self, State)> {
        thread::spawn_named("PTY reader", move || {
            let mut state = State::default();
            let mut buf = vec![0u8; READ_BUFFER_SIZE].into_boxed_slice();

            // This loop runs queued image decodes itself, after each read and without the lock.
            self.terminal.lock_unfair().set_deferred_graphics_decode(true);

            let poll_opts = PollMode::Level;
            let mut interest = PollingEvent::readable(0);
            let mut quarantined = false;
            let mut registered = true;

            // Register TTY through EventedRW interface.
            // SAFETY: this worker owns the PTY until deregistration and outlives every poll.
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
                    // SAFETY: the worker still owns this live PTY, and deregisters it before drop.
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
                                        // Discard pending writes, parser buffers, and marker fragments immediately.
                                        state.reset_client_state();
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
                                        // Discard pending writes, parser buffers, and marker fragments immediately.
                                        state.reset_client_state();
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

#[derive(Debug)]
/// A terminal input notifier backed by the PTY worker channel.
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
/// A failure to queue a command for the PTY worker.
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

#[derive(Debug, Clone)]
/// A clonable PTY-worker command sender that wakes the polling loop.
pub struct EventLoopSender {
    sender: Sender<Msg>,
    poller: Arc<Poller>,
}

impl EventLoopSender {
    /// Queue a PTY command and wake its polling loop.
    ///
    /// # Errors
    ///
    /// Returns `EventLoopSendError` if the worker channel closes or its poller cannot be awakened.
    pub fn send(&self, msg: Msg) -> Result<(), EventLoopSendError> {
        self.sender.send(msg).map_err(EventLoopSendError::Send)?;
        self.poller.notify().map_err(EventLoopSendError::Io)
    }
}

// Debug omits user content and native resources, and never acquires application locks.
impl<T: tty::EventedPty, U: EventListener> std::fmt::Debug for EventLoop<T, U> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventLoop").finish_non_exhaustive()
    }
}

// Debug omits user content and native resources, and never acquires application locks.
impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod terminal_reply_tests {
    use super::*;
    use crate::event::{EventProxy, EventSink, EventType};
    use winit::window::WindowId;

    fn channel() -> (EventLoopSender, Receiver<Msg>) {
        let (sender, receiver) = mpsc::channel();
        (EventLoopSender { sender, poller: Arc::new(Poller::new().unwrap()) }, receiver)
    }

    fn expect_reply(receiver: &Receiver<Msg>, expected: &[u8]) {
        let Msg::Input { bytes, completion } = receiver.try_recv().unwrap() else {
            panic!("expected terminal reply");
        };
        assert_eq!(bytes.as_ref(), expected);
        assert_eq!(completion, None);
    }

    #[test]
    fn parser_and_writer_faults_discard_partial_state_without_touching_another_owner() {
        use crate::terminal::event::VoidListener;
        use crate::terminal::index::{Column, Line};
        use crate::terminal::term::Config;
        use crate::terminal::term::test::TermSize;
        let size = TermSize::new(12, 2);
        for class in [ClientFaultClass::TerminalParser, ClientFaultClass::PtyIo] {
            let mut failed = Term::new(Config::default(), &size, VoidListener);
            let mut failed_state = State::default();
            let mut surviving = Term::new(Config::default(), &size, VoidListener);
            let mut surviving_state = State::default();
            surviving_state.advance_test_chunks(&mut surviving, [b"other"]);
            let fault = client_fault::catch(class, "injected terminal worker failure", || {
                failed_state.advance_test_chunks(&mut failed, [b"partial"]);
                failed_state.write_list.push_back(PendingInput {
                    bytes: Cow::Borrowed(b"pending"),
                    #[cfg(any(unix, windows))]
                    completion: Some(9),
                });
                panic!("partial parser or writer mutation");
            });
            assert!(fault.is_err());
            // The worker clears pending parser/write state; its owner replaces the terminal.
            failed_state.reset_client_state();
            failed = Term::new(Config::default(), &size, VoidListener);
            assert!(!failed_state.needs_write());
            failed_state.advance_test_chunks(&mut failed, [b"fresh"]);
            assert_eq!(failed.grid()[Line(0)][Column(0)].c, 'f');
            assert_eq!(surviving.grid()[Line(0)][Column(0)].c, 'o');
            assert_eq!(surviving.grid()[Line(0)][Column(4)].c, 'r');
            surviving_state.advance_test_chunks(&mut surviving, [b"!"]);
            assert_eq!(surviving.grid()[Line(0)][Column(5)].c, '!');
        }
    }

    #[test]
    fn replies_bypass_ui_and_remain_ordered() {
        let (sink, ui) = EventSink::headless();
        let proxy = EventProxy::new(sink, WindowId::dummy());
        let (writer, replies) = channel();
        proxy.set_pty_writer(writer);
        for text in ["\x1b[0n", "\x1b[3;4R", ""] {
            EventListener::send_event(&proxy, Event::PtyWrite(text.into()));
        }
        expect_reply(&replies, b"\x1b[0n");
        expect_reply(&replies, b"\x1b[3;4R");
        assert!(replies.try_recv().is_err());
        assert!(ui.try_recv().is_err());
    }

    #[test]
    fn replacing_one_owner_writer_leaves_another_owner_intact() {
        let (sink, ui) = EventSink::headless();
        let first = EventProxy::new(sink.clone(), WindowId::dummy());
        let second = EventProxy::new(sink, WindowId::dummy());
        let first_parser = first.clone();
        let (writer, old_replies) = channel();
        first.set_pty_writer(writer);
        let (writer, other_replies) = channel();
        second.set_pty_writer(writer);
        EventListener::send_event(&first_parser, Event::PtyWrite("old".into()));
        expect_reply(&old_replies, b"old");

        let (writer, new_replies) = channel();
        first.set_pty_writer(writer);
        EventListener::send_event(&first_parser, Event::PtyWrite("new".into()));
        EventListener::send_event(&second, Event::PtyWrite("other".into()));
        expect_reply(&new_replies, b"new");
        expect_reply(&other_replies, b"other");
        assert!(old_replies.try_recv().is_err());
        assert!(new_replies.try_recv().is_err());
        assert!(other_replies.try_recv().is_err());
        assert!(ui.try_recv().is_err());
    }

    #[test]
    fn unattached_proxy_preserves_event_listener_contract() {
        let (sink, ui) = EventSink::headless();
        let proxy = EventProxy::new(sink, WindowId::dummy());
        EventListener::send_event(&proxy, Event::PtyWrite("reply".into()));
        assert!(matches!(ui.try_recv().unwrap().payload(),
            EventType::Terminal(Event::PtyWrite(text)) if text == "reply"));
    }
}

/// All of the mutable state needed to run the event loop.
///
/// Whether a PTY read left the UI anything new to draw.
///
/// Of the `processed` bytes, `synchronized` are held back by a synchronized update and `quiet`
/// were image data that has not yet added up to an image. A read made of nothing else changed
/// nothing on screen, and waking the UI for it would redraw an identical frame; at the megabytes
/// per second an image transfer runs at, that is a redraw per display refresh for nothing.
fn wakeup_needed(
    processed: usize,
    synchronized: usize,
    quiet: usize,
    graphics_changed: bool,
) -> bool {
    processed > 0 && (graphics_changed || synchronized.saturating_add(quiet) < processed)
}

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
    /// Exercise the complete input pipeline in presenter integration tests.
    #[cfg(test)]
    pub(crate) fn advance_test_chunks<T: EventListener>(
        &mut self,
        terminal: &mut Term<T>,
        chunks: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) {
        #[cfg(any(unix, windows))]
        let transcript = Arc::new(Mutex::new(Transcript::default()));
        for chunk in chunks {
            self.advance(
                terminal,
                chunk.as_ref(),
                #[cfg(any(unix, windows))]
                &transcript,
            );
        }
    }

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
                OscMessage::ShellIntegration(marker) => {
                    terminal.shell_integration_report(marker);
                },
                OscMessage::Progress(report) => {
                    terminal.progress_report(report);
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
            // Parsed like everything else, but kept out of the transcript: image data is megabytes
            // of base64 that would push every line of real output out of a 1 MiB ring, and
            // announcing it as output would wake the UI for bytes that show nothing.
            VividChunk::Graphics(bytes) => {
                processed += bytes.len();
                terminal.advance(parser, bytes);
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
#[cfg(test)]
const VIVID_MARKER_ENVELOPES: [VividMarkerEnvelope; 2] = [APC_VIVID_MARKER, CONPTY_VIVID_MARKER];

/// One span of scanned PTY bytes: ordinary terminal data, one authenticated anchor marker, or
/// part of a Kitty graphics command.
///
/// Spans borrow either the caller's read buffer or the scanner's own bounded `pending` buffer, so
/// ordinary output reaches the terminal without being copied.
enum VividChunk<'a> {
    Bytes(&'a [u8]),
    Marker {
        raw: &'a [u8],
        marker: &'a str,
        pass_to_terminal: bool,
    },
    /// Bytes of an `ESC _ G … ESC \` command, introducer and terminator included.
    Graphics(&'a [u8]),
}

/// Introducer of a Kitty graphics command.
const GRAPHICS_INTRODUCER: &[u8] = b"\x1b_G";

/// Where a Kitty graphics command that has begun ends within a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GraphicsEnd {
    /// It ends at this offset; what follows is ordinary input.
    Closed(usize),
    /// The buffer ends on an ESC at this offset that may be the terminator's first byte.
    Undecided(usize),
    /// It runs past the end of the buffer.
    Open,
}

/// Find the end of the graphics command that `buf` continues.
///
/// This mirrors what the parser does with an application program command: it runs to the next
/// ESC, CAN or SUB. An ESC followed by `\` is the terminator and belongs to the command; any other
/// byte there cancels the command and is itself ordinary input.
fn graphics_end(buf: &[u8]) -> GraphicsEnd {
    match memchr::memchr3(0x1B, 0x18, 0x1A, buf) {
        None => GraphicsEnd::Open,
        Some(at) if buf[at] != 0x1B => GraphicsEnd::Closed(at),
        Some(at) => match buf.get(at + 1) {
            None => GraphicsEnd::Undecided(at),
            Some(b'\\') => GraphicsEnd::Closed(at + 2),
            Some(_) => GraphicsEnd::Closed(at),
        },
    }
}

#[derive(Default)]
struct VividMarkerScanner {
    pending: Vec<u8>,
    /// Whether the input so far ends inside a Kitty graphics command.
    graphics: bool,
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
            let consumed = self.scan(bytes, emit);
            self.pending.extend_from_slice(&bytes[consumed..]);
        } else {
            self.pending.extend_from_slice(bytes);
            let pending = mem::take(&mut self.pending);
            let consumed = self.scan(&pending, emit);
            self.pending = pending;
            self.pending.drain(..consumed);
        }
    }

    /// Emit every complete chunk in `buf`, returning how many leading bytes were consumed.
    ///
    /// A graphics command left open by the previous call is continued first. Its data is handed
    /// on as it arrives, never accumulated: the only byte ever held back for it is a trailing ESC
    /// whose meaning the next read decides.
    fn scan<F>(&mut self, buf: &[u8], emit: &mut F) -> usize
    where
        F: FnMut(VividChunk<'_>),
    {
        let mut start = 0;
        if self.graphics {
            match graphics_end(buf) {
                GraphicsEnd::Open => {
                    emit_graphics(emit, buf);
                    return buf.len();
                },
                GraphicsEnd::Undecided(at) => {
                    emit_graphics(emit, &buf[..at]);
                    return at;
                },
                GraphicsEnd::Closed(end) => {
                    emit_graphics(emit, &buf[..end]);
                    self.graphics = false;
                    start = end;
                },
            }
        }
        let (consumed, graphics) = scan_markers(&buf[start..], emit);
        self.graphics = graphics;
        start + consumed
    }
}

/// Emit every complete chunk in `buf`, returning how many leading bytes were consumed and whether
/// the consumed input ends inside a Kitty graphics command.
///
/// The unconsumed tail is a partial prefix or an unterminated marker still short enough to
/// complete in a later read.
fn scan_markers<F>(buf: &[u8], emit: &mut F) -> (usize, bool)
where
    F: FnMut(VividChunk<'_>),
{
    let mut cursor = 0;
    let mut text_start = 0;
    // Cache each search independently: a rejected candidate must not cause another scan of the
    // remainder for the other envelope. Searching for APC's two-byte prefix also skips ordinary
    // CSI/OSC escapes without inspecting each one in the marker loop.
    let mut apc_start = find_bytes(buf, b"\x1b_");
    let mut conpty_start = memchr::memchr(b'V', buf);

    loop {
        if apc_start.is_some_and(|start| start < cursor) {
            apc_start = find_bytes(&buf[cursor..], b"\x1b_").map(|start| cursor + start);
        }
        if conpty_start.is_some_and(|start| start < cursor) {
            conpty_start = memchr::memchr(b'V', &buf[cursor..]).map(|start| cursor + start);
        }
        let (start, envelope) = match (apc_start, conpty_start) {
            (Some(apc), Some(conpty)) if apc < conpty => (apc, APC_VIVID_MARKER),
            (_, Some(conpty)) => (conpty, CONPTY_VIVID_MARKER),
            (Some(apc), None) => (apc, APC_VIVID_MARKER),
            (None, None) => {
                // The first APC prefix byte can straddle reads.
                let keep = usize::from(buf[cursor..].ends_with(b"\x1b"));
                let end = buf.len() - keep;
                emit_bytes(emit, &buf[text_start..end]);
                return (end, false);
            },
        };
        if envelope.payload_skip != 0 && !buf[start..].starts_with(envelope.prefix) {
            if envelope.prefix.starts_with(&buf[start..]) {
                emit_bytes(emit, &buf[text_start..start]);
                return (start, false);
            }
            if buf[start..].starts_with(GRAPHICS_INTRODUCER) {
                emit_bytes(emit, &buf[text_start..start]);
                let body = start + GRAPHICS_INTRODUCER.len();
                match graphics_end(&buf[body..]) {
                    GraphicsEnd::Open => {
                        emit_graphics(emit, &buf[start..]);
                        return (buf.len(), true);
                    },
                    GraphicsEnd::Undecided(at) => {
                        emit_graphics(emit, &buf[start..body + at]);
                        return (body + at, true);
                    },
                    GraphicsEnd::Closed(end) => {
                        emit_graphics(emit, &buf[start..body + end]);
                        cursor = body + end;
                        text_start = cursor;
                        continue;
                    },
                }
            }
            cursor = start + 1;
            continue;
        }
        if envelope.payload_skip == 0 {
            use vivid_protocol::anchor::conpty::{self, Scan};
            match conpty::scan(&buf[start..]) {
                Scan::Complete { consumed, body } => {
                    let end = start + consumed;
                    emit_bytes(emit, &buf[text_start..start]);
                    emit(VividChunk::Marker {
                        raw: &buf[start..end],
                        marker: &body,
                        pass_to_terminal: false,
                    });
                    cursor = end;
                    text_start = end;
                },
                Scan::Incomplete => {
                    emit_bytes(emit, &buf[text_start..start]);
                    return (start, false);
                },
                Scan::Invalid => cursor = start + 1,
            }
            continue;
        }
        let payload_start = start + envelope.payload_skip;
        let terminator_search = start + envelope.prefix.len();

        // Search only the bounded marker candidate, not the entire remaining PTY read.
        let candidate_end = start + (buf.len() - start).min(MAX_VIVID_MARKER_BYTES);
        let Some(relative_end) =
            find_bytes(&buf[terminator_search..candidate_end], envelope.terminator)
        else {
            if buf.len() - start > MAX_VIVID_MARKER_BYTES {
                cursor = start + envelope.prefix.len();
                continue;
            }
            emit_bytes(emit, &buf[text_start..start]);
            return (start, false);
        };

        let terminator = terminator_search + relative_end;
        let end = terminator + envelope.terminator.len();
        if let Ok(marker) = std::str::from_utf8(&buf[payload_start..terminator]) {
            emit_bytes(emit, &buf[text_start..start]);
            emit(VividChunk::Marker {
                raw: &buf[start..end],
                marker,
                pass_to_terminal: envelope.pass_to_terminal,
            });
            text_start = end;
        }
        cursor = end;
    }
}

fn emit_bytes<F>(emit: &mut F, bytes: &[u8])
where
    F: FnMut(VividChunk<'_>),
{
    if !bytes.is_empty() {
        emit(VividChunk::Bytes(bytes));
    }
}

fn emit_graphics<F>(emit: &mut F, bytes: &[u8])
where
    F: FnMut(VividChunk<'_>),
{
    if !bytes.is_empty() {
        emit(VividChunk::Graphics(bytes));
    }
}

/// Substring search over PTY output.
///
/// `memmem` selects rare bytes and vectorizes; comparing every offset in software instead showed up
/// directly in the throughput of ordinary terminal traffic.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    memmem::find(haystack, needle)
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
#[path = "parser_benchmark.rs"]
mod parser_benchmark;

#[cfg(test)]
mod graphics_tests {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;

    use super::*;
    use crate::terminal::event::VoidListener;
    use crate::terminal::term::Config;
    use crate::terminal::term::test::TermSize;

    fn pipeline() -> (Term<VoidListener>, State, Arc<Mutex<Transcript>>) {
        let term = Term::new(Config::default(), &TermSize::new(80, 24), VoidListener);
        (term, State::default(), Arc::new(Mutex::new(Transcript::default())))
    }

    /// Run `bytes` through the whole input pipeline as one PTY read, and report what the read
    /// loop would conclude from it: bytes processed, quiet bytes, and whether to wake the UI.
    fn read(
        (term, state, transcript): &mut (Term<VoidListener>, State, Arc<Mutex<Transcript>>),
        bytes: &[u8],
    ) -> (usize, usize, bool) {
        let synchronized = state.parser.sync_bytes_count() != 0;
        let processed = state.advance(term, bytes, transcript);
        let (mut quiet, changed) = term.take_graphics_activity();
        if synchronized || state.parser.sync_bytes_count() != 0 {
            quiet = 0;
        }
        let wake = wakeup_needed(processed, state.parser.sync_bytes_count(), quiet, changed);
        (processed, quiet, wake)
    }

    /// One 64x64 frame in three chunks, as kitim sends video.
    fn frame() -> [Vec<u8>; 3] {
        let data = STANDARD.encode([7, 7, 7, 255].repeat(64 * 64));
        let chunk =
            |control: &str, payload: &str| format!("\x1b_G{control};{payload}\x1b\\").into_bytes();
        [
            chunk("a=T,f=32,s=64,v=64,C=1,q=2,m=1", &data[..4096]),
            chunk("q=2,m=1", &data[4096..8192]),
            chunk("q=2,m=0", &data[8192..]),
        ]
    }

    #[test]
    fn image_data_wakes_the_ui_only_when_it_completes_an_image() {
        let mut pipeline = pipeline();
        let [first, second, last] = frame();
        assert_eq!(read(&mut pipeline, &first), (first.len(), first.len(), false));
        assert_eq!(read(&mut pipeline, &second), (second.len(), second.len(), false));
        assert_eq!(read(&mut pipeline, &last), (last.len(), last.len(), true));
        assert_eq!(pipeline.0.graphics().store().placements().len(), 1);
    }

    #[test]
    fn a_read_that_ends_inside_a_chunk_is_still_quiet() {
        let mut pipeline = pipeline();
        let stream = frame().concat();
        let (head, tail) = stream.split_at(5000);
        assert_eq!(read(&mut pipeline, head), (5000, 5000, false));
        assert!(read(&mut pipeline, tail).2, "the image completes in the second read");
    }

    #[test]
    fn anything_else_in_the_read_wakes_the_ui() {
        let [first, second, _] = frame();
        for extra in [&b"x"[..], b"\x1b[H", b"\r", b"\x1b]0;title\x07"] {
            let mut pipeline = pipeline();
            let mut bytes = first.clone();
            bytes.extend_from_slice(extra);
            bytes.extend_from_slice(&second);
            let (processed, quiet, wake) = read(&mut pipeline, &bytes);
            assert_eq!(processed - quiet, extra.len(), "{extra:?}");
            assert!(wake, "{extra:?}");
        }
    }

    /// A command that is not closed by its terminator ends in bytes that mean something else.
    /// Those must never be counted as quiet: `ESC M` here scrolls the screen.
    #[test]
    fn a_command_cut_off_by_another_sequence_wakes_the_ui_for_that_sequence() {
        let mut pipeline = pipeline();
        let [first, ..] = frame();
        let mut bytes = first[..first.len() - 2].to_vec();
        bytes.extend_from_slice(b"\x1bM");
        let (processed, quiet, wake) = read(&mut pipeline, &bytes);
        assert_eq!(processed - quiet, 2);
        assert!(wake);
    }

    #[test]
    fn a_synchronized_update_falls_back_to_the_plain_rule() {
        let mut pipeline = pipeline();
        let [first, second, last] = frame();
        let mut held = b"\x1b[?2026h".to_vec();
        held.extend_from_slice(&first);
        read(&mut pipeline, &held);
        assert!(pipeline.0.graphics().store().is_empty(), "the update holds the command back");
        let (_, quiet, wake) = read(&mut pipeline, &second);
        assert_eq!(quiet, 0, "held bytes never reached the graphics state");
        assert!(!wake, "everything in this read is held by the update");
        let mut released = last.clone();
        released.extend_from_slice(b"\x1b[?2026l");
        let (_, quiet, wake) = read(&mut pipeline, &released);
        assert_eq!(quiet, 0, "replayed bytes are not counted against this read");
        assert!(wake);
        assert_eq!(pipeline.0.graphics().store().placements().len(), 1);
    }

    /// Image data would otherwise flush every line of real output out of the 1 MiB transcript,
    /// and announce megabytes of "output" to the UI that changed nothing.
    #[test]
    fn graphics_commands_stay_out_of_the_transcript_and_announce_no_output() {
        let mut pipeline = pipeline();
        let [first, second, last] = frame();

        read(&mut pipeline, b"before ");
        assert_eq!(pipeline.1.take_output_range(), Some((0, 7)));
        for chunk in [&first, &second] {
            read(&mut pipeline, chunk);
            assert_eq!(pipeline.1.take_output_range(), None, "nothing was added to the transcript");
        }
        let mut mixed = last.clone();
        mixed.extend_from_slice(b"after");
        read(&mut pipeline, &mixed);
        assert_eq!(pipeline.1.take_output_range(), Some((7, 12)));

        let transcript = pipeline.2.lock().unwrap();
        assert_eq!(transcript.range(0, 64).unwrap(), b"before after");
        assert_eq!(pipeline.0.graphics().store().placements().len(), 1, "and it still ran");
    }

    #[test]
    fn the_wakeup_rule_itself() {
        assert!(!wakeup_needed(0, 0, 0, false));
        assert!(!wakeup_needed(0, 0, 0, true), "nothing was read");
        assert!(wakeup_needed(10, 0, 0, false));
        assert!(!wakeup_needed(10, 10, 0, false));
        assert!(!wakeup_needed(10, 0, 10, false));
        assert!(!wakeup_needed(10, 4, 6, false));
        assert!(wakeup_needed(10, 4, 5, false));
        assert!(wakeup_needed(10, 0, 10, true));
        assert!(!wakeup_needed(10, usize::MAX, usize::MAX, false), "no overflow");
    }

    /// The sequence the read loop goes through for a compressed image: take the decode out,
    /// run it with no terminal in hand, and put the result back.
    #[test]
    fn a_compressed_image_is_decoded_outside_the_terminal_and_installed_after() {
        use image::ImageEncoder;

        let mut file = Vec::new();
        image::codecs::png::PngEncoder::new(&mut file)
            .write_image(&[90; 20 * 40 * 4], 20, 40, image::ExtendedColorType::Rgba8)
            .unwrap();
        let command = format!("\x1b_Ga=T,f=100,i=1,q=2;{}\x1b\\", STANDARD.encode(file));

        let mut pipeline = pipeline();
        pipeline.0.set_deferred_graphics_decode(true);
        let (_, _, wake) = read(&mut pipeline, command.as_bytes());
        assert!(wake, "the placement exists already, if without pixels");
        let term = &mut pipeline.0;
        assert!(term.has_graphics_jobs());
        let jobs = term.take_graphics_jobs();
        assert!(!term.has_graphics_jobs());

        // No reference to the terminal is held here.
        let finished: Vec<_> = jobs.into_iter().map(QueuedJob::run).collect();

        term.finish_graphics_jobs(finished);
        let store = term.graphics().store();
        let image = store.image(store.key_by_id(1).unwrap()).unwrap();
        assert_eq!(image.pixels.as_ref().unwrap().len(), 20 * 40 * 4);
        assert!(term.take_graphics_activity().1, "the pixels arriving must reach the screen");
    }
}

#[cfg(test)]
mod vivid_marker_tests {
    use super::*;

    const MARKER_PAYLOAD: &[u8] =
        b"A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA";
    const MARKER_BODY: &str =
        "VIVID;3;A;AAAAAAAAAAAAAAAAAAAAAA;0000000000000003;0000000000000007;AAAAAAAAAAAAAAAAAAAAAA";

    /// Run `reads` through one scanner, returning the ordinary bytes, the graphics bytes, and the
    /// markers, each concatenated in order.
    fn classify(reads: &[&[u8]]) -> (Vec<u8>, Vec<u8>, Vec<String>, VividMarkerScanner) {
        let mut scanner = VividMarkerScanner::default();
        let (mut text, mut graphics, mut markers) = (Vec::new(), Vec::new(), Vec::new());
        for read in reads {
            scanner.push(read, &mut |chunk| match chunk {
                VividChunk::Bytes(bytes) => text.extend_from_slice(bytes),
                VividChunk::Graphics(bytes) => graphics.extend_from_slice(bytes),
                VividChunk::Marker { marker, .. } => markers.push(marker.to_owned()),
            });
        }
        (text, graphics, markers, scanner)
    }

    /// The payload is full of `V`s and even spells the marker prefix, none of which may be taken
    /// for a marker candidate or leak into the ordinary bytes.
    #[test]
    fn graphics_commands_are_set_apart_across_every_read_boundary() {
        let first = b"\x1b_Ga=T,f=32,s=2,v=2,m=1;VVVVVIVIDVVVV/VIVID+3+AAAA\x1b\\".as_slice();
        let second = b"\x1b_Gm=0;VVVV\x1b\\".as_slice();
        let marker =
            [APC_VIVID_MARKER.prefix, MARKER_PAYLOAD, APC_VIVID_MARKER.terminator].concat();
        let input = [b"before V ", first, b" between \x1b[31m", &marker, second, b"after"].concat();
        let graphics = [first, second].concat();

        for split in 0..=input.len() {
            let (text, found, markers, scanner) = classify(&[&input[..split], &input[split..]]);
            assert_eq!(text, b"before V  between \x1b[31mafter", "split at {split}");
            assert_eq!(found, graphics, "split at {split}");
            assert_eq!(markers, [MARKER_BODY], "split at {split}");
            assert!(scanner.pending.is_empty() && !scanner.graphics, "split at {split}");
        }
        let bytes: Vec<&[u8]> = input.chunks(1).collect();
        let (text, found, markers, _) = classify(&bytes);
        assert_eq!(text, b"before V  between \x1b[31mafter");
        assert_eq!(found, graphics);
        assert_eq!(markers, [MARKER_BODY]);
    }

    /// The parser cancels a command that anything but its terminator interrupts, and treats the
    /// interrupting byte as ordinary input. The scanner has to draw the line in the same place.
    #[test]
    fn a_graphics_command_ends_where_the_parser_ends_it() {
        for (input, graphics, text) in [
            (&b"\x1b_Gabc\x1b\\x"[..], &b"\x1b_Gabc\x1b\\"[..], &b"x"[..]),
            (b"\x1b_Gabc\x1b[31mx", b"\x1b_Gabc", b"\x1b[31mx"),
            (b"\x1b_Gabc\x18x", b"\x1b_Gabc", b"\x18x"),
            (b"\x1b_Gabc\x1ax", b"\x1b_Gabc", b"\x1ax"),
            (b"\x1b_Gabc\x1b_Gdef\x1b\\", b"\x1b_Gabc\x1b_Gdef\x1b\\", b""),
            (b"\x1b_G\x1b\\", b"\x1b_G\x1b\\", b""),
            (b"\x1b_Xnot graphics\x1b\\", b"", b"\x1b_Xnot graphics\x1b\\"),
        ] {
            for split in 0..=input.len() {
                let (found_text, found, _, scanner) = classify(&[&input[..split], &input[split..]]);
                let shown = String::from_utf8_lossy(input);
                assert_eq!(found, graphics, "{shown:?} split at {split}");
                assert_eq!(found_text, text, "{shown:?} split at {split}");
                assert!(scanner.pending.is_empty(), "{shown:?} split at {split}");
            }
        }
    }

    #[test]
    fn an_unterminated_graphics_command_is_passed_on_as_it_arrives_not_accumulated() {
        let mut scanner = VividMarkerScanner::default();
        let mut passed = 0;
        scanner.push(b"\x1b_Ga=T;", &mut |_| {});
        for _ in 0..64 {
            scanner.push(&[b'V'; 65_536], &mut |chunk| match chunk {
                VividChunk::Graphics(bytes) => passed += bytes.len(),
                _ => panic!("payload must stay graphics"),
            });
            assert!(scanner.pending.is_empty());
        }
        assert_eq!(passed, 64 * 65_536);
        assert!(scanner.graphics);

        // The one byte ever held back is an ESC that may or may not begin the terminator.
        scanner.push(b"AAAA\x1b", &mut |_| {});
        assert_eq!(scanner.pending, b"\x1b");
        let mut text = Vec::new();
        scanner.push(b"\\ok", &mut |chunk| {
            if let VividChunk::Bytes(bytes) = chunk {
                text.extend_from_slice(bytes);
            }
        });
        assert_eq!(text, b"ok");
        assert!(scanner.pending.is_empty() && !scanner.graphics);
    }

    /// A marker printed inside a graphics command is image data, not an anchor.
    #[test]
    fn a_marker_inside_a_graphics_command_is_not_a_marker() {
        let conpty =
            [CONPTY_VIVID_MARKER.prefix, MARKER_PAYLOAD, CONPTY_VIVID_MARKER.terminator].concat();
        let input = [b"\x1b_Ga=T;".as_slice(), &conpty, b"\x1b\\"].concat();
        let (text, graphics, markers, _) = classify(&[&input]);
        assert!(text.is_empty() && markers.is_empty());
        assert_eq!(graphics, input);
    }

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
                VividChunk::Graphics(_) => panic!("no graphics command in this input"),
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
            VividChunk::Graphics(_) => panic!("no graphics command in this input"),
        });

        assert_eq!((printable, markers), (0, 1));
    }

    #[test]
    fn conpty_wraps_inside_every_part_of_the_envelope_are_zero_width() {
        let marker = format!("{MARKER_BODY};VIVID-END");
        for insertion in 1..marker.len() {
            let input = format!(
                "before{}\r\n\x1b[23;40H{}{}after",
                &marker[..insertion],
                char::from(marker.as_bytes()[insertion - 1]),
                &marker[insertion..]
            );
            let mut scanner = VividMarkerScanner::default();
            let mut text = Vec::new();
            let mut markers = Vec::new();
            for byte in input.bytes() {
                scanner.push(&[byte], &mut |chunk| match chunk {
                    VividChunk::Bytes(bytes) => text.extend_from_slice(bytes),
                    VividChunk::Marker { marker, pass_to_terminal, .. } => {
                        assert!(!pass_to_terminal);
                        markers.push(marker.to_owned());
                    },
                    VividChunk::Graphics(_) => panic!("no graphics command in this input"),
                });
            }
            assert_eq!(text, b"beforeafter");
            assert_eq!(markers, [MARKER_BODY]);
            assert!(scanner.pending.is_empty());
        }
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
    fn rejected_marker_candidates_stay_in_one_borrowed_text_span() {
        // Ordinary ASCII and kitty-style OSC payloads must not split at every `V` or escape.
        let ascii = b"V text VV VIVID;no marker \x1b[31mred\x1b[m \x1b_not-an-anchor ";
        let mut input = ascii.repeat(2048);
        input.extend_from_slice(b"\x1b]6;");
        input.extend_from_slice(&ascii.repeat(2048));
        input.push(7);
        let mut scanner = VividMarkerScanner::default();
        let mut spans = 0;
        scanner.push(&input, &mut |chunk| {
            let VividChunk::Bytes(bytes) = chunk else { panic!("unexpected marker") };
            assert_eq!(bytes, input);
            assert_eq!(bytes.as_ptr(), input.as_ptr());
            spans += 1;
        });
        assert_eq!(spans, 1);
        assert!(scanner.pending.is_empty());
    }

    #[test]
    fn rejected_candidates_around_markers_preserve_text_and_order() {
        let before = b"VV \x1b[31m \x1b_invalid ";
        let after = b"VV \x1b[m ";
        for envelope in VIVID_MARKER_ENVELOPES {
            let mut input = before.to_vec();
            input.extend_from_slice(envelope.prefix);
            input.extend_from_slice(MARKER_PAYLOAD);
            input.extend_from_slice(envelope.terminator);
            input.extend_from_slice(after);
            for split in 0..=input.len() {
                let mut scanner = VividMarkerScanner::default();
                let mut text = Vec::new();
                let mut markers = 0;
                let mut emit = |chunk: VividChunk<'_>| match chunk {
                    VividChunk::Bytes(bytes) => text.extend_from_slice(bytes),
                    VividChunk::Marker { marker, .. } => {
                        assert_eq!(text, before);
                        assert_eq!(marker, MARKER_BODY);
                        markers += 1;
                    },
                    VividChunk::Graphics(_) => panic!("no graphics command in this input"),
                };
                scanner.push(&input[..split], &mut emit);
                scanner.push(&input[split..], &mut emit);
                assert_eq!(text, [before.as_slice(), after.as_slice()].concat());
                assert_eq!(markers, 1);
                assert!(scanner.pending.is_empty());
            }
        }
    }

    #[test]
    fn apc_size_limit_and_invalid_utf8_preserve_stream_bytes() {
        for len in [MAX_VIVID_MARKER_BYTES - 1, MAX_VIVID_MARKER_BYTES, MAX_VIVID_MARKER_BYTES + 1]
        {
            for valid_utf8 in [false, true] {
                let mut input = APC_VIVID_MARKER.prefix.to_vec();
                input.resize(len - 2, if valid_utf8 { b'x' } else { 0xff });
                input.extend_from_slice(APC_VIVID_MARKER.terminator);
                for split in 0..=input.len() {
                    let mut scanner = VividMarkerScanner::default();
                    let mut text = Vec::new();
                    let mut markers = 0;
                    let mut emit = |chunk: VividChunk<'_>| match chunk {
                        VividChunk::Bytes(bytes) => text.extend_from_slice(bytes),
                        VividChunk::Marker { raw, .. } => {
                            assert_eq!(raw, input);
                            markers += 1;
                        },
                        VividChunk::Graphics(_) => panic!("no graphics command in this input"),
                    };
                    scanner.push(&input[..split], &mut emit);
                    scanner.push(&input[split..], &mut emit);
                    if valid_utf8 && len <= MAX_VIVID_MARKER_BYTES {
                        assert_eq!(markers, 1);
                        assert!(text.is_empty());
                    } else {
                        assert_eq!(markers, 0);
                        assert_eq!(text, input);
                    }
                    assert!(scanner.pending.is_empty());
                }
            }
        }

        let mut input = APC_VIVID_MARKER.prefix.to_vec();
        input.resize(MAX_VIVID_MARKER_BYTES, b'x');
        let mut scanner = VividMarkerScanner::default();
        scanner.push(&input, &mut |_| panic!("candidate released before the size limit"));
        assert_eq!(scanner.pending, input);
        let mut text = Vec::new();
        scanner.push(b"!", &mut |chunk| {
            let VividChunk::Bytes(bytes) = chunk else { panic!("unexpected marker") };
            text.extend_from_slice(bytes);
        });
        input.push(b'!');
        assert_eq!(text, input);
        assert!(scanner.pending.is_empty());
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
