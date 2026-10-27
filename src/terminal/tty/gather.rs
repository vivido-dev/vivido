//! Bounded macOS PTY read-ahead. The terminal worker remains the only parser.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const CAPACITY: usize = 64 * 1024;
const SLOTS: usize = 4;
// Saturated macOS PTY refill gaps are usually shorter than a poll wakeup.
const RETRIES: usize = 16;
const SMALL_BURST: usize = 1024;
const BRIDGE_BUDGET: Duration = Duration::from_millis(3);

pub(super) fn set_qos() {
    // SAFETY: this only sets the calling worker's scheduling class, with a valid priority.
    let result = unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0)
    };
    if result != 0 {
        log::debug!("PTY worker QoS request failed: {result}");
    }
}

struct Batch {
    bytes: Box<[u8]>,
    len: usize,
}
struct State {
    free: Vec<Box<[u8]>>,
    ready: VecDeque<Batch>,
    stop: bool,
    busy: bool,
    bridging: bool,
    generation: u64,
    error: Option<io::Error>,
    eof: bool,
}
struct Shared {
    state: Mutex<State>,
    space: Condvar,
}

pub(super) struct Gather {
    shared: Arc<Shared>,
    pub(super) ready: UnixStream,
    wake: UnixStream,
    _notify: UnixStream,
    worker: Option<JoinHandle<()>>,
    current: Option<Batch>,
    offset: usize,
}

