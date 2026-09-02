//! Owner-only rendezvous and process identity for headless Vivido sessions.
//!
//! Ported from `vvland/src/linux/runtime.rs`, which solved the same problem for headless desktop
//! sessions. The invariant it exists to enforce: a registry file is not proof that its PID still
//! names the daemon that wrote it. Every liveness and teardown decision therefore includes process
//! birth time, and every deletion compares the complete recorded instance before removing a socket
//! or registry belonging to a possibly-different daemon.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::polling::ipc::PROTOCOL_VERSION;

const REGISTRY_SCHEMA: u32 = 1;
const MAX_REGISTRY_BYTES: u64 = 16 * 1024;
const MAX_SESSION_NAME: usize = 64;

/// Filesystem names for one session.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    pub socket: PathBuf,
    pub registry: PathBuf,
    endpoint_id: String,
}

/// Identity of the process that owns a session, beyond its recyclable PID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum ProcessBirth {
    Linux { start_ticks: u64 },
    Macos { start_micros: u64 },
    Windows { creation_time: u64 },
}

/// Published same-user rendezvous for a running headed or headless instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionRegistry {
    pub schema: u32,
    pub name: String,
    pub pid: u32,
    pub instance_nonce: String,
    pub vivido_version: String,
    pub protocol_version: u16,
    pub endpoint_id: String,
    pub process_birth: ProcessBirth,
    pub socket: PathBuf,
    #[serde(default = "default_headless_registry")]
    pub headless: bool,
    pub columns: u16,
    pub lines: u16,
}

fn default_headless_registry() -> bool {
    true
}

impl SessionPaths {
    pub fn for_session(name: &str) -> io::Result<Self> {
        Self::for_session_in_root(name, &runtime_root()?)
    }

    /// Build registry paths for a process whose IPC endpoint was explicitly selected.
    pub fn for_endpoint(name: &str, socket: PathBuf) -> io::Result<Self> {
        Self::for_endpoint_in_root(name, &runtime_root()?, socket)
    }

    fn for_session_in_root(name: &str, root: &Path) -> io::Result<Self> {
        validate_session_name(name)?;
        #[cfg(unix)]
        let uid = effective_uid();
        #[cfg(windows)]
        let uid = 0;
        ensure_private_directory(root, uid)?;
        let hash = hex(&Sha256::digest(name.as_bytes())[..16]);
        #[cfg(unix)]
        let socket = root.join(format!("session-{hash}.sock"));
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\vivido-session-{hash}"));
        Self::for_endpoint_in_root(name, root, socket)
    }

    fn for_endpoint_in_root(name: &str, root: &Path, socket: PathBuf) -> io::Result<Self> {
        validate_session_name(name)?;
        #[cfg(unix)]
        let uid = effective_uid();
        #[cfg(windows)]
        let uid = 0;
        ensure_private_directory(root, uid)?;
        let hash = hex(&Sha256::digest(name.as_bytes())[..16]);
        let registry = root.join(format!("session-{hash}.json"));
        let endpoint_id =
            hex(&domain_hash(b"vivido endpoint identity v1\0", &endpoint_identity_bytes(&socket)));
        Ok(Self { socket, registry, endpoint_id })
    }

    /// Publish rendezvous once the IPC socket is bound.
    pub fn write_registry(
        &self,
        name: &str,
        nonce: &[u8; 32],
        dimensions: (u16, u16),
        headless: bool,
    ) -> io::Result<SessionRegistry> {
        let registry = SessionRegistry {
            schema: REGISTRY_SCHEMA,
            name: name.to_owned(),
            pid: std::process::id(),
            instance_nonce: hex(nonce),
            vivido_version: env!("VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
            endpoint_id: self.endpoint_id.clone(),
            process_birth: process_birth(std::process::id())?,
            socket: self.socket.clone(),
            headless,
            columns: dimensions.0,
            lines: dimensions.1,
        };
        validate_registry(&registry)?;
        self.validate_identity(&registry)?;
        write_registry_file(&self.registry, &registry)?;
        Ok(registry)
    }

    /// Publish this process as the owner of the prepared endpoint and return its cleanup guard.
    ///
    /// The endpoint listener must already be bound before this is called. Dropping the returned
    /// guard removes only registry/socket state which still belongs to this exact process.
    pub fn register(
        self,
        name: &str,
        dimensions: (u16, u16),
        headless: bool,
    ) -> io::Result<RegistryGuard> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(io::Error::other)?;
        let registry = self.write_registry(name, &nonce, dimensions, headless)?;
        Ok(RegistryGuard::new(self, registry))
    }

