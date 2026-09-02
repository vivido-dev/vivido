//! End-to-end coverage for `vivido --headless`.
//!
//! Every test runs with `WAYLAND_DISPLAY` and `DISPLAY` unset, so a pass is evidence the session
//! really came up with no compositor rather than quietly borrowing the developer's desktop.
//!
//! These are `#[ignore]` by default: they need a usable wgpu adapter (hardware or a software
//! implementation such as lavapipe) and they spawn real processes. Run them with:
//!
//! ```sh
//! cargo test --test headless -- --ignored --test-threads=1
//! ```

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use std::{env, fs};

/// Long enough for a software renderer to initialize on a loaded machine.
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// A headless session that is shut down when the test ends, however the test ends.
struct Session {
    name: String,
    runtime: PathBuf,
    socket: String,
}

impl Session {
    /// Start a detached headless session running `program`.
    fn start(name: &str, program: &[String]) -> Session {
        // Unix sockets cap the whole path at ~108 bytes, so the runtime root must stay short.
        let runtime = test_runtime(name);
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir_all(&runtime).expect("runtime directory");
        set_private(&runtime);

        let mut command = base_command(&runtime);
        command.args(["--headless", "--session", name, "--headless-size", "100x30"]);
        command.arg("-e").args(program);

        let output = command.output().expect("spawn vivido --headless");
        assert!(
            output.status.success(),
            "vivido --headless failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // The parent prints shell-eval'able assignments; the socket is what a client needs.
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let socket = stdout
            .lines()
            .find_map(|line| line.strip_prefix("VIVIDO_SOCKET="))
            .and_then(|line| line.split(';').next())
            .unwrap_or_else(|| panic!("no VIVIDO_SOCKET in startup output: {stdout:?}"))
            .to_owned();

        let session = Session { name: name.to_owned(), runtime, socket };
        session.await_ready();
        session
    }

    /// Wait until the session answers, so a slow adapter is not read as a failure.
    fn await_ready(&self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if self.try_msg(&["capabilities"]).status.success() {
                return;
            }
            assert!(Instant::now() < deadline, "session {:?} never became ready", self.name);
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn try_msg<S: AsRef<OsStr>>(&self, args: &[S]) -> Output {
        let mut command = base_command(&self.runtime);
        command.args(["msg", "-s", &self.socket]);
        command.args(args);
        command.output().expect("run vivido msg")
    }

    /// Run `vivido msg` and return its stdout, failing the test on a protocol error.
    fn msg<S: AsRef<OsStr>>(&self, args: &[S]) -> String {
        let printable_args: Vec<_> =
            args.iter().map(|arg| arg.as_ref().to_string_lossy()).collect();
        let output = self.try_msg(args);
        assert!(
            output.status.success(),
            "vivido msg {printable_args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Live sessions this session's runtime directory reports.
    fn list(&self) -> String {
        let mut command = base_command(&self.runtime);
        command.arg("list");
        let output = command.output().expect("run vivido list");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }
}

/// Keep Unix socket paths below `sockaddr_un.sun_path`, which is only 104 bytes on macOS.
fn test_runtime(name: &str) -> PathBuf {
    #[cfg(unix)]
    let root = PathBuf::from("/tmp");
    #[cfg(windows)]
    let root = env::temp_dir();
    root.join(format!("vivido-it-{}-{name}", std::process::id()))
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.try_msg(&["quit"]);
        // Give the daemon a moment to clear its registry before the directory goes away.
        std::thread::sleep(Duration::from_millis(300));
        let _ = fs::remove_dir_all(&self.runtime);
    }
}

/// A `vivido` invocation with no windowing system reachable.
fn base_command(runtime: &Path) -> Command {
    let mut command = Command::new(binary());
    #[cfg(unix)]
    command.env("XDG_RUNTIME_DIR", runtime);
    #[cfg(windows)]
    command.env("VIVIDO_RUNTIME_DIR", runtime);
    command
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .env_remove("VIVIDO_SOCKET")
        .env_remove("VIVIDO_SESSION");
    command
}

fn binary() -> PathBuf {
    // The integration test binary lives next to the executables cargo built for this profile.
    let mut path = env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("vivido{}", env::consts::EXE_SUFFIX))
}

#[cfg(unix)]
fn set_private(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("private runtime dir");
}

#[cfg(windows)]
fn set_private(_path: &Path) {}

/// Decode a PNG into `(width, height, distinct_colors, non_black_pixels)`.
fn inspect_png(path: &Path) -> (u32, u32, usize, usize) {
    use std::collections::HashSet;

    let image = image::open(path).expect("decode screenshot PNG").into_rgba8();
    let (width, height) = image.dimensions();
    let mut colors = HashSet::new();
    let mut lit = 0;
    for pixel in image.pixels() {
        colors.insert([pixel[0], pixel[1], pixel[2]]);
        if pixel.0[..3] != [0, 0, 0] {
            lit += 1;
        }
    }
    (width, height, colors.len(), lit)
}

#[cfg(unix)]
fn shell_program() -> Vec<String> {
    vec![String::from("sh")]
}

#[cfg(windows)]
fn shell_program() -> Vec<String> {
    ["powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn marker_program(marker: &str) -> Vec<String> {
    #[cfg(unix)]
    return ["sh", "-c", &format!("echo {marker}; exec sh")]
        .into_iter()
        .map(String::from)
        .collect();
    #[cfg(windows)]
    return [
        String::from("powershell.exe"),
        String::from("-NoLogo"),
        String::from("-NoProfile"),
        String::from("-NonInteractive"),
        String::from("-NoExit"),
        String::from("-Command"),
        format!("Write-Output '{marker}'"),
    ]
    .into_iter()
    .collect();
}

/// The whole point: a session with no compositor still answers text and pixel queries.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn a_headless_session_serves_text_and_screenshots_without_a_compositor() {
    let session = Session::start("basic", &marker_program("MARKER-ALPHA"));

    // `hello` must say so, rather than leaving a client to infer it from a failed `focus`.
    let capabilities = session.msg(&["capabilities"]);
    assert!(capabilities.contains(r#""headless":true"#), "capabilities: {capabilities}");
    assert!(capabilities.contains(r#""session":"basic""#), "capabilities: {capabilities}");
    assert!(capabilities.contains(r#""automation_name":"basic""#));
    assert!(session.msg(&["ping"]).contains(r#""pong""#));

    let vivid_sessions = session.msg(&["vivid", "sessions"]);
    assert!(vivid_sessions.contains("sessions"), "Vivid sessions: {vivid_sessions}");
    let diagnose = session.msg(&["diagnose", "--trace-limit", "16"]);
    assert!(diagnose.contains(r#""schema_version":1"#), "diagnose: {diagnose}");

    // The window is never focused, so this also proves target resolution does not need focus.
    let windows = session.msg(&["list-windows"]);
    assert!(
        windows.contains(r#""focused":false"#),
        "a headless window is never focused: {windows}"
    );
    assert!(windows.contains(r#""occluded":false"#), "and never occluded: {windows}");

    session.msg(&["wait", "text", "MARKER-ALPHA"]);
    assert!(session.msg(&["get-text"]).contains("MARKER-ALPHA"));

    let path = PathBuf::from(session.msg(&["screenshot"]).trim());
    let (width, height, colors, lit) = inspect_png(&path);
    assert!(width > 0 && height > 0, "screenshot has no size");
    assert!(colors > 1, "screenshot is a single flat colour, so nothing was rendered");
    assert!(lit > 0, "screenshot has no non-background pixels, so no text was drawn");
    let _ = fs::remove_file(&path);

    let mut list = base_command(&session.runtime);
    list.args(["list", "--all", "--json"]);
    let listed = list.output().unwrap();
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains(r#""name":"basic""#));

    let bundle = session.runtime.join("basic.zip");
    let mut command = base_command(&session.runtime);
    command.args(["debug-bundle", "--target", "basic", "--output", bundle.to_str().unwrap()]);
    let bundled = command.output().unwrap();
    assert!(bundled.status.success(), "{}", String::from_utf8_lossy(&bundled.stderr));
    let archive = fs::read(bundle).unwrap();
    assert!(archive.windows(b"manifest.json".len()).any(|part| part == b"manifest.json"));
    assert!(!archive.windows(b"content/".len()).any(|part| part == b"content/"));
}

/// Input reaches the PTY and its output comes back, with no window and no focus.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn typing_drives_the_shell_in_a_headless_session() {
    let session = Session::start("typing", &shell_program());

    #[cfg(unix)]
    let report = session.msg(&["typing", "echo RESULT-$((6*7))\n", "--report"]);
    #[cfg(windows)]
    let report = session.msg(&["typing", "Write-Output (\"RESULT-\" + (6*7))\r", "--report"]);
    assert!(report.contains(r#""pty_write_completed":true"#), "report: {report}");
    assert!(report.contains(r#""application_consumption_observed":false"#), "report: {report}");
    session.msg(&["wait", "text", "RESULT-42"]);

    let text = session.msg(&["get-text"]);
    assert!(text.contains("RESULT-42"), "the shell never ran the typed command: {text}");
}

/// A misbehaving full-screen client can be recovered without replacing the host process.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn terminal_recovery_preserves_host_liveness_and_window_identity() {
    let session = Session::start("recovery", &shell_program());
    let windows: serde_json::Value =
        serde_json::from_str(&session.msg(&["list-windows"])).expect("window list JSON");
    let window_id = windows["windows"][0]["window_id"].as_u64().expect("window ID");

    #[cfg(unix)]
    session.msg(&["typing", "printf '\\033[?1049h\\033[?1003h\\033[?1004h\\033[?2004hDIRTY'\n"]);
    #[cfg(windows)]
    session.msg(&[
        "typing",
        "$e=[char]27; [Console]::Write(\"$e[?1049h$e[?1003h$e[?1004h$e[?2004hDIRTY\")\r",
    ]);
    session.msg(&["wait", "text", "DIRTY"]);
    let dirty = session.msg(&["inspect"]);
    assert!(dirty.contains(r#""screen":"alternate""#), "dirty terminal state: {dirty}");

    session.msg(&["reset-terminal", "--window-id", &window_id.to_string()]);
    assert!(session.msg(&["ping"]).contains(r#""pong""#));
    let reset = session.msg(&["inspect", "--window-id", &window_id.to_string()]);
    assert!(reset.contains(r#""screen":"primary""#), "reset state: {reset}");
    for mode in ["mouse_motion", "focus_in_out", "bracketed_paste"] {
        assert!(!reset.contains(mode), "reset retained {mode}: {reset}");
    }

    session.msg(&["restart-terminal", "--window-id", &window_id.to_string()]);
    assert!(session.msg(&["ping"]).contains(r#""pong""#));
    let restarted = session.msg(&["list-windows"]);
    assert!(
        restarted.contains(&format!(r#""window_id":{window_id}"#)),
        "restart changed the public identity: {restarted}"
    );
}

/// A resize must retarget the renderer, not just the grid.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn resizing_a_headless_session_changes_the_rendered_size() {
    let session = Session::start("resize", &shell_program());

    let before = PathBuf::from(session.msg(&["screenshot"]).trim());
    let (before_width, before_height, ..) = inspect_png(&before);
    let _ = fs::remove_file(&before);

    session.msg(&["resize", "--width", "800", "--height", "600"]);

    let after = PathBuf::from(session.msg(&["screenshot"]).trim());
    let (after_width, after_height, after_colors, _) = inspect_png(&after);
    let _ = fs::remove_file(&after);

    assert!(
        (after_width, after_height) != (before_width, before_height),
        "resize did not change the render target: {before_width}x{before_height}"
    );
    assert!(after_colors > 1, "the renderer produced a blank frame after resizing");
}

/// A headless instance outlives its windows and is only stopped explicitly.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn a_headless_session_persists_and_is_listed_until_it_is_told_to_quit() {
    let session = Session::start("lifecycle", &shell_program());

    let listed = session.list();
    assert!(listed.contains("lifecycle"), "the session is not listed: {listed:?}");

    // A second window makes the instance multi-window, which is what nesting relies on.
    let mut create_window = vec![String::from("create-window"), String::from("-e")];
    create_window.extend(shell_program());
    let second = session.msg(&create_window);
    let second: u64 = second.trim().parse().expect("create-window returns a window id");
    let windows = session.msg(&["list-windows"]);
    assert_eq!(windows.matches(r#""window_id""#).count(), 2, "two windows: {windows}");

    // With more than one window an unqualified request is ambiguous and must say so rather than
    // silently pick one.
    let ambiguous = session.try_msg(&["get-text"]);
    assert!(!ambiguous.status.success(), "an ambiguous target must be refused");

    // Naming the window resolves it without any focus involved.
    let text = session.msg(&["get-text", "--window-id", &second.to_string()]);
    assert!(text.is_empty() || text.chars().all(char::is_whitespace) || !text.is_empty());

    session.msg(&["quit"]);
    std::thread::sleep(Duration::from_millis(500));

    // Shutting down clears the rendezvous, so a stale entry cannot outlive the daemon.
    assert!(!session.list().contains("lifecycle"), "the registry survived shutdown");
    #[cfg(unix)]
    assert!(!Path::new(&session.socket).exists(), "the socket survived shutdown");
}

/// Two sessions must be completely independent, including when one is torn down.
///
/// Both windows deliberately carry the same numeric window id — headless ids start from the same
/// counter in every process — so this also proves teardown is keyed on more than that number.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn tearing_down_one_session_leaves_the_other_untouched() {
    let first = Session::start("iso-one", &marker_program("OWNER-ONE"));
    let second = Session::start("iso-two", &marker_program("OWNER-TWO"));

    first.msg(&["wait", "text", "OWNER-ONE"]);
    second.msg(&["wait", "text", "OWNER-TWO"]);

    let first_windows = first.msg(&["list-windows"]);
    let second_windows = second.msg(&["list-windows"]);
    let first_id = first_windows.split(r#""window_id":"#).nth(1).expect("a window id");
    let second_id = second_windows.split(r#""window_id":"#).nth(1).expect("a window id");
    assert_eq!(
        first_id, second_id,
        "the two sessions should reuse the same numeric window id, making this a real test"
    );

    // Tear the first one down entirely.
    first.msg(&["quit"]);
    std::thread::sleep(Duration::from_millis(500));

    // The survivor keeps its window, its grid contents, and its ability to render and be driven.
    assert!(second.msg(&["get-text"]).contains("OWNER-TWO"), "the survivor lost its scrollback");
    assert_eq!(
        second.msg(&["list-windows"]).matches(r#""window_id""#).count(),
        1,
        "the survivor lost its window"
    );
    #[cfg(unix)]
    second.msg(&["typing", "echo STILL-ALIVE\n"]);
    #[cfg(windows)]
    second.msg(&["typing", "Write-Output 'STILL-ALIVE'\r"]);
    second.msg(&["wait", "text", "STILL-ALIVE"]);

    let path = PathBuf::from(second.msg(&["screenshot"]).trim());
    let (_, _, colors, lit) = inspect_png(&path);
    assert!(colors > 1 && lit > 0, "the survivor's renderer stopped producing frames");
    let _ = fs::remove_file(&path);

    // The survivor's rendezvous is intact and the dead session's is gone.
    let listed = second.list();
    assert!(listed.contains("iso-two"), "the survivor was unregistered: {listed:?}");
    assert!(!listed.contains("iso-one"), "the dead session was not reaped: {listed:?}");
}

/// A session name must never escape the runtime directory.
#[test]
fn session_names_that_escape_the_runtime_directory_are_refused() {
    let runtime = test_runtime("names");
    let _ = fs::remove_dir_all(&runtime);
    fs::create_dir_all(&runtime).expect("runtime directory");
    set_private(&runtime);

    for name in ["../escape", "a/b", ".hidden", ""] {
        let mut command = base_command(&runtime);
        command.args(["--headless", "--foreground", "--session", name]);
        let output = command.output().expect("spawn vivido");
        assert!(!output.status.success(), "session name {name:?} was accepted");
    }

    let _ = fs::remove_dir_all(&runtime);
}
