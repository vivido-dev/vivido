//! Accepted-side Vivid 1.5 framing for the private presenter endpoint.

use std::io::{self, IoSlice, Read, Write};
#[cfg(windows)]
use std::net::TcpStream as LocalStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream as LocalStream;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vivid_protocol::wire::{
    ConnectionKind, HEADER_SIZE, PREFACE_SIZE, Preface, PrefaceClassification, RECORD_KNOWN_FLAGS,
    Record, RecordHeader,
};
use vivid_protocol::{CONTROL_MAX_RECORD_BODY, HARD_MAX_RECORD_BODY};

/// How long an accepted connection has to get from its preface to an authenticated session.
///
/// Every connection holds a slot and an OS thread from the moment it is accepted, so the phase
/// before the peer has proved anything is deadlined. Ten seconds is orders of magnitude above any
/// real `HELLO`, `CHANNEL_OPEN` or `LANE_OPEN` round trip — a producer that is merely slow cannot
/// reach it, and one that says nothing at all no longer holds its slot forever. The deadline is
/// lifted by [`Reader::finish_handshake`] once the peer is authenticated, so a live session is
/// never timed out for being idle.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct Reader {
    stream: Arc<LocalStream>,
    #[cfg(windows)]
    cancelled: Arc<AtomicBool>,
    negotiated_maximum: u32,
    maximum: u32,
    sequence: u64,
    first_record: bool,
    handshake_deadline: Option<Instant>,
}

impl Reader {
    pub fn new(mut stream: LocalStream) -> io::Result<(Self, Preface, [u8; PREFACE_SIZE])> {
        let handshake_deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let mut bytes = [0_u8; PREFACE_SIZE];
        stream.read_exact(&mut bytes)?;
        let preface = match Preface::classify(bytes)? {
            PrefaceClassification::Accepted(preface) => preface,
            PrefaceClassification::UnsupportedVersion(_) => {
                stream.write_all(&vivid_protocol::wire::unsupported_version_record())?;
                stream.flush()?;
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported Vivid protocol version",
                ));
            },
        };
        let maximum = preface.initiator_tx_body_limit.min(HARD_MAX_RECORD_BODY);
        #[cfg(windows)]
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        Ok((
            Self {
                stream: Arc::new(stream),
                #[cfg(windows)]
                cancelled: Arc::new(AtomicBool::new(false)),
                negotiated_maximum: maximum,
                maximum: if preface.kind == ConnectionKind::Control {
                    maximum.min(CONTROL_MAX_RECORD_BODY)
                } else {
                    maximum
                },
                sequence: 0,
                first_record: true,
                handshake_deadline: Some(handshake_deadline),
            },
            preface,
            bytes,
        ))
    }

    /// Lift the handshake deadline once this connection's peer has been authenticated.
    ///
    /// An established session may legitimately stay silent for hours, so the deadline covers only
    /// the records before `HELLO`/`CHANNEL_OPEN`/`LANE_OPEN` has been accepted. On Windows the
    /// short polling timeout stays in place: it is what makes [`ReadShutdown`] able to wake a
    /// parked reader, not a deadline.
    pub fn finish_handshake(&mut self) -> io::Result<()> {
        self.handshake_deadline = None;
        #[cfg(unix)]
        self.stream.set_read_timeout(None)?;
        Ok(())
    }

    /// Shorten the handshake deadline so a test does not have to wait the production ten seconds.
    #[cfg(test)]
    pub fn set_handshake_deadline(&mut self, within: Duration) {
        self.handshake_deadline = Some(Instant::now() + within);
    }

    /// A handle that can unblock a reader parked in [`Self::read_record`].
    ///
    /// The session actor uses it after a clean `GOODBYE`: core §6.3 has the presenter reply `OK`
    /// and close the logical session, so the reader must stop rather than wait for a peer EOF that
    /// a producer is not obliged to send promptly.
    pub fn shutdown_handle(&self) -> io::Result<ReadShutdown> {
        Ok(ReadShutdown {
            stream: self.stream.clone(),
            #[cfg(windows)]
            cancelled: self.cancelled.clone(),
        })
    }

    pub fn read_record(&mut self, kind: ConnectionKind) -> io::Result<Record> {
        let mut body = Vec::new();
        let header = self.read_record_body_into(kind, &mut body)?;
        Ok(Record {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    fn read_record_body_into(
        &mut self,
        kind: ConnectionKind,
        body: &mut Vec<u8>,
    ) -> io::Result<RecordHeader> {
        let mut bytes = [0_u8; HEADER_SIZE];
        #[cfg(unix)]
        read_exact_interruptibly(self.stream.as_ref(), self.handshake_deadline, &mut bytes)?;
        #[cfg(windows)]
        read_exact_interruptibly(
            self.stream.as_ref(),
            &self.cancelled,
            self.handshake_deadline,
            &mut bytes,
        )?;
        let header = RecordHeader::decode(bytes);
        if header.flags & !RECORD_KNOWN_FLAGS != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Vivid record has nonzero reserved flags",
            ));
        }
        if header.body_length > self.maximum || header.body_length > HARD_MAX_RECORD_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Vivid record exceeds the accepted body limit",
            ));
        }
        let expected = self.sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "record sequence exhausted")
        })?;
        if header.sequence != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Vivid record sequence is not contiguous",
            ));
        }
        if self.first_record {
            kind.validate_first_record(&header)?;
            self.first_record = false;
        }
        self.sequence = header.sequence;
        body.resize(header.body_length as usize, 0);
        #[cfg(unix)]
        read_exact_interruptibly(self.stream.as_ref(), self.handshake_deadline, body)?;
        #[cfg(windows)]
        read_exact_interruptibly(
            self.stream.as_ref(),
            &self.cancelled,
            self.handshake_deadline,
            body,
        )?;
        Ok(header)
    }

    pub fn writer(&self, kind: ConnectionKind) -> io::Result<Writer> {
        Ok(Writer {
            inner: Mutex::new(WriterInner {
                stream: self.stream.clone(),
                maximum: if kind == ConnectionKind::Control {
                    CONTROL_MAX_RECORD_BODY
                } else {
                    HARD_MAX_RECORD_BODY
                },
                sequence: 0,
            }),
        })
    }

    pub fn set_maximum(&mut self, maximum: u32) -> io::Result<()> {
        if maximum == 0 || maximum > HARD_MAX_RECORD_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid incoming record limit",
            ));
        }
        self.maximum = self.negotiated_maximum.min(maximum);
        Ok(())
    }
}