    pub fn read_registry(&self) -> io::Result<SessionRegistry> {
        let registry = read_registry_file(&self.registry)?;
        self.validate_identity(&registry)?;
        Ok(registry)
    }

    /// Reserve this session's names, reaping only a proven-stale prior instance.
    pub fn prepare_endpoint(&self, name: &str) -> io::Result<()> {
        match self.read_registry() {
            Ok(registry) if registry_process_matches(&registry) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("vivido session {name:?} is already running"),
                ));
            },
            Ok(registry) => {
                self.remove_instance(&registry)?;
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                #[cfg(unix)]
                if self.socket.exists() {
                    match UnixStream::connect(&self.socket) {
                        Ok(_) => {
                            return Err(io::Error::new(
                                io::ErrorKind::AlreadyExists,
                                format!("vivido session {name:?} is already starting"),
                            ));
                        },
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                            ) =>
                        {
                            match fs::remove_file(&self.socket) {
                                Ok(()) => {},
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                                Err(error) => return Err(error),
                            }
                        },
                        Err(error) => return Err(error),
                    }
                }
                #[cfg(windows)]
                match crate::polling::transport::LocalStream::connect(&self.socket) {
                    Ok(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            format!("vivido session {name:?} is already starting"),
                        ));
                    },
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound
                                | io::ErrorKind::ConnectionRefused
                                | io::ErrorKind::TimedOut
                        ) => {},
                    Err(error) => return Err(error),
                }
            },
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn validate_identity(&self, registry: &SessionRegistry) -> io::Result<()> {
        if registry.socket != self.socket || registry.endpoint_id != self.endpoint_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session registry endpoint identity does not match its session name",
            ));
        }
        Ok(())
    }

    /// Remove artifacts only if the registry still records the exact expected daemon instance.
    pub fn remove_instance(&self, expected: &SessionRegistry) -> io::Result<bool> {
        let actual = match self.read_registry() {
            Ok(actual) => actual,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !same_instance(&actual, expected) {
            return Ok(false);
        }

        // Revalidate immediately before each mutation. A replacement daemon may legitimately use
        // the same session paths, and teardown for the old instance must never remove its files.
        if self.read_registry().is_ok_and(|current| same_instance(&current, expected)) {
            #[cfg(unix)]
            match fs::remove_file(&self.socket) {
                Ok(()) => {},
                Err(error) if error.kind() == io::ErrorKind::NotFound => {},
                Err(error) => return Err(error),
            }
        } else {
            return Ok(false);
        }
        if self.read_registry().is_ok_and(|current| same_instance(&current, expected)) {
            fs::remove_file(&self.registry)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

/// Removes a session's registry when the daemon that owns it shuts down.
pub struct RegistryGuard {
    paths: SessionPaths,
    registry: SessionRegistry,
}

impl RegistryGuard {
    pub fn new(paths: SessionPaths, registry: SessionRegistry) -> Self {
        Self { paths, registry }
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        let _ = self.paths.remove_instance(&self.registry);
    }
}

pub fn validate_session_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > MAX_SESSION_NAME
        || name.starts_with('.')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session name must be 1-64 ASCII letters, digits, '.', '-' or '_' and not start '.'",
        ));
    }
    Ok(())
}

/// Every live session, reaping registries whose process is gone.
pub fn list_registries() -> io::Result<Vec<SessionRegistry>> {
    list_registries_in_root(&runtime_root()?)
}

fn list_registries_in_root(root: &Path) -> io::Result<Vec<SessionRegistry>> {
    let mut sessions = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with("session-") || !file_name.ends_with(".json") {
            continue;
        }
        let Ok(registry) = read_registry_file(&entry.path()) else {
            continue;
        };
        let Ok(paths) =
            SessionPaths::for_endpoint_in_root(&registry.name, root, registry.socket.clone())
        else {
            continue;
        };
        if paths.registry != entry.path() || paths.validate_identity(&registry).is_err() {
            continue;
        }
        if registry_process_matches(&registry) {
            sessions.push(registry);
        } else {
            let _ = paths.remove_instance(&registry);
        }
    }
    sessions.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(sessions)
}

