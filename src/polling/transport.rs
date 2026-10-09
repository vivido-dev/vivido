//! Owner-authenticated local IPC transports.

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

#[cfg(unix)]
mod platform {
    use std::fs;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    use super::*;

    /// An owner-restricted listener for local automation connections.
    pub struct LocalListener(UnixListener);

    impl LocalListener {
        /// Bind an owner-restricted local automation endpoint.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the endpoint cannot be created or secured for this process owner.
        pub fn bind(endpoint: &Path) -> io::Result<Self> {
            let socket = UnixListener::bind(endpoint)?;
            let result = fs::set_permissions(endpoint, fs::Permissions::from_mode(0o600))
                .and_then(|()| socket.set_nonblocking(true));
            if let Err(error) = result {
                drop(socket);
                let _ = fs::remove_file(endpoint);
                return Err(error);
            }
            Ok(Self(socket))
        }

        /// Accept one connection after checking the peer's process owner.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if accepting the transport or validating its peer owner fails.
        pub fn accept(&self) -> io::Result<LocalStream> {
            let (stream, _) = self.0.accept()?;
            require_peer_owner(&stream)?;
            Ok(LocalStream(stream))
        }

        /// Change whether local transport operations block.
        ///
        /// # Errors
        ///
        /// Returns the operating system error if the transport mode cannot be changed.
        pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
            self.0.set_nonblocking(on)
        }
    }

    impl AsRawFd for LocalListener {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }
    }

    impl AsFd for LocalListener {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }

    /// A local automation connection with blocking reads and explicit shutdown.
    pub struct LocalStream(UnixStream);

    impl LocalStream {
        /// Connect to a local automation endpoint.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the endpoint is unavailable or the connection cannot be established.
        pub fn connect(endpoint: &Path) -> io::Result<Self> {
            let stream = UnixStream::connect(endpoint)?;
            require_peer_owner(&stream)?;
            Ok(Self(stream))
        }

        /// Clone this transport handle while preserving its underlying connection.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the operating system cannot duplicate the transport handle.
        pub fn try_clone(&self) -> io::Result<Self> {
            self.0.try_clone().map(Self)
        }

        /// Change whether local transport operations block.
        ///
        /// # Errors
        ///
        /// Returns the operating system error if the transport mode cannot be changed.
        pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
            self.0.set_nonblocking(on)
        }

        /// Set the maximum time a blocking transport write may wait.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the operating system rejects the requested timeout.
        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.set_write_timeout(timeout)
        }

        /// Shut down both directions of the connection.
        ///
        /// # Errors
        ///
        /// Returns the operating system error if the transport cannot be shut down.
        pub fn shutdown(&self) -> io::Result<()> {
            self.0.shutdown(std::net::Shutdown::Both)
        }

        #[cfg(test)]
        ///
        /// # Errors
        ///
        /// Returns an I/O error if a connected local transport pair cannot be created.
        pub fn pair() -> io::Result<(Self, Self)> {
            let (left, right) = UnixStream::pair()?;
            Ok((Self(left), Self(right)))
        }
    }

    impl Read for LocalStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.0.read(buffer)
        }
    }

    impl AsRawFd for LocalStream {
        fn as_raw_fd(&self) -> RawFd {
            self.0.as_raw_fd()
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0.write(buffer)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()
        }
    }

    /// Refuse a peer that is not the effective user running this process.
    fn require_peer_owner(stream: &UnixStream) -> io::Result<()> {
        let peer = peer_uid(stream)?;
        // SAFETY: geteuid has no preconditions.
        let owner = unsafe { libc::geteuid() };
        if peer != owner {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("IPC socket peer uid {peer} is not owner uid {owner}"),
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
        let mut credential = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the output buffers have the exact sizes passed to getsockopt.
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut credential).cast(),
                &raw mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(credential.uid)
    }

    #[cfg(not(target_os = "linux"))]
    fn peer_uid(stream: &UnixStream) -> io::Result<libc::uid_t> {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: both out-parameters are valid writable locations for getpeereid.
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &raw mut uid, &raw mut gid) };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;
    use std::sync::{Arc, Mutex};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_INCOMPLETE, ERROR_IO_PENDING,
        ERROR_OPERATION_ABORTED, ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, ERROR_SEM_TIMEOUT,
        GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY,
        TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    };
    use windows_sys::Win32::System::IO::{
        CancelIoEx, GetOverlappedResult, GetOverlappedResultEx, OVERLAPPED,
    };
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeClientProcessId,
        GetNamedPipeServerProcessId, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
        PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, GetCurrentProcess, OpenProcess, OpenProcessToken,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use super::*;

    /// Named-pipe listener with an already-created first instance, preventing endpoint races.
    pub struct LocalListener {
        endpoint: Vec<u16>,
        security: Arc<SecurityDescriptor>,
        pending: Mutex<Option<OwnedHandle>>,
    }

    impl LocalListener {
        /// Bind an owner-restricted local automation endpoint.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the endpoint cannot be created or secured for this process owner.
        pub fn bind(endpoint: &Path) -> io::Result<Self> {
            let endpoint = wide_path(endpoint)?;
            let security = Arc::new(SecurityDescriptor::for_current_user()?);
            let pending = create_pipe(&endpoint, &security, true)?;
            Ok(Self { endpoint, security, pending: Mutex::new(Some(pending)) })
        }

        /// Accept one connection after checking the peer's process owner.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if accepting the transport or validating its peer owner fails.
        pub fn accept(&self) -> io::Result<LocalStream> {
            let connected = {
                let mut slot = self.pending.lock().unwrap_or_else(|error| error.into_inner());
                let connected = slot.take().ok_or_else(|| io::Error::other("missing pipe"))?;
                let event =
                    // SAFETY: Null name and security pointers select defaults; the returned event is checked and owned by the guard.
                    OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
                let mut overlapped = OVERLAPPED { hEvent: event.raw(), ..OVERLAPPED::default() };
                // SAFETY: The pipe and event handles are owned here; the OVERLAPPED storage remains live until completion is awaited.
                let result = unsafe { ConnectNamedPipe(connected.raw(), &mut overlapped) };
                if result == 0 {
                    match last_error_code() {
                        ERROR_PIPE_CONNECTED => {},
                        ERROR_IO_PENDING => {
                            let mut transferred = 0;
                            // SAFETY: The pending operation owns its handle, event, and OVERLAPPED storage; the blocking wait completes before storage is released.
                            if unsafe {
                                GetOverlappedResult(
                                    connected.raw(),
                                    &overlapped,
                                    &mut transferred,
                                    1,
                                )
                            } == 0
                            {
                                *slot = Some(connected);
                                return Err(io::Error::last_os_error());
                            }
                        },
                        _ => {
                            *slot = Some(connected);
                            return Err(io::Error::last_os_error());
                        },
                    }
                }
                *slot = Some(create_pipe(&self.endpoint, &self.security, false)?);
                connected
            };
            require_pipe_client_owner(connected.raw())?;
            Ok(LocalStream::from_handle(connected, true))
        }

        /// Named-pipe accept is intentionally driven by a blocking background thread.
        /// Change whether local transport operations block.
        ///
        /// # Errors
        ///
        /// Returns the operating system error if the transport mode cannot be changed.
        pub fn set_nonblocking(&self, _on: bool) -> io::Result<()> {
            Ok(())
        }
    }

    pub struct LocalStream {
        handle: Arc<OwnedHandle>,
        server_end: bool,
        write_timeout: Arc<Mutex<Option<Duration>>>,
    }

    impl LocalStream {
        fn from_handle(handle: OwnedHandle, server_end: bool) -> Self {
            Self { handle: Arc::new(handle), server_end, write_timeout: Arc::new(Mutex::new(None)) }
        }

        /// Connect to a local automation endpoint.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the endpoint is unavailable or the connection cannot be established.
        pub fn connect(endpoint: &Path) -> io::Result<Self> {
            let endpoint = wide_path(endpoint)?;
            // SAFETY: The endpoint is a live NUL-terminated UTF-16 buffer, borrowed only for this bounded wait.
            if unsafe { WaitNamedPipeW(endpoint.as_ptr(), 3_000) } == 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: The endpoint is NUL-terminated; null optional pointers select defaults and the returned handle is checked.
            let handle = unsafe {
                CreateFileW(
                    endpoint.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };
            let handle = OwnedHandle::new(handle)?;
            require_pipe_server_owner(handle.raw())?;
            Ok(Self::from_handle(handle, false))
        }

        /// Clone this transport handle while preserving its underlying connection.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the operating system cannot duplicate the transport handle.
        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                handle: self.handle.clone(),
                server_end: self.server_end,
                write_timeout: self.write_timeout.clone(),
            })
        }

        /// Change whether local transport operations block.
        ///
        /// # Errors
        ///
        /// Returns the operating system error if the transport mode cannot be changed.
        pub fn set_nonblocking(&self, _on: bool) -> io::Result<()> {
            Ok(())
        }

        /// Set the maximum time a blocking transport write may wait.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the operating system rejects the requested timeout.
        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            *self.write_timeout.lock().unwrap_or_else(|error| error.into_inner()) = timeout;
            Ok(())
        }

        /// Process ID of the owner-side named-pipe server.
        ///
        /// # Errors
        ///
        /// Returns an I/O error if the connected pipe cannot identify its server.
        pub fn server_process_id(&self) -> io::Result<u32> {
            let mut pid = 0;
            // SAFETY: The connected pipe handle is live and the PID output is an initialized writable local.
            if unsafe { GetNamedPipeServerProcessId(self.handle.raw(), &mut pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(pid)
        }

        /// Shut down both directions of the connection.
        ///
        /// # Errors
        ///
        /// Returns the operating system error if the transport cannot be shut down.
        pub fn shutdown(&self) -> io::Result<()> {
            // SAFETY: The pipe is live; cancellation is followed by a completion wait before any OVERLAPPED storage is released.
            unsafe {
                CancelIoEx(self.handle.raw(), ptr::null());
                if self.server_end {
                    DisconnectNamedPipe(self.handle.raw());
                }
            }
            Ok(())
        }

        #[cfg(test)]
        /// Create a connected, owner-authenticated named-pipe pair.
        /// # Errors
        ///
        /// Returns an I/O error if a connected local transport pair cannot be created.
        pub fn pair() -> io::Result<(Self, Self)> {
            use std::sync::atomic::{AtomicU64, Ordering};

            static NEXT_PAIR: AtomicU64 = AtomicU64::new(1);
            let endpoint = std::path::PathBuf::from(format!(
                r"\\.\pipe\vivido-test-{}-{}",
                std::process::id(),
                NEXT_PAIR.fetch_add(1, Ordering::Relaxed)
            ));
            let listener = LocalListener::bind(&endpoint)?;
            let connector = std::thread::spawn(move || LocalStream::connect(&endpoint));
            let server = listener.accept()?;
            let client = connector
                .join()
                .map_err(|_| io::Error::other("named-pipe connector panicked"))??;
            Ok((client, server))
        }
    }

    impl Read for LocalStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            // SAFETY: Null name and security pointers select defaults; the returned event is checked and owned by the guard.
            let event = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
            let mut overlapped = OVERLAPPED { hEvent: event.raw(), ..OVERLAPPED::default() };
            let mut transferred = 0;
            // SAFETY: The mutable slice bounds the native write and remains exclusively borrowed until the overlapped operation completes.
            let result = unsafe {
                ReadFile(
                    self.handle.raw(),
                    buffer.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                    &mut transferred,
                    &mut overlapped,
                )
            };
            if result == 0 && last_error_code() != ERROR_IO_PENDING {
                return pipe_read_error();
            }
            if result == 0
                // SAFETY: The pending operation owns its handle, event, and OVERLAPPED storage; the blocking wait completes before storage is released.
                && unsafe {
                    GetOverlappedResult(self.handle.raw(), &overlapped, &mut transferred, 1)
                } == 0
            {
                return pipe_read_error();
            }
            Ok(transferred as usize)
        }
    }

    impl Write for LocalStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            // SAFETY: Null name and security pointers select defaults; the returned event is checked and owned by the guard.
            let event = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
            let mut overlapped = OVERLAPPED { hEvent: event.raw(), ..OVERLAPPED::default() };
            let mut transferred = 0;
            // SAFETY: The slice remains live through the write; the owned event and OVERLAPPED are retained through completion or cancellation.
            let result = unsafe {
                WriteFile(
                    self.handle.raw(),
                    buffer.as_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                    &mut transferred,
                    &mut overlapped,
                )
            };
            if result == 0 && last_error_code() != ERROR_IO_PENDING {
                return Err(pipe_write_error());
            }
            if result == 0 {
                let timeout = *self.write_timeout.lock().unwrap_or_else(|error| error.into_inner());
                let complete = if let Some(timeout) = timeout {
                    // SAFETY: The handle, event, transfer count, and OVERLAPPED belong to this pending operation and remain live until completion or cancellation.
                    unsafe {
                        GetOverlappedResultEx(
                            self.handle.raw(),
                            &overlapped,
                            &mut transferred,
                            u32::try_from(timeout.as_millis())
                                .unwrap_or(u32::MAX - 1)
                                .min(u32::MAX - 1),
                            0,
                        )
                    }
                } else {
                    // SAFETY: The pending operation owns its handle, event, and OVERLAPPED storage; the blocking wait completes before storage is released.
                    unsafe {
                        GetOverlappedResult(self.handle.raw(), &overlapped, &mut transferred, 1)
                    }
                };
                if complete == 0 {
                    // Capture the failure before cancellation changes the thread's last error.
                    let error = pipe_write_error();
                    let timed_out = matches!(
                        error.raw_os_error().map(|code| code as u32),
                        Some(WAIT_TIMEOUT | ERROR_IO_INCOMPLETE | ERROR_SEM_TIMEOUT)
                    );
                    // SAFETY: Every failed wait may leave I/O pending. The handle, buffer, event,
                    // and OVERLAPPED remain live until cancellation's blocking completion wait
                    // retires the operation, including when it completed concurrently.
                    unsafe {
                        CancelIoEx(self.handle.raw(), &overlapped);
                        GetOverlappedResult(self.handle.raw(), &overlapped, &mut transferred, 1);
                    }
                    return Err(if timed_out {
                        io::Error::new(io::ErrorKind::TimedOut, "named-pipe write timed out")
                    } else {
                        error
                    });
                }
            }
            Ok(transferred as usize)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn create_pipe(
        endpoint: &[u16],
        security: &SecurityDescriptor,
        first: bool,
    ) -> io::Result<OwnedHandle> {
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.pointer.cast(),
            bInheritHandle: 0,
        };
        let first_flag = if first {
            windows_sys::Win32::Storage::FileSystem::FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
        // SAFETY: The endpoint is NUL-terminated and the immutable security descriptor remains alive through this call; the returned handle is checked.
        let handle = unsafe {
            CreateNamedPipeW(
                endpoint.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | first_flag,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &attributes,
            )
        };
        OwnedHandle::new(handle)
    }

    /// Adapted from vvmux's shipping owner-only Windows named-pipe transport.
    struct SecurityDescriptor {
        pointer: PSECURITY_DESCRIPTOR,
    }

    // SAFETY: The descriptor is immutable after creation; its allocation is retained until the last Arc owner drops it.
    unsafe impl Send for SecurityDescriptor {}
    // SAFETY: The descriptor is immutable after creation; its allocation is retained until the last Arc owner drops it.
    unsafe impl Sync for SecurityDescriptor {}

    impl SecurityDescriptor {
        fn for_current_user() -> io::Result<Self> {
            let token = ProcessToken::current()?;
            let sid = token.sid_string()?;
            let sddl = wide_string(&format!("O:{sid}G:{sid}D:P(A;;GA;;;SY)(A;;GA;;;{sid})"))?;
            let mut pointer = ptr::null_mut();
            // SAFETY: The SDDL is NUL-terminated; the writable output receives an owned allocation freed with LocalFree.
            if unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut pointer,
                    ptr::null_mut(),
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self { pointer })
            }
        }
    }

    impl Drop for SecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: This pointer is an owned allocation returned by the matching Windows allocation API and is released once.
            unsafe { LocalFree(self.pointer.cast()) };
        }
    }

    struct OwnedHandle(HANDLE);

    // SAFETY: Windows kernel handles may be used across threads; shared ownership prevents closure during an operation.
    unsafe impl Send for OwnedHandle {}
    // SAFETY: Windows kernel handles may be used across threads; shared ownership prevents closure during an operation.
    unsafe impl Sync for OwnedHandle {}

    impl OwnedHandle {
        fn new(handle: HANDLE) -> io::Result<Self> {
            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: This scope owns the checked native handle and releases it exactly once after its final use.
            unsafe { CloseHandle(self.0) };
        }
    }

    fn last_error_code() -> u32 {
        io::Error::last_os_error().raw_os_error().unwrap_or_default() as u32
    }

    fn pipe_read_error() -> io::Result<usize> {
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED) => Ok(0),
            _ => Err(error),
        }
    }

    fn pipe_write_error() -> io::Error {
        let error = io::Error::last_os_error();
        match error.raw_os_error().map(|code| code as u32) {
            Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED) => {
                io::Error::new(io::ErrorKind::BrokenPipe, "named-pipe connection closed")
            },
            _ => error,
        }
    }

    struct ProcessToken {
        _token: OwnedHandle,
        buffer: Vec<usize>,
    }

    impl ProcessToken {
        fn current() -> io::Result<Self> {
            // SAFETY: The pseudo-handle is borrowed and never closed; the query takes no arguments.
            Self::from_process(unsafe { GetCurrentProcess() })
        }

        fn for_pid(pid: u32) -> io::Result<Self> {
            // SAFETY: Only scalar access flags and a PID are supplied; the returned handle is checked before use.
            let process = OwnedHandle::new(unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
            })?;
            Self::from_process(process.raw())
        }

        fn from_process(process: HANDLE) -> io::Result<Self> {
            let mut token = ptr::null_mut();
            // SAFETY: The process handle is live and the output receives a checked owned token handle.
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle::new(token)?;
            let mut length = 0;
            // SAFETY: The token is live; a null sizing query or pointer-aligned buffer with the reported byte capacity bounds the native write.
            unsafe { GetTokenInformation(token.raw(), TokenUser, ptr::null_mut(), 0, &mut length) };
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            if length < std::mem::size_of::<TOKEN_USER>() as u32 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "short token information"));
            }
            // TOKEN_USER contains pointers and requires pointer-aligned backing storage.
            let mut buffer = vec![0usize; (length as usize).div_ceil(std::mem::size_of::<usize>())];
            // SAFETY: The token is live; a null sizing query or pointer-aligned buffer with the reported byte capacity bounds the native write.
            if unsafe {
                GetTokenInformation(
                    token.raw(),
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    length,
                    &mut length,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { _token: token, buffer })
        }

        fn sid(&self) -> *mut core::ffi::c_void {
            // SAFETY: GetTokenInformation filled the buffer with one TOKEN_USER.
            unsafe { (*(self.buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid }
        }

        fn sid_string(&self) -> io::Result<String> {
            let mut string = ptr::null_mut();
            // SAFETY: The SID points inside the retained token buffer; the output receives an owned NUL-terminated UTF-16 allocation.
            if unsafe { ConvertSidToStringSidW(self.sid(), &mut string) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut length = 0;
            // SAFETY: ConvertSidToStringSidW guarantees a NUL-terminated allocation which remains owned until LocalFree below.
            unsafe {
                while *string.add(length) != 0 {
                    length += 1;
                }
            }
            // SAFETY: The scan found the terminator within the Windows-owned SID string, which remains live for this slice.
            let result = String::from_utf16(unsafe { std::slice::from_raw_parts(string, length) })
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user SID is not UTF-16"));
            // SAFETY: This pointer is an owned allocation returned by the matching Windows allocation API and is released once.
            unsafe { LocalFree(string.cast()) };
            result
        }
    }

    fn require_pipe_client_owner(handle: HANDLE) -> io::Result<()> {
        let mut pid = 0;
        // SAFETY: The connected pipe handle is live and the PID output is an initialized writable local.
        if unsafe { GetNamedPipeClientProcessId(handle, &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        require_process_owner(pid)
    }

    fn require_pipe_server_owner(handle: HANDLE) -> io::Result<()> {
        let mut pid = 0;
        // SAFETY: The connected pipe handle is live and the PID output is an initialized writable local.
        if unsafe { GetNamedPipeServerProcessId(handle, &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        require_process_owner(pid)
    }

    fn require_process_owner(pid: u32) -> io::Result<()> {
        let current = ProcessToken::current()?;
        let peer = ProcessToken::for_pid(pid)?;
        // SAFETY: Both SID pointers refer to TOKEN_USER data in live retained token buffers.
        if unsafe { EqualSid(current.sid(), peer.sid()) } == 0 {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named-pipe peer belongs to a different Windows user",
            ))
        } else {
            Ok(())
        }
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let value: Vec<u16> = path.as_os_str().encode_wide().collect();
        if value.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "pipe name contains NUL"));
        }
        Ok(value.into_iter().chain(std::iter::once(0)).collect())
    }

    fn wide_string(value: &str) -> io::Result<Vec<u16>> {
        if value.encode_utf16().any(|unit| unit == 0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "Windows string contains NUL"));
        }
        Ok(value.encode_utf16().chain(std::iter::once(0)).collect())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn timed_out_write_retires_pending_io_before_releasing_its_buffer() {
            for timeout in [Duration::ZERO, Duration::from_millis(1)] {
                let (mut writer, peer) = LocalStream::pair().unwrap();
                writer.set_write_timeout(Some(timeout)).unwrap();
                // Exceed the pipe's 64 KiB capacity while the peer never drains it.
                let buffer = vec![0x5a; 8 * 1024 * 1024];
                let error = writer.write(&buffer).unwrap_err();
                assert_eq!(error.kind(), io::ErrorKind::TimedOut);
                drop(buffer);
                writer.shutdown().unwrap();
                drop(peer);
            }
        }
    }
}

pub use platform::{LocalListener, LocalStream};

// Debug omits user content and native resources, and never acquires application locks.
impl std::fmt::Debug for LocalListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalListener").finish_non_exhaustive()
    }
}

// Debug omits user content and native resources, and never acquires application locks.
impl std::fmt::Debug for LocalStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalStream").finish_non_exhaustive()
    }
}
