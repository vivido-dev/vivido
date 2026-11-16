//! Transport retirement scoped to one authenticated control runtime.

// Rust guideline compliant 2026-10-07
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard};

use super::transport::ReadShutdown;
use super::{MAX_CONNECTIONS, lock};

/// A resumed runtime has its own table, even when its logical owner is unchanged.
#[derive(Default)]
pub(super) struct Connections {
    stopped: AtomicBool,
    readers: Mutex<Vec<(Arc<()>, ReadShutdown)>>,
    admission: Mutex<()>,
    media: RwLock<()>,
}

impl Connections {
    pub fn register(self: &Arc<Self>, shutdown: ReadShutdown) -> io::Result<Connection> {
        let mut readers = lock(&self.readers);
        if self.stopped() || readers.len() >= MAX_CONNECTIONS {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "session is retiring"));
        }
        let id = Arc::new(());
        readers.push((id.clone(), shutdown));
        Ok(Connection { owner: self.clone(), id })
    }

    /// Serialize short admission/publication operations against final retirement.
    pub fn admit(&self) -> io::Result<MutexGuard<'_, ()>> {
        let guard = lock(&self.admission);
        if self.stopped() {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "session is retiring"));
        }
        Ok(guard)
    }

    /// Media compilation never shares the lane's input/control admission lock.
    pub fn admit_media(&self) -> io::Result<RwLockReadGuard<'_, ()>> {
        let guard = self.media.read().unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.stopped() {
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "session is retiring"));
        }
        Ok(guard)
    }

    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Unblock socket I/O before waiting for already-admitted operations.
    pub fn close(&self) {
        self.stopped.store(true, Ordering::Release);
        let readers = std::mem::take(&mut *lock(&self.readers));
        for (_, shutdown) in readers {
            shutdown.stop();
        }
        drop(lock(&self.admission));
        drop(self.media.write().unwrap_or_else(|poisoned| poisoned.into_inner()));
    }
}

/// Removing an old socket cannot remove one from a resumed runtime's table.
pub(super) struct Connection {
    owner: Arc<Connections>,
    id: Arc<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        lock(&self.owner.readers).retain(|(id, _)| !Arc::ptr_eq(id, &self.id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(unix)]
    fn closing_old_sockets_and_late_guard_drop_leave_replacement_sockets_live() {
        use super::super::transport::Reader;
        use std::io::{Read, Write};
        use std::os::unix::net::UnixStream;
        use vivid_protocol::wire::{ConnectionKind, encode_preface};

        let socket = || {
            let (mut client, server) = UnixStream::pair().unwrap();
            client.set_read_timeout(Some(std::time::Duration::from_secs(1))).unwrap();
            client.write_all(&encode_preface(ConnectionKind::Lane, 65536)).unwrap();
            let (reader, _, _) = Reader::new(server).unwrap();
            (client, reader)
        };
        let old = Arc::new(Connections::default());
        let replacement = Arc::new(Connections::default());
        let (mut old_client, old_reader) = socket();
        let old_guard = old.register(old_reader.shutdown_handle().unwrap()).unwrap();
        let (mut new_client, new_reader) = socket();
        let _new_guard = replacement.register(new_reader.shutdown_handle().unwrap()).unwrap();
        old.close();
        assert!(old.register(old_reader.shutdown_handle().unwrap()).is_err());
        let mut byte = [0];
        assert_eq!(old_client.read(&mut byte).unwrap(), 0);
        drop(old_guard);
        assert_eq!(lock(&replacement.readers).len(), 1);
        new_client.write_all(&[7]).unwrap();
        assert!(!replacement.stopped());
        replacement.close();
        // Unread client-to-server data may make the peer report a reset on close.
        assert!(matches!(new_client.read(&mut byte), Ok(0) | Err(_)));
    }

    #[test]
    fn retirement_waits_for_admission_without_stopping_the_replacement_runtime() {
        let old = Arc::new(Connections::default());
        let replacement = Connections::default();
        let admitted = old.admit_media().unwrap();
        assert!(old.admit().is_ok(), "media cannot block input admission");
        assert!(old.admit_media().is_ok(), "one track cannot block another track");
        let retiring = old.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            retiring.close();
            sent.send(()).unwrap();
        });
        while !old.stopped() {
            std::thread::yield_now();
        }
        assert!(received.try_recv().is_err());
        assert!(replacement.admit().is_ok());
        drop(admitted);
        received.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        assert!(old.admit().is_err());
        assert!(replacement.admit().is_ok());
        worker.join().unwrap();
    }
}
