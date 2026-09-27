use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, RawHandle};
use std::path::Path;

use crate::cli::Options;
use crate::session::{SessionPaths, validate_session_name};

use super::{
    MAX_DIAGNOSTIC_BYTES, READINESS_TIMEOUT, Readiness, parse_readiness_report, scrub_environment,
    serve,
};

/// Re-enter the daemon after `spawn_detached` re-execs this executable.
pub fn run_reexec(
    mut options: Options,
    session: String,
    readiness_handle: usize,
) -> Result<(), Box<dyn Error>> {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation,
    };

    validate_session_name(&session)?;
    let paths = SessionPaths::for_session(&session)?;
    options.session = Some(session.clone());
    options.socket = Some(paths.socket.clone());
    let raw_readiness = readiness_handle as HANDLE;
    let mut handle_flags = 0;
    if raw_readiness.is_null()
        || unsafe { GetHandleInformation(raw_readiness, &mut handle_flags) } == 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "internal headless readiness handle is invalid",
        )
        .into());
    }
    // SAFETY: the parent explicitly made this pipe handle inheritable and transferred sole child
    // ownership by numeric value in the hidden option; GetHandleInformation validated it above.
    let readiness = unsafe { File::from_raw_handle(raw_readiness as RawHandle) };
    // The handle had to cross this re-exec, but the shell must never inherit it. Otherwise the
    // parent cannot observe EOF after readiness and waits for the shell to exit.
    if unsafe { SetHandleInformation(readiness.as_raw_handle() as HANDLE, HANDLE_FLAG_INHERIT, 0) }
        == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    scrub_environment();
    let readiness = Readiness::new(readiness);
    match serve(options, session, paths, Some(&readiness)) {
        Ok(()) => Ok(()),
        Err(error) => {
            readiness.failure(&error.to_string());
            Err(error)
        },
    }
}

pub(super) fn spawn_detached(
    _options: Options,
    session: String,
    _paths: SessionPaths,
) -> Result<(), Box<dyn Error>> {
    let (read, write) = readiness_pipe()
        .map_err(|error| io::Error::new(error.kind(), format!("readiness pipe: {error}")))?;
    let arguments = crate::cli::headless_reexec_args(
        std::env::args_os().skip(1).collect(),
        write.as_raw_handle() as usize,
        &session,
    );
    let child = spawn_isolated(&std::env::current_exe()?, &arguments, &write)
        .map_err(|error| io::Error::new(error.kind(), format!("headless re-exec: {error}")))?;
    drop(write);

    let paths = SessionPaths::for_session(&session)
        .map_err(|error| io::Error::new(error.kind(), format!("session paths: {error}")))?;
    await_readiness(read, child, &session, &paths)
}