/// Resolve one registered headed or headless instance by exact name.
pub fn registered_instance(name: &str) -> io::Result<SessionRegistry> {
    validate_session_name(name)?;
    let root = runtime_root()?;
    let derived = SessionPaths::for_session_in_root(name, &root)?;
    let registry = read_registry_file(&derived.registry)?;
    let paths = SessionPaths::for_endpoint_in_root(name, &root, registry.socket.clone())?;
    if paths.registry != derived.registry || paths.validate_identity(&registry).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Vivido registry endpoint identity is invalid",
        ));
    }
    if !registry_process_matches(&registry) {
        let _ = paths.remove_instance(&registry);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Vivido instance {name:?} is no longer running"),
        ));
    }
    Ok(registry)
}

pub fn print_sessions() -> io::Result<()> {
    for session in list_registries()?.into_iter().filter(|session| session.headless) {
        println!(
            "{}\tpid {}\t{}x{}\t{}",
            session.name,
            session.pid,
            session.columns,
            session.lines,
            session.socket.display()
        );
    }
    Ok(())
}

/// Ask a session's daemon to shut down, refusing to signal a recycled PID.
pub fn terminate_session(name: &str) -> io::Result<()> {
    let registry = registered_instance(name).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("vivido session {name:?} does not exist"),
            )
        } else {
            error
        }
    })?;
    if !registry.headless {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Vivido instance {name:?} is headed; use `vivido msg --target {name} quit`"),
        ));
    }
    let paths = SessionPaths::for_endpoint(name, registry.socket.clone())?;

    signal_session(&paths, &registry)
}

/// Send `SIGTERM`, re-checking birth time so a PID recycled mid-call cannot be signalled.
#[cfg(target_os = "linux")]
fn signal_session(paths: &SessionPaths, registry: &SessionRegistry) -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, registry.pid, 0) };
    if fd < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            let _ = paths.remove_instance(registry);
        }
        return Err(error);
    }
    // SAFETY: pidfd_open returned a new owned descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(fd as i32) };

    // Opening a pidfd pins the process object. Re-read birth time afterward so a PID recycled
    // between the liveness check and pidfd_open cannot receive the signal.
    if !registry_process_matches(registry) {
        let _ = paths.remove_instance(registry);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("vivido session {:?} is no longer running", registry.name),
        ));
    }

    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// macOS has no pidfd, so re-check birth time immediately before signalling.
///
/// This leaves a small window a pidfd would close; it is the best the platform offers.
#[cfg(target_os = "macos")]
fn signal_session(paths: &SessionPaths, registry: &SessionRegistry) -> io::Result<()> {
    if !registry_process_matches(registry) {
        let _ = paths.remove_instance(registry);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("vivido session {:?} is no longer running", registry.name),
        ));
    }

    let result = unsafe { libc::kill(registry.pid as libc::pid_t, libc::SIGTERM) };
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Terminate through a pinned process handle after re-verifying its creation time.
#[cfg(windows)]
fn signal_session(paths: &SessionPaths, registry: &SessionRegistry) -> io::Result<()> {
    use windows_sys::Win32::System::Threading::{
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, TerminateProcess,
    };

    let process = WindowsProcessHandle::open(
        registry.pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
    )?;
    let actual = process_birth_from_handle(process.raw())?;
    if actual != registry.process_birth {
        let _ = paths.remove_instance(registry);
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("vivido session {:?} is no longer running", registry.name),
        ));
    }

    // This forceful path cannot run RegistryGuard; the next registry listing reaps its artifact.
    if unsafe { TerminateProcess(process.raw(), 1) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub fn registry_process_matches(registry: &SessionRegistry) -> bool {
    process_birth(registry.pid).is_ok_and(|actual| actual == registry.process_birth)
}

fn read_registry_file(path: &Path) -> io::Result<SessionRegistry> {
    let bytes = read_registry_bytes(path)?;
    decode_registry(&bytes)
}

fn decode_registry(bytes: &[u8]) -> io::Result<SessionRegistry> {
    let registry: SessionRegistry = serde_json::from_slice(bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid vivido session registry: {error}"),
        )
    })?;
    validate_registry(&registry)?;
    Ok(registry)
}