impl Gather {
    // The PTY constructor and test fixtures supply a nonblocking descriptor.
    pub(super) fn new(mut file: File) -> io::Result<Self> {
        let (ready, mut notify) = UnixStream::pair()?;
        let (wake, mut interrupt) = UnixStream::pair()?;
        let keep_notify = notify.try_clone()?;
        for socket in [&ready, &notify, &wake, &interrupt] {
            socket.set_nonblocking(true)?;
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                free: (0..SLOTS).map(|_| vec![0; CAPACITY].into_boxed_slice()).collect(),
                ready: VecDeque::with_capacity(SLOTS),
                stop: false,
                busy: false,
                bridging: false,
                generation: 0,
                error: None,
                eof: false,
            }),
            space: Condvar::new(),
        });
        let owner = shared.clone();
        let worker = std::thread::Builder::new().name("PTY gather".into()).spawn(move || {
            set_qos();
            loop {
                let (mut bytes, generation) = {
                    let mut state = owner.state.lock().unwrap();
                    while state.free.is_empty() && !state.stop {
                        state = owner.space.wait(state).unwrap();
                    }
                    if state.stop {
                        break;
                    }
                    (state.free.pop().unwrap(), state.generation)
                };
                let mut len = 0;
                let mut start = None;
                let mut retries = 0;
                let mut done = false;
                let mut eof = false;
                let mut failure = None;
                loop {
                    let read = {
                        let state = owner.state.lock().unwrap();
                        if state.stop || state.generation != generation {
                            break;
                        }
                        // Serialize reset with the nonblocking read, never with a poll or parsing.
                        file.read(&mut bytes[len..])
                    };
                    match read {
                        Ok(0) => {
                            eof = true;
                            done = true;
                            break;
                        },
                        Ok(n) => {
                            #[cfg(feature = "perf")]
                            crate::performance::READ_BYTES.record(n as u64);
                            len += n;
                            retries = 0;
                            if len == CAPACITY {
                                break;
                            }
                        },
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if len > 0 {
                                if len < SMALL_BURST {
                                    break;
                                }
                                // Retry without sleeping before deciding whether a saturated
                                // refill gap needs a poll. An idle parser never waits for a poll.
                                if retries < RETRIES {
                                    retries += 1;
                                    continue;
                                }
                                let start = start.get_or_insert_with(Instant::now);
                                if start.elapsed() >= BRIDGE_BUDGET {
                                    break;
                                }
                                let mut state = owner.state.lock().unwrap();
                                if !state.busy && state.ready.is_empty() {
                                    break;
                                }
                                state.bridging = true;
                            }
                            // Idle reads sleep until output or shutdown; bridging waits at most 1 ms.
                            let mut fds = [
                                libc::pollfd {
                                    fd: file.as_raw_fd(),
                                    events: libc::POLLIN,
                                    revents: 0,
                                },
                                libc::pollfd {
                                    fd: interrupt.as_raw_fd(),
                                    events: libc::POLLIN,
                                    revents: 0,
                                },
                            ];
                            // SAFETY: both descriptors and the two-element array live through poll.
                            let result = unsafe {
                                libc::poll(fds.as_mut_ptr(), 2, if len == 0 { -1 } else { 1 })
                            };
                            if result < 0
                                && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                            {
                                failure = Some(io::Error::last_os_error());
                                done = true;
                                break;
                            }
                            owner.state.lock().unwrap().bridging = false;
                            let interrupted = interrupt.read(&mut [0; 64]).is_ok();
                            if len > 0 && (result == 0 || interrupted) {
                                break;
                            }
                        },
                        Err(error) => {
                            failure = Some(error);
                            done = true;
                            break;
                        },
                    }
                }
                let mut state = owner.state.lock().unwrap();
                state.bridging = false;
                if len > 0 && generation == state.generation {
                    #[cfg(feature = "perf")]
                    crate::performance::GATHER_BYTES.record(len as u64);
                    state.ready.push_back(Batch { bytes, len });
                    let _ = notify.write(&[1]);
                } else {
                    state.free.push(bytes);
                }
                if done {
                    // Publish terminal state together with the final bytes. Readers must
                    // never observe EOF or an error ahead of an unpublished tail batch.
                    state.eof = eof;
                    state.error = failure;
                    let _ = notify.write(&[1]);
                    break;
                }
                if state.stop {
                    break;
                }
            }
        })?;
        Ok(Self {
            shared,
            ready,
            wake,
            _notify: keep_notify,
            worker: Some(worker),
            current: None,
            offset: 0,
        })
    }

    pub(super) fn stop(&mut self) {
        self.shared.state.lock().unwrap().stop = true;
        self.shared.space.notify_all();
        let _ = self.wake.write(&[1]);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    pub(super) fn idle(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.busy = false;
        if state.bridging {
            let _ = self.wake.write(&[1]);
        }
    }

    pub(super) fn reset(&mut self) {
        let mut state = self.shared.state.lock().unwrap();
        state.generation = state.generation.checked_add(1).expect("PTY reset generation exhausted");
        while let Some(batch) = state.ready.pop_front() {
            state.free.push(batch.bytes);
        }
        if let Some(batch) = self.current.take() {
            state.free.push(batch.bytes);
        }
        self.offset = 0;
        state.busy = false;
        let _ = self.ready.read(&mut [0; SLOTS + 1]);
        self.shared.space.notify_all();
        let _ = self.wake.write(&[1]);
    }
}

impl Read for Gather {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.current.is_none() {
            let mut state = self.shared.state.lock().unwrap();
            self.current = state.ready.pop_front();
            self.offset = 0;
            if self.current.is_some() {
                state.busy = true;
            } else {
                state.busy = false;
                if state.bridging {
                    let _ = self.wake.write(&[1]);
                }
                if let Some(error) = state.error.take() {
                    let _ = self.ready.read(&mut [0; SLOTS + 1]);
                    return Err(error);
                }
                return if state.eof || state.stop {
                    let _ = self.ready.read(&mut [0; SLOTS + 1]);
                    Ok(0)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                };
            }
        }
        let batch = self.current.as_ref().unwrap();
        let count = out.len().min(batch.len - self.offset);
        out[..count].copy_from_slice(&batch.bytes[self.offset..self.offset + count]);
        self.offset += count;
        if self.offset == batch.len {
            let _ = self.ready.read(&mut [0]);
            let batch = self.current.take().unwrap();
            self.shared.state.lock().unwrap().free.push(batch.bytes);
            self.shared.space.notify_one();
        }
        Ok(count)
    }
}
impl Drop for Gather {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;