/// Unblocks a parked reader after the connection's egress has drained.
pub struct ReadShutdown {
    stream: Arc<LocalStream>,
    #[cfg(windows)]
    cancelled: Arc<AtomicBool>,
}

impl ReadShutdown {
    pub fn stop(&self) {
        #[cfg(windows)]
        self.cancelled.store(true, Ordering::Release);
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

#[cfg(windows)]
fn read_exact_interruptibly(
    stream: &LocalStream,
    cancelled: &AtomicBool,
    deadline: Option<Instant>,
    mut bytes: &mut [u8],
) -> io::Result<()> {
    while !bytes.is_empty() {
        let mut stream = stream;
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => bytes = &mut bytes[read..],
            Err(error)
                if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) =>
            {
                if cancelled.load(Ordering::Acquire) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "reader stopped"));
                }
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    return Err(handshake_expired());
                }
            },
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn read_exact_interruptibly(
    stream: &LocalStream,
    deadline: Option<Instant>,
    mut bytes: &mut [u8],
) -> io::Result<()> {
    let mut stream = stream;
    let Some(deadline) = deadline else {
        return stream.read_exact(bytes);
    };
    // Before the handshake completes the socket carries a receive timeout, and it is refreshed to
    // what is left of the deadline so a peer that dribbles one byte at a time cannot extend it.
    while !bytes.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(handshake_expired());
        }
        // A peer that has already closed makes this fail on macOS; the timeout installed before
        // the preface read still bounds the wait, so the read below decides the outcome.
        let _ = stream.set_read_timeout(Some(remaining));
        match stream.read(bytes) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => bytes = &mut bytes[read..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {},
            Err(error)
                if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) =>
            {
                return Err(handshake_expired());
            },
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn handshake_expired() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "Vivid connection did not complete its handshake before the deadline",
    )
}

pub struct Writer {
    inner: Mutex<WriterInner>,
}

struct WriterInner {
    stream: Arc<LocalStream>,
    maximum: u32,
    sequence: u64,
}

impl Writer {
    pub fn set_maximum(&self, maximum: u32) -> io::Result<()> {
        if maximum == 0 || maximum > HARD_MAX_RECORD_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid outgoing record limit",
            ));
        }
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).maximum = maximum;
        Ok(())
    }

    pub fn write_record(&self, record_type: u16, object_id: u64, body: &[u8]) -> io::Result<()> {
        self.write_record_parts(record_type, object_id, &[body])
    }

    pub fn write_record_parts(
        &self,
        record_type: u16,
        object_id: u64,
        parts: &[&[u8]],
    ) -> io::Result<()> {
        let body_length = parts.iter().try_fold(0_usize, |total, part| {
            total.checked_add(part.len()).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "record body length overflows")
            })
        })?;
        let body_length = u32::try_from(body_length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "record body exceeds u32"))?;
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if body_length > inner.maximum || body_length > HARD_MAX_RECORD_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "outgoing Vivid record exceeds the accepted body limit",
            ));
        }
        inner.sequence = inner.sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "outgoing sequence exhausted")
        })?;
        let header = RecordHeader {
            body_length,
            record_type,
            flags: 0,
            object_id,
            sequence: inner.sequence,
        };
        let mut stream = inner.stream.as_ref();
        write_parts(&mut stream, &header.encode(), parts)?;
        stream.flush()
    }
}