fn validate_registry(registry: &SessionRegistry) -> io::Result<()> {
    validate_session_name(&registry.name)?;
    let valid = registry.schema == REGISTRY_SCHEMA
        && registry.pid != 0
        && is_lower_hex(&registry.instance_nonce, 64)
        && !registry.vivido_version.is_empty()
        && registry.vivido_version.len() <= 64
        && registry.protocol_version == PROTOCOL_VERSION
        && is_lower_hex(&registry.endpoint_id, 64)
        && registry.socket.is_absolute()
        && registry.columns != 0
        && registry.lines != 0;
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vivido session registry fields are invalid",
        ));
    }
    Ok(())
}

fn read_registry_bytes(path: &Path) -> io::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        let uid = effective_uid();
        let mut options = OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        let metadata = file.metadata()?;
        if !safe_registry_metadata(metadata.is_file(), metadata.uid(), metadata.mode(), uid) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe vivido session registry",
            ));
        }
        read_bounded_registry(&mut file, metadata.len())
    }

    #[cfg(windows)]
    {
        // The registry is rendezvous metadata, not an authority: the named pipe independently
        // enforces an owner/SYSTEM-only DACL and verifies both peer process owners. Keeping the
        // file under the current user's LocalAppData prevents ordinary cross-user replacement.
        let mut file = OpenOptions::new().read(true).open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe vivido session registry",
            ));
        }
        read_bounded_registry(&mut file, metadata.len())
    }
}

#[cfg(unix)]
fn safe_registry_metadata(is_file: bool, uid: u32, mode: u32, expected_uid: u32) -> bool {
    is_file && uid == expected_uid && mode & 0o077 == 0
}

fn read_bounded_registry(reader: &mut impl Read, length: u64) -> io::Result<Vec<u8>> {
    if length > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vivido session registry exceeds 16 KiB",
        ));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    reader.take(MAX_REGISTRY_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vivido session registry exceeds 16 KiB",
        ));
    }
    Ok(bytes)
}

fn write_registry_file(path: &Path, registry: &SessionRegistry) -> io::Result<()> {
    let bytes = serde_json::to_vec(registry).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_REGISTRY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "vivido session registry exceeds 16 KiB",
        ));
    }
    // A unique temporary name keeps two racing daemons from clobbering each other's staging file.
    let temporary = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        &registry.instance_nonce[..16]
    ));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(&temporary)?;
    let result = file.write_all(&bytes).and_then(|()| file.sync_all()).and_then(|()| {
        drop(file);
        fs::rename(&temporary, path)
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn same_instance(left: &SessionRegistry, right: &SessionRegistry) -> bool {
    left.schema == right.schema
        && left.name == right.name
        && left.pid == right.pid
        && left.instance_nonce == right.instance_nonce
        && left.endpoint_id == right.endpoint_id
        && left.process_birth == right.process_birth
        && left.socket == right.socket
}

#[cfg(target_os = "linux")]
fn process_birth(pid: u32) -> io::Result<ProcessBirth> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // The comm field may contain spaces and parentheses, so fields are counted after the last ")".
    let end = stat.rfind(") ").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "malformed Linux process stat")
    })?;
    let start_ticks = stat[end + 2..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Linux process start time is missing")
        })?
        .parse::<u64>()
        .map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Linux process start time is invalid")
        })?;
    Ok(ProcessBirth::Linux { start_ticks })
}

#[cfg(target_os = "macos")]
fn process_birth(pid: u32) -> io::Result<ProcessBirth> {
    let start_micros = crate::macos::proc::start_time_micros(pid as libc::c_int)?;
    Ok(ProcessBirth::Macos { start_micros })
}

#[cfg(windows)]
fn process_birth(pid: u32) -> io::Result<ProcessBirth> {
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

    let process = WindowsProcessHandle::open(pid, PROCESS_QUERY_LIMITED_INFORMATION)?;
    process_birth_from_handle(process.raw())
}

#[cfg(windows)]
fn process_birth_from_handle(
    handle: windows_sys::Win32::Foundation::HANDLE,
) -> io::Result<ProcessBirth> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    let mut creation = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let creation_time =
        (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    Ok(ProcessBirth::Windows { creation_time })
}

