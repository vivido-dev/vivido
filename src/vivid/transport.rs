use std::io::{self, Read, Write};
#[cfg(windows)]
use std::net::TcpStream as LocalStream;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(unix)]
type LocalStream = UnixStream;

use vivid_protocol::wire::{
    HEADER_SIZE, PREFACE_SIZE, Preface, RECORD_KNOWN_FLAGS, Record, RecordHeader,
};
use vivid_protocol::{CONTROL_MAX_RECORD_BODY, FRAMING_MAJOR, FRAMING_MINOR, HARD_MAX_RECORD_BODY};

pub struct Reader {
    stream: LocalStream,
    negotiated_maximum: u32,
    maximum: u32,
    sequence: u64,
}

impl Reader {
    pub fn new(mut stream: LocalStream) -> io::Result<(Self, Preface)> {
        let mut bytes = [0_u8; PREFACE_SIZE];
        stream.read_exact(&mut bytes)?;
        let preface = Preface::decode(bytes)?;
        if preface.major != FRAMING_MAJOR || preface.minor != FRAMING_MINOR {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported Vivid major version {}", preface.major),
            ));
        }
        Ok((
            Self {
                stream,
                negotiated_maximum: preface.initiator_tx_body_limit.min(HARD_MAX_RECORD_BODY),
                maximum: preface
                    .initiator_tx_body_limit
                    .min(HARD_MAX_RECORD_BODY)
                    .min(CONTROL_MAX_RECORD_BODY),
                sequence: 0,
            },
            preface,
        ))
    }

    pub fn read_record(&mut self) -> io::Result<Record> {
        let mut bytes = [0_u8; HEADER_SIZE];
        self.stream.read_exact(&mut bytes)?;
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
                "Vivid record exceeds negotiated maximum",
            ));
        }
        let expected = self.sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "record sequence exhausted")
        })?;
        if header.sequence != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Vivid record sequence {} does not match {expected}", header.sequence),
            ));
        }
        self.sequence = header.sequence;
        let mut body = vec![0_u8; header.body_length as usize];
        self.stream.read_exact(&mut body)?;
        Ok(Record {
            record_type: header.record_type,
            flags: header.flags,
            object_id: header.object_id,
            sequence: header.sequence,
            body,
        })
    }

    pub fn writer(&self) -> io::Result<Writer> {
        Ok(Writer {
            inner: Mutex::new(WriterInner {
                stream: self.stream.try_clone()?,
                maximum: CONTROL_MAX_RECORD_BODY,
                sequence: 0,
            }),
        })
    }

    pub fn set_maximum(&mut self, maximum: u32) {
        self.maximum = self.negotiated_maximum.min(maximum);
    }

    #[cfg(unix)]
    pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        let mut descriptor =
            libc::pollfd { fd: self.stream.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let timeout_ms = timeout.as_millis().min(c_int_max() as u128) as libc::c_int;
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        Ok(result > 0)
    }

    #[cfg(windows)]
    pub fn wait_readable(&self, timeout: Duration) -> io::Result<bool> {
        self.stream.set_read_timeout(Some(timeout))?;
        let mut byte = [0_u8; 1];
        let result = match self.stream.peek(&mut byte) {
            Ok(_) => Ok(true),
            Err(error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                Ok(false)
            },
            Err(error) => Err(error),
        };
        self.stream.set_read_timeout(None)?;
        result
    }
}

#[cfg(unix)]
const fn c_int_max() -> libc::c_int {
    libc::c_int::MAX
}

pub struct Writer {
    inner: Mutex<WriterInner>,
}

struct WriterInner {
    stream: LocalStream,
    maximum: u32,
    sequence: u64,
}

impl Writer {
    pub fn set_maximum(&self, maximum: u32) -> io::Result<()> {
        if maximum == 0 || maximum > HARD_MAX_RECORD_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid outgoing Vivid record limit",
            ));
        }
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).maximum =
            maximum.min(CONTROL_MAX_RECORD_BODY);
        Ok(())
    }

    pub fn write_record(&self, record_type: u16, object_id: u64, body: &[u8]) -> io::Result<()> {
        let body_length = u32::try_from(body.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "record body exceeds u32"))?;
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if body_length > inner.maximum || body_length > HARD_MAX_RECORD_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "outgoing Vivid record exceeds negotiated maximum",
            ));
        }
        inner.sequence = inner.sequence.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "outgoing record sequence exhausted")
        })?;
        let header = RecordHeader {
            body_length,
            record_type,
            flags: 0,
            object_id,
            sequence: inner.sequence,
        };
        inner.stream.write_all(&header.encode())?;
        inner.stream.write_all(body)?;
        inner.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vivid_protocol::wire::{ConnectionKind, encode_preface};

    #[cfg(unix)]
    fn stream_pair() -> (LocalStream, LocalStream) {
        UnixStream::pair().unwrap()
    }

    #[cfg(windows)]
    fn stream_pair() -> (LocalStream, LocalStream) {
        use std::net::{Ipv4Addr, TcpListener};

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let client = LocalStream::connect(address).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn rejects_out_of_order_records_before_body_dispatch() {
        let (mut client, server) = stream_pair();
        client.write_all(&encode_preface(ConnectionKind::Control, 1024)).unwrap();
        client
            .write_all(
                &RecordHeader {
                    body_length: 0,
                    record_type: 1,
                    flags: 0,
                    object_id: 0,
                    sequence: 2,
                }
                .encode(),
            )
            .unwrap();
        let (mut reader, _) = Reader::new(server).unwrap();
        assert!(reader.read_record().is_err());
    }

    #[test]
    fn rejects_reserved_record_flags_before_body_dispatch() {
        let (mut client, server) = stream_pair();
        client.write_all(&encode_preface(ConnectionKind::Control, 1024)).unwrap();
        client
            .write_all(
                &RecordHeader {
                    body_length: 0,
                    record_type: 1,
                    flags: 2,
                    object_id: 0,
                    sequence: 1,
                }
                .encode(),
            )
            .unwrap();
        let (mut reader, _) = Reader::new(server).unwrap();
        assert!(reader.read_record().is_err());
    }

    #[test]
    fn accepts_split_writes_at_every_framing_boundary() {
        let body = b"split-me";
        let header = RecordHeader {
            body_length: body.len() as u32,
            record_type: 7,
            flags: 0,
            object_id: 9,
            sequence: 1,
        }
        .encode();
        let mut wire = encode_preface(ConnectionKind::Control, 1024).to_vec();
        wire.extend_from_slice(&header);
        wire.extend_from_slice(body);

        for split in 0..=wire.len() {
            let (mut client, server) = stream_pair();
            let bytes = wire.clone();
            let writer = std::thread::spawn(move || {
                client.write_all(&bytes[..split]).unwrap();
                client.write_all(&bytes[split..]).unwrap();
            });
            let (mut reader, _) = Reader::new(server).unwrap();
            let record = reader.read_record().unwrap();
            assert_eq!(record.body, body);
            writer.join().unwrap();
        }
    }
}