fn write_parts(stream: &mut &LocalStream, header: &[u8], parts: &[&[u8]]) -> io::Result<()> {
    let mut buffers = Vec::with_capacity(parts.len() + 1);
    buffers.push(IoSlice::new(header));
    buffers.extend(parts.iter().map(|part| IoSlice::new(part)));
    let mut index = 0;
    let mut offset = 0;
    while index < buffers.len() {
        let current = &buffers[index..];
        let mut adjusted = Vec::with_capacity(current.len());
        adjusted.push(IoSlice::new(&current[0][offset..]));
        adjusted.extend(current[1..].iter().map(|slice| IoSlice::new(slice)));
        let written = stream.write_vectored(&adjusted)?;
        if written == 0 {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write Vivid record"));
        }
        let mut remaining = written;
        while index < buffers.len() {
            let available = buffers[index].len() - offset;
            if remaining < available {
                offset += remaining;
                break;
            }
            remaining -= available;
            index += 1;
            offset = 0;
            if remaining == 0 {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::wire::{ConnectionKind, encode_preface};

    #[cfg(unix)]
    fn stream_pair() -> (LocalStream, LocalStream) {
        LocalStream::pair().unwrap()
    }

    #[cfg(windows)]
    fn stream_pair() -> (LocalStream, LocalStream) {
        use std::net::{Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let client = LocalStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn validates_kind_specific_first_record() {
        let (mut client, server) = stream_pair();
        client.write_all(&encode_preface(ConnectionKind::Track, 1024)).unwrap();
        client
            .write_all(
                &RecordHeader {
                    body_length: 0,
                    record_type: vivid_protocol::messages::HELLO,
                    flags: 0,
                    object_id: 0,
                    sequence: 1,
                }
                .encode(),
            )
            .unwrap();
        let (mut reader, preface, _) = Reader::new(server).unwrap();
        assert!(reader.read_record(preface.kind).is_err());
    }

    /// A peer that sends a preface and then goes silent must not park the reader forever.
    #[test]
    fn a_silent_peer_is_dropped_when_the_handshake_deadline_expires() {
        let (mut client, server) = stream_pair();
        client.write_all(&encode_preface(ConnectionKind::Control, 1024)).unwrap();
        let (mut reader, preface, _) = Reader::new(server).unwrap();
        reader.set_handshake_deadline(Duration::from_millis(50));

        let started = Instant::now();
        let Err(error) = reader.read_record(preface.kind) else {
            panic!("a silent peer must not be served past the handshake deadline");
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < HANDSHAKE_TIMEOUT, "the deadline, not the socket, ended it");
        drop(client);
    }

    /// The deadline covers the handshake only: an established session may idle indefinitely.
    #[test]
    fn finishing_the_handshake_lifts_the_deadline() {
        let (mut client, server) = stream_pair();
        client.write_all(&encode_preface(ConnectionKind::Control, 1024)).unwrap();
        let (mut reader, preface, _) = Reader::new(server).unwrap();
        reader.set_handshake_deadline(Duration::from_millis(50));
        reader.finish_handshake().unwrap();

        // Nothing arrives for longer than the deadline would have allowed; the record that then
        // does arrive is still served.
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            write_hello(&mut client);
            client
        });
        let record = reader.read_record(preface.kind).expect("an idle session stays open");
        assert_eq!(record.record_type, vivid_protocol::messages::HELLO);
        drop(writer.join().unwrap());
    }

    fn write_hello(stream: &mut LocalStream) {
        stream
            .write_all(
                &RecordHeader {
                    body_length: 0,
                    record_type: vivid_protocol::messages::HELLO,
                    flags: 0,
                    object_id: 0,
                    sequence: 1,
                }
                .encode(),
            )
            .unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn emits_one_typed_error_for_a_well_formed_version_mismatch() {
        let (mut client, server) = stream_pair();
        let mut preface = encode_preface(ConnectionKind::Control, 1024);
        preface[5] = preface[5].wrapping_add(1);
        client.write_all(&preface).unwrap();
        assert!(Reader::new(server).is_err());
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, vivid_protocol::wire::unsupported_version_record());
    }
}