#[cfg(windows)]
struct WindowsProcessHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsProcessHandle {
    fn open(pid: u32, access: u32) -> io::Result<Self> {
        use windows_sys::Win32::System::Threading::OpenProcess;

        let handle = unsafe { OpenProcess(access, 0, pid) };
        if handle.is_null() { Err(io::Error::last_os_error()) } else { Ok(Self(handle)) }
    }

    fn raw(&self) -> windows_sys::Win32::Foundation::HANDLE {
        self.0
    }
}

#[cfg(windows)]
impl Drop for WindowsProcessHandle {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

#[cfg(unix)]
fn runtime_root() -> io::Result<PathBuf> {
    let uid = effective_uid();
    let root = if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(path);
        validate_runtime_parent(&path, uid)?;
        path.join("vivido")
    } else {
        PathBuf::from(format!("/tmp/vivido-{uid}"))
    };
    ensure_private_directory(&root, uid)?;
    Ok(root)
}

#[cfg(windows)]
fn runtime_root() -> io::Result<PathBuf> {
    let root = if let Some(root) = std::env::var_os("VIVIDO_RUNTIME_DIR") {
        // An explicit root is useful for hermetic service accounts and tests. Registry files are
        // discovery metadata only; the named pipe remains protected by its DACL and peer SID.
        PathBuf::from(root)
    } else {
        dirs::data_local_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA is unavailable"))?
            .join("vivido")
            .join("sessions")
    };
    ensure_private_directory(&root, 0)?;
    Ok(root)
}

#[cfg(unix)]
fn validate_runtime_parent(path: &Path, uid: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "XDG_RUNTIME_DIR must be an owner-controlled directory",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path, uid: u32) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("unsafe vivido runtime directory {}", path.display()),
                ));
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "runtime directory ownership changed during creation",
                ));
            }
        },
        Err(error) => return Err(error),
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_private_directory(path: &Path, _uid: u32) -> io::Result<()> {
    // `%LOCALAPPDATA%` is a per-user known folder. Endpoint authorization does not rely on this
    // directory: each named pipe has a protected owner/SYSTEM-only DACL and validates peer SIDs.
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("unsafe vivido runtime directory {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn endpoint_identity_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        path.as_os_str().encode_wide().flat_map(u16::to_le_bytes).collect()
    }
}