    fn pair() -> (Gather, UnixStream) {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        (Gather::new(File::from(OwnedFd::from(reader))).unwrap(), writer)
    }
    fn receive(gather: &mut Gather, count: usize) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = Vec::new();
        let mut buffer = [0; 8192];
        while bytes.len() < count {
            match gather.read(&mut buffer[..(count - bytes.len()).min(8192)]) {
                Ok(n) if n > 0 => bytes.extend_from_slice(&buffer[..n]),
                Ok(_) => panic!("early EOF"),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "gather stalled");
                    std::thread::yield_now();
                },
                Err(error) => panic!("{error}"),
            }
        }
        bytes
    }

    #[test]
    fn partial_batch_keeps_readiness_and_reset_accepts_new_data() {
        let (mut gather, mut writer) = pair();
        writer.write_all(b"first").unwrap();
        assert_eq!(receive(&mut gather, 1), b"f");
        let mut fd =
            libc::pollfd { fd: gather.ready.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one live socket descriptor and one initialized pollfd.
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 0) }, 1);
        gather.reset();
        writer.write_all(b"next").unwrap();
        assert_eq!(receive(&mut gather, 4), b"next");
    }

    #[test]
    fn queue_preserves_order_across_backpressure_and_eof() {
        let (mut gather, mut writer) = pair();
        let expected: Vec<_> = (0..CAPACITY * 12).map(|i| (i % 251) as u8).collect();
        let input = expected.clone();
        let worker = std::thread::spawn(move || writer.write_all(&input).unwrap());
        assert_eq!(receive(&mut gather, expected.len()), expected);
        worker.join().unwrap();
        gather.stop();
        assert_eq!(gather.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn stopping_full_owner_does_not_stop_another_owner() {
        let (mut first, mut writer) = pair();
        let (mut second, mut peer) = pair();
        let producer = std::thread::spawn(move || writer.write_all(&vec![1; CAPACITY * SLOTS]));
        let deadline = Instant::now() + Duration::from_secs(5);
        while first.shared.state.lock().unwrap().ready.len() < SLOTS {
            assert!(Instant::now() < deadline, "queue never filled");
            std::thread::yield_now();
        }
        first.stop();
        first.reset();
        assert!(first.shared.state.lock().unwrap().ready.is_empty());
        peer.write_all(b"independent query reply").unwrap();
        assert_eq!(receive(&mut second, 23), b"independent query reply");
        let result = producer.join().unwrap();
        assert!(result.is_ok() || result.unwrap_err().kind() == io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn stopped_reader_drains_queued_bytes_before_eof_without_spinning() {
        let (mut gather, mut writer) = pair();
        writer.write_all(b"ordered tail").unwrap();
        assert_eq!(receive(&mut gather, 1), b"o");
        gather.stop();
        assert_eq!(receive(&mut gather, 11), b"rdered tail");
        assert_eq!(gather.read(&mut [0; 1]).unwrap(), 0);
        let mut fd =
            libc::pollfd { fd: gather.ready.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one live socket descriptor and one initialized pollfd.
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 0) }, 0);
    }

    #[test]
    fn eof_follows_the_final_partial_batch() {
        for _ in 0..16 {
            let (mut gather, mut writer) = pair();
            writer.write_all(b"final partial batch").unwrap();
            drop(writer);
            let mut received = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let mut buffer = [0; 64];
                match gather.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => received.extend_from_slice(&buffer[..n]),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "EOF never arrived");
                        std::thread::yield_now();
                    },
                    Err(error) => panic!("{error}"),
                }
            }
            assert_eq!(received, b"final partial batch");
        }
    }

    #[test]
    fn idle_shutdown_interrupts_native_poll() {
        let (mut gather, _writer) = pair();
        let start = Instant::now();
        gather.stop();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