/// Start the detached daemon so it inherits the readiness pipe and nothing else.
///
/// `std::process::Command` passes every inheritable handle in this process to the child, and the
/// launcher controls which those are: a Rust parent that captured our output with
/// `Command::output()` leaks its own inheritable handles here, including the write ends of pipes
/// it is reading. A daemon that inherited one would keep that reader waiting for EOF for as long
/// as the session lives, so `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` names the only handles it may
/// have. Standard handles are the NUL device, as a daemon has no console to report to.
fn spawn_isolated(program: &Path, arguments: &[OsString], readiness: &File) -> io::Result<u32> {
    use std::{mem, ptr};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
    };

    let null = inheritable_null()?;
    let inherited: [HANDLE; 2] =
        [readiness.as_raw_handle() as HANDLE, null.as_raw_handle() as HANDLE];

    let mut size = 0;
    // Sizing call: it fails by design and reports the size the one-attribute list needs.
    unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), 1, 0, &mut size) };
    // `usize` storage keeps the opaque list pointer-aligned.
    let mut storage = vec![0usize; size.div_ceil(mem::size_of::<usize>())];
    let attributes = storage.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
    if unsafe { InitializeProcThreadAttributeList(attributes, 1, 0, &mut size) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Deletes the list on every exit path; `storage` and `inherited` outlive it.
    struct AttributeList(LPPROC_THREAD_ATTRIBUTE_LIST);
    impl Drop for AttributeList {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }
    let _attributes = AttributeList(attributes);
    if unsafe {
        UpdateProcThreadAttribute(
            attributes,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            inherited.as_ptr().cast(),
            mem::size_of_val(&inherited),
            ptr::null_mut(),
            ptr::null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }

    let mut startup: STARTUPINFOEXW = unsafe { mem::zeroed() };
    startup.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited[1];
    startup.StartupInfo.hStdOutput = inherited[1];
    startup.StartupInfo.hStdError = inherited[1];
    startup.lpAttributeList = attributes;

    let application = wide_nul(program.as_os_str())?;
    let mut command_line = command_line(program.as_os_str(), arguments)?;
    let mut process: PROCESS_INFORMATION = unsafe { mem::zeroed() };
    // SAFETY: every pointer refers to a live, NUL-terminated buffer or initialized structure, and
    // the handle list names two open, inheritable handles owned by this function's caller.
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            1,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | EXTENDED_STARTUPINFO_PRESENT,
            ptr::null(),
            ptr::null(),
            &startup.StartupInfo,
            &mut process,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(process.dwProcessId)
}

/// Open the NUL device as an inheritable read/write handle.
fn inheritable_null() -> io::Result<File> {
    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};

    let null = std::fs::OpenOptions::new().read(true).write(true).open("NUL")?;
    if unsafe {
        SetHandleInformation(
            null.as_raw_handle() as HANDLE,
            HANDLE_FLAG_INHERIT,
            HANDLE_FLAG_INHERIT,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(null)
}

fn wide_nul(value: &OsStr) -> io::Result<Vec<u16>> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    if wide.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL"));
    }
    wide.push(0);
    Ok(wide)
}

/// Build a NUL-terminated command line that `CommandLineToArgvW` and the C runtime split back
/// into exactly `program` followed by `arguments`, quoting as the standard library does.
fn command_line(program: &OsStr, arguments: &[OsString]) -> io::Result<Vec<u16>> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;

    // The program name is split on quotes alone, with no backslash escapes, so it cannot hold one.
    let mut line = vec![QUOTE];
    for unit in program.encode_wide() {
        if unit == QUOTE || unit == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid program path"));
        }
        line.push(unit);
    }
    line.push(QUOTE);

    for argument in arguments {
        line.push(b' ' as u16);
        let units: Vec<u16> = argument.encode_wide().collect();
        if units.contains(&0) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "argument contains NUL"));
        }
        let quote = units.is_empty() || units.iter().any(|&unit| unit == 0x20 || unit == 0x09);
        if quote {
            line.push(QUOTE);
        }
        let mut backslashes = 0;
        for &unit in &units {
            if unit == BACKSLASH {
                backslashes += 1;
            } else {
                if unit == QUOTE {
                    // 2n+1 backslashes in total before an embedded quote.
                    line.extend(std::iter::repeat_n(BACKSLASH, backslashes + 1));
                }
                backslashes = 0;
            }
            line.push(unit);
        }
        if quote {
            // 2n backslashes in total before the closing quote.
            line.extend(std::iter::repeat_n(BACKSLASH, backslashes));
            line.push(QUOTE);
        }
    }
    line.push(0);
    Ok(line)
}

