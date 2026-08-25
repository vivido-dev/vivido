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

    pub struct LocalListener(UnixListener);

    impl LocalListener {
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

        pub fn accept(&self) -> io::Result<LocalStream> {
            let (stream, _) = self.0.accept()?;
            require_peer_owner(&stream)?;
            Ok(LocalStream(stream))
        }

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

    pub struct LocalStream(UnixStream);

    impl LocalStream {
        pub fn connect(endpoint: &Path) -> io::Result<Self> {
            let stream = UnixStream::connect(endpoint)?;
            require_peer_owner(&stream)?;
            Ok(Self(stream))
        }

        pub fn try_clone(&self) -> io::Result<Self> {
            self.0.try_clone().map(Self)
        }

        pub fn set_nonblocking(&self, on: bool) -> io::Result<()> {
            self.0.set_nonblocking(on)
        }

        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            self.0.set_write_timeout(timeout)
        }

        pub fn shutdown(&self) -> io::Result<()> {
            self.0.shutdown(std::net::Shutdown::Both)
        }

        #[cfg(test)]
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
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_OPERATION_ABORTED,
        ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, ERROR_SEM_TIMEOUT, GENERIC_READ,
        GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
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
        pub fn bind(endpoint: &Path) -> io::Result<Self> {
            let endpoint = wide_path(endpoint)?;
            let security = Arc::new(SecurityDescriptor::for_current_user()?);
            let pending = create_pipe(&endpoint, &security, true)?;
            Ok(Self { endpoint, security, pending: Mutex::new(Some(pending)) })
        }

        pub fn accept(&self) -> io::Result<LocalStream> {
            let connected = {
                let mut slot = self.pending.lock().unwrap_or_else(|error| error.into_inner());
                let connected = slot.take().ok_or_else(|| io::Error::other("missing pipe"))?;
                let event =
                    OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
                let mut overlapped = OVERLAPPED { hEvent: event.raw(), ..OVERLAPPED::default() };
                let result = unsafe { ConnectNamedPipe(connected.raw(), &mut overlapped) };
                if result == 0 {
                    match last_error_code() {
                        ERROR_PIPE_CONNECTED => {},
                        ERROR_IO_PENDING => {
                            let mut transferred = 0;
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

        pub fn connect(endpoint: &Path) -> io::Result<Self> {
            let endpoint = wide_path(endpoint)?;
            if unsafe { WaitNamedPipeW(endpoint.as_ptr(), 3_000) } == 0 {
                return Err(io::Error::last_os_error());
            }
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

        pub fn try_clone(&self) -> io::Result<Self> {
            Ok(Self {
                handle: self.handle.clone(),
                server_end: self.server_end,
                write_timeout: self.write_timeout.clone(),
            })
        }

        pub fn set_nonblocking(&self, _on: bool) -> io::Result<()> {
            Ok(())
        }

        pub fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            *self.write_timeout.lock().unwrap_or_else(|error| error.into_inner()) = timeout;
            Ok(())
        }

        /// Process ID of the owner-side named-pipe server.
        pub fn server_process_id(&self) -> io::Result<u32> {
            let mut pid = 0;
            if unsafe { GetNamedPipeServerProcessId(self.handle.raw(), &mut pid) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(pid)
        }

        pub fn shutdown(&self) -> io::Result<()> {
            unsafe {
                CancelIoEx(self.handle.raw(), ptr::null());
                if self.server_end {
                    DisconnectNamedPipe(self.handle.raw());
                }
            }
            Ok(())
        }

        #[cfg(test)]
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
            let event = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
            let mut overlapped = OVERLAPPED { hEvent: event.raw(), ..OVERLAPPED::default() };
            let mut transferred = 0;
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
            let event = OwnedHandle::new(unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) })?;
            let mut overlapped = OVERLAPPED { hEvent: event.raw(), ..OVERLAPPED::default() };
            let mut transferred = 0;
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
                    unsafe {
                        GetOverlappedResultEx(
                            self.handle.raw(),
                            &overlapped,
                            &mut transferred,
                            u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1),
                            0,
                        )
                    }
                } else {
                    unsafe {
                        GetOverlappedResult(self.handle.raw(), &overlapped, &mut transferred, 1)
                    }
                };
                if complete == 0 {
                    if last_error_code() == ERROR_SEM_TIMEOUT {
                        unsafe {
                            CancelIoEx(self.handle.raw(), &overlapped);
                            GetOverlappedResult(
                                self.handle.raw(),
                                &overlapped,
                                &mut transferred,
                                1,
                            );
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "named-pipe write timed out",
                        ));
                    }
                    return Err(pipe_write_error());
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

    unsafe impl Send for SecurityDescriptor {}
    unsafe impl Sync for SecurityDescriptor {}

    impl SecurityDescriptor {
        fn for_current_user() -> io::Result<Self> {
            let token = ProcessToken::current()?;
            let sid = token.sid_string()?;
            let sddl = wide_string(&format!("O:{sid}G:{sid}D:P(A;;GA;;;SY)(A;;GA;;;{sid})"))?;
            let mut pointer = ptr::null_mut();
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
            unsafe { LocalFree(self.pointer.cast()) };
        }
    }

    struct OwnedHandle(HANDLE);

    unsafe impl Send for OwnedHandle {}
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
        buffer: Vec<u8>,
    }

    impl ProcessToken {
        fn current() -> io::Result<Self> {
            Self::from_process(unsafe { GetCurrentProcess() })
        }

        fn for_pid(pid: u32) -> io::Result<Self> {
            let process = OwnedHandle::new(unsafe {
                OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid)
            })?;
            Self::from_process(process.raw())
        }

        fn from_process(process: HANDLE) -> io::Result<Self> {
            let mut token = ptr::null_mut();
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let token = OwnedHandle::new(token)?;
            let mut length = 0;
            unsafe { GetTokenInformation(token.raw(), TokenUser, ptr::null_mut(), 0, &mut length) };
            if length == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0u8; length as usize];
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
            if unsafe { ConvertSidToStringSidW(self.sid(), &mut string) } == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut length = 0;
            unsafe {
                while *string.add(length) != 0 {
                    length += 1;
                }
            }
            let result = String::from_utf16(unsafe { std::slice::from_raw_parts(string, length) })
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "user SID is not UTF-16"));
            unsafe { LocalFree(string.cast()) };
            result
        }
    }

    fn require_pipe_client_owner(handle: HANDLE) -> io::Result<()> {
        let mut pid = 0;
        if unsafe { GetNamedPipeClientProcessId(handle, &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        require_process_owner(pid)
    }

    fn require_pipe_server_owner(handle: HANDLE) -> io::Result<()> {
        let mut pid = 0;
        if unsafe { GetNamedPipeServerProcessId(handle, &mut pid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        require_process_owner(pid)
    }

    fn require_process_owner(pid: u32) -> io::Result<()> {
        let current = ProcessToken::current()?;
        let peer = ProcessToken::for_pid(pid)?;
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
}

pub use platform::{LocalListener, LocalStream};