fn domain_hash(domain: &[u8], value: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update(value);
    hash.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_lower_hex(value: &str, expected_length: usize) -> bool {
    value.len() == expected_length
        && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(name: &str, paths: &SessionPaths) -> SessionRegistry {
        SessionRegistry {
            schema: REGISTRY_SCHEMA,
            name: name.to_owned(),
            pid: std::process::id(),
            instance_nonce: hex(&[7; 32]),
            vivido_version: env!("VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
            endpoint_id: paths.endpoint_id.clone(),
            process_birth: process_birth(std::process::id()).unwrap(),
            socket: paths.socket.clone(),
            headless: true,
            columns: 80,
            lines: 24,
        }
    }

    #[test]
    fn session_names_reject_traversal_and_oversize() {
        assert!(validate_session_name("build-42").is_ok());
        assert!(validate_session_name("a.b_c").is_ok());
        assert!(validate_session_name("").is_err());
        assert!(validate_session_name(".hidden").is_err());
        assert!(validate_session_name("../escape").is_err());
        assert!(validate_session_name("has/slash").is_err());
        assert!(validate_session_name("has space").is_err());
        assert!(validate_session_name(&"x".repeat(MAX_SESSION_NAME + 1)).is_err());
    }

    #[test]
    fn distinct_names_get_distinct_endpoints() {
        let root = std::env::temp_dir().join(format!("vivido-session-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let first = SessionPaths::for_session_in_root("one", &root).unwrap();
        let second = SessionPaths::for_session_in_root("two", &root).unwrap();
        assert_ne!(first.socket, second.socket);
        assert_ne!(first.endpoint_id, second.endpoint_id);

        // The endpoint id is derived from the socket path, so it is stable across calls.
        let again = SessionPaths::for_session_in_root("one", &root).unwrap();
        assert_eq!(first.endpoint_id, again.endpoint_id);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_registry_round_trips_and_matches_its_own_process() {
        let root = std::env::temp_dir().join(format!("vivido-session-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = SessionPaths::for_session_in_root("live", &root).unwrap();

        let written = paths.write_registry("live", &[3; 32], (100, 40), true).unwrap();
        assert_eq!(paths.read_registry().unwrap(), written);
        assert!(registry_process_matches(&written), "our own process must match its birth time");
        assert_eq!(list_registries_in_root(&root).unwrap(), vec![written.clone()]);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn registration_guard_publishes_and_removes_the_exact_instance() {
        let root =
            std::env::temp_dir().join(format!("vivido-session-guard-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = SessionPaths::for_session_in_root("guarded", &root).unwrap();

        let guard = paths.clone().register("guarded", (120, 50), false).unwrap();
        let registered = paths.read_registry().unwrap();
        assert_eq!((registered.columns, registered.lines), (120, 50));
        assert!(!registered.headless);
        assert!(registry_process_matches(&registered));

        drop(guard);
        assert!(!paths.registry.exists());
        #[cfg(unix)]
        assert!(!paths.socket.exists());
        let _ = fs::remove_dir_all(&root);
    }

    /// A registry naming a different instance must never have its files removed.
    ///
    /// This is the recycled-PID case: same PID, different daemon.
    #[test]
    fn teardown_refuses_to_remove_a_different_instance() {
        let root = std::env::temp_dir().join(format!("vivido-session-inst-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = SessionPaths::for_session_in_root("scoped", &root).unwrap();

        let live = paths.write_registry("scoped", &[1; 32], (80, 24), true).unwrap();
        #[cfg(unix)]
        fs::write(&paths.socket, b"").unwrap();

        // Same PID and name, but a different instance nonce: a replacement daemon.
        let mut stale = sample("scoped", &paths);
        stale.instance_nonce = hex(&[9; 32]);
        assert!(!paths.remove_instance(&stale).unwrap(), "a foreign instance must not be removed");
        assert!(paths.registry.exists(), "the live registry survived");
        #[cfg(unix)]
        assert!(paths.socket.exists(), "the live socket survived");

        // The true owner may remove its own artifacts.
        assert!(paths.remove_instance(&live).unwrap());
        assert!(!paths.registry.exists());
        #[cfg(unix)]
        assert!(!paths.socket.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_registry_from_a_dead_process_is_not_live() {
        let root = std::env::temp_dir().join(format!("vivido-session-dead-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let paths = SessionPaths::for_session_in_root("dead", &root).unwrap();

        let mut registry = sample("dead", &paths);
        // Same PID, but birth time from a process that no longer exists.
        registry.process_birth = match registry.process_birth {
            ProcessBirth::Linux { start_ticks } => {
                ProcessBirth::Linux { start_ticks: start_ticks.wrapping_add(1) }
            },
            ProcessBirth::Macos { start_micros } => {
                ProcessBirth::Macos { start_micros: start_micros.wrapping_add(1) }
            },
            ProcessBirth::Windows { creation_time } => {
                ProcessBirth::Windows { creation_time: creation_time.wrapping_add(1) }
            },
        };
        write_registry_file(&paths.registry, &registry).unwrap();

        assert!(!registry_process_matches(&registry));
        // Listing reaps it rather than reporting a session nobody owns.
        assert!(list_registries_in_root(&root).unwrap().is_empty());
        assert!(!paths.registry.exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn oversize_and_malformed_registries_are_refused() {
        assert!(decode_registry(b"not json").is_err());
        assert!(read_bounded_registry(&mut &b"x"[..], MAX_REGISTRY_BYTES + 1).is_err());

        let mut oversize = vec![b'x'; (MAX_REGISTRY_BYTES + 1) as usize];
        assert!(read_bounded_registry(&mut oversize.as_slice(), 0).is_err());
        oversize.clear();
    }

    #[test]
    #[cfg(unix)]
    fn group_or_world_readable_registries_are_refused() {
        assert!(safe_registry_metadata(true, 1000, 0o600, 1000));
        assert!(!safe_registry_metadata(true, 1000, 0o640, 1000), "group readable");
        assert!(!safe_registry_metadata(true, 1000, 0o604, 1000), "world readable");
        assert!(!safe_registry_metadata(true, 0, 0o600, 1000), "owned by another user");
        assert!(!safe_registry_metadata(false, 1000, 0o600, 1000), "not a regular file");
    }
}
