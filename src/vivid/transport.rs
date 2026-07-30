//! Accepted-side Vivid 1.5 framing for the private presenter endpoint.

use std::io::{self, IoSlice, Read, Write};
#[cfg(windows)]
use std::net::TcpStream as LocalStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream as LocalStream;
use std::sync::Mutex;

use vivid_protocol::wire::{
    ConnectionKind, HEADER_SIZE, PREFACE_SIZE, Preface, PrefaceClassification, RECORD_KNOWN_FLAGS,
    Record, RecordHeader,
};
use vivid_protocol::{CONTROL_MAX_RECORD_BODY, HARD_MAX_RECORD_BODY};

pub struct Reader {
    stream: LocalStream,
    negotiated_maximum: u32,
    maximum: u32,
    sequence: u64,
    first_record: bool,
}

impl Reader {
    pub fn new(mut stream: LocalStream) -> io::Result<(Self, Preface, [u8; PREFACE_SIZE])> {
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
        Ok((
            Self {
                stream,
                negotiated_maximum: maximum,
                maximum: if preface.kind == ConnectionKind::Control {
                    maximum.min(CONTROL_MAX_RECORD_BODY)
                } else {
                    maximum
                },
                sequence: 0,
                first_record: true,
            },
            preface,
            bytes,
        ))
    }

    /// A handle that can unblock a reader parked in [`Self::read_record`].
    ///
    /// The session actor uses it after a clean `GOODBYE`: core §6.3 has the presenter reply `OK`
    /// and close the logical session, so the reader must stop rather than wait for a peer EOF that
    /// a producer is not obliged to send promptly.
    pub fn shutdown_handle(&self) -> io::Result<ReadShutdown> {
        Ok(ReadShutdown { stream: self.stream.try_clone()? })
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
        self.stream.read_exact(body)?;
        Ok(header)
    }

    pub fn writer(&self, kind: ConnectionKind) -> io::Result<Writer> {
        Ok(Writer {
            inner: Mutex::new(WriterInner {
                stream: self.stream.try_clone()?,
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

/// Unblocks a parked reader by shutting the receive half of its connection.
pub struct ReadShutdown {
    stream: LocalStream,
}

impl ReadShutdown {
    pub fn stop(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Read);
    }
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
        write_parts(&mut inner.stream, &header.encode(), parts)?;
        inner.stream.flush()
    }
}

fn write_parts(stream: &mut LocalStream, header: &[u8], parts: &[&[u8]]) -> io::Result<()> {
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