pub(super) fn readiness_pipe() -> io::Result<(File, File)> {
    use std::ptr;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read = ptr::null_mut();
    let mut write = ptr::null_mut();
    if unsafe { CreatePipe(&mut read, &mut write, &attributes, 8 * 1024) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // Only the write end crosses re-exec. If the child inherited the read end, the readiness
    // protocol could not reliably observe writer closure after an early startup failure.
    if unsafe { SetHandleInformation(read, HANDLE_FLAG_INHERIT, 0) } == 0 {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(read);
            windows_sys::Win32::Foundation::CloseHandle(write);
        }
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreatePipe returned two fresh handles and ownership transfers to File.
    Ok(unsafe {
        (File::from_raw_handle(read as RawHandle), File::from_raw_handle(write as RawHandle))
    })
}

fn await_readiness(
    mut read: File,
    child: u32,
    session: &str,
    paths: &SessionPaths,
) -> Result<(), Box<dyn Error>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut report = String::new();
        let result = Read::by_ref(&mut read)
            .take(MAX_DIAGNOSTIC_BYTES + 64)
            .read_to_string(&mut report)
            .map(|_| report);
        let _ = sender.send(result);
    });

    let report = match receiver.recv_timeout(READINESS_TIMEOUT) {
        Ok(result) => result?,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "vivido session {session:?} did not report readiness within {}s; it is still \
                     running as pid {child}. Check it with `vivido list`.",
                    READINESS_TIMEOUT.as_secs()
                ),
            )
            .into());
        },
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            return Err(io::Error::other("readiness reader stopped unexpectedly").into());
        },
    };
    parse_readiness_report(&report, session, paths)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Split `line` the way the daemon's C runtime will.
    fn split(line: &[u16]) -> Vec<OsString> {
        use std::os::windows::ffi::OsStringExt;
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

        let mut count = 0;
        let argv = unsafe { CommandLineToArgvW(line.as_ptr(), &mut count) };
        assert!(!argv.is_null(), "{}", io::Error::last_os_error());
        let split = (0..count as usize)
            .map(|index| unsafe {
                let argument = *argv.add(index);
                let length = (0..).take_while(|&offset| *argument.add(offset) != 0).count();
                OsString::from_wide(std::slice::from_raw_parts(argument, length))
            })
            .collect();
        unsafe { LocalFree(argv.cast()) };
        split
    }

    #[test]
    fn command_line_round_trips_through_argv_splitting() {
        let program = OsStr::new(r"C:\Program Files\Vivido\vivido.exe");
        let arguments: Vec<OsString> = [
            "--headless",
            "",
            "two words",
            r#"quote"inside"#,
            r"trailing\",
            r"trailing space\ ",
            r#"back\"slash"#,
            "tab\there",
            r"\server\share\",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();

        let line = command_line(program, &arguments).unwrap();

        let mut expected = vec![program.to_owned()];
        expected.extend(arguments);
        assert_eq!(split(&line), expected);
    }

    #[test]
    fn command_line_rejects_what_cannot_round_trip() {
        assert!(command_line(OsStr::new(r#"C:\a"b.exe"#), &[]).is_err());
        assert!(command_line(OsStr::new("a.exe"), &[OsString::from("nul\0byte")]).is_err());
    }

    /// Reports when `pipe` reaches EOF, i.e. when every copy of its write end is closed.
    fn eof_watch(mut pipe: File) -> mpsc::Receiver<()> {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let _ = pipe.read_to_end(&mut sink);
            let _ = sender.send(());
        });
        receiver
    }

    /// A launcher that captured `vivido --headless` output leaks the pipe it reads into this
    /// process as an inheritable handle. The daemon must not hold it, or the launcher never sees
    /// EOF while the session lives; it must still hold the readiness pipe.
    #[test]
    fn the_daemon_inherits_only_the_readiness_pipe() {
        let (readiness_read, readiness_write) = readiness_pipe().unwrap();
        let (leaked_read, leaked_write) = readiness_pipe().unwrap();
        // Outlives the assertions below, then exits on its own.
        let program =
            PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join(r"System32\PING.EXE");
        let arguments = ["-n", "6", "127.0.0.1"].map(OsString::from);
        let _pid = spawn_isolated(&program, &arguments, &readiness_write).unwrap();
        drop(readiness_write);
        drop(leaked_write);

        let leaked = eof_watch(leaked_read);
        let readiness = eof_watch(readiness_read);
        leaked
            .recv_timeout(Duration::from_secs(2))
            .expect("the child inherited a handle outside its list");
        assert!(
            readiness.recv_timeout(Duration::from_millis(500)).is_err(),
            "the child did not inherit the readiness pipe"
        );
        readiness
            .recv_timeout(Duration::from_secs(30))
            .expect("the readiness pipe closed when the child exited");
    }
}
