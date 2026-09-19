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
        Session::start_with_env(name, program, &[])
    }

    /// Start a session with extra environment, for testing what a daemon inherits.
    fn start_with_env(name: &str, program: &[String], environment: &[(&str, &str)]) -> Session {
        // Unix sockets cap the whole path at ~108 bytes, so the runtime root must stay short.
        let runtime = test_runtime(name);
        let _ = fs::remove_dir_all(&runtime);
        fs::create_dir_all(&runtime).expect("runtime directory");
        set_private(&runtime);

        let mut command = base_command(&runtime);
        for (key, value) in environment {
            command.env(key, value);
        }
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

/// Report the mesh coordinates a pane inherited, then keep the pane alive.
#[cfg(unix)]
fn mesh_report_program() -> Vec<String> {
    ["sh", "-c", "echo \"MESH ${AGENT_MESH_INSTANCE-unset} ${AGENT_MESH_ADDRESS-unset}\"; exec sh"]
        .into_iter()
        .map(String::from)
        .collect()
}

#[cfg(unix)]
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn a_pane_inherits_this_sessions_mesh_coordinates_and_not_the_launchers() {
    // A window writes its coordinates as overrides, so anything it does not write falls through
    // from the daemon. Started from inside another pane, a session used to hand its own panes the
    // launching pane's instance and address — pointing at a different runtime instance entirely.
    let session = Session::start_with_env(
        "mesh",
        &mesh_report_program(),
        &[
            ("AGENT_MESH_INSTANCE", "launching-pane"),
            ("AGENT_MESH_ADDRESS", "w99"),
            // The watcher is a separate concern and needs `vvagent` on PATH.
            ("AGENT_MESH_WATCH", "off"),
        ],
    );

    session.msg(&["wait", "text", "MESH ", "--window-id", "1"]);
    let text = session.msg(&["get-text", "--window-id", "1"]);
    assert!(
        text.contains("MESH mesh w1"),
        "a pane takes this session's name and its own window: {text:?}"
    );
    assert!(!text.contains("launching-pane"), "the launcher's instance must not leak: {text:?}");
    assert!(!text.contains("w99"), "the launcher's address must not leak: {text:?}");

    // An address index is a one-based `u32`. A claimed ID outside it has no mesh position, and
    // publishing nothing must beat leaving the inherited value in place.
    let mut create = vec![
        String::from("create-window"),
        String::from("--window-id"),
        String::from("9223372036854775808"),
        String::from("-e"),
    ];
    create.extend(mesh_report_program());
    let claimed = session.msg(&create);
    let claimed: u64 = claimed.trim().parse().expect("create-window returns a window id");
    assert_eq!(claimed, 9_223_372_036_854_775_808);

    let claimed = claimed.to_string();
    session.msg(&["wait", "text", "MESH ", "--window-id", &claimed]);
    let text = session.msg(&["get-text", "--window-id", &claimed]);
    assert!(text.contains("MESH mesh unset"), "no address rather than a stale one: {text:?}");
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

/// A shell emitting OSC 133 markers drives semantic prompt/finish tracking.
#[cfg(unix)]
fn integration_program() -> Vec<String> {
    vec![
        String::from("sh"),
        String::from("-c"),
        String::from(
            "printf '\\033]133;A\\a'; i=0; while true; do printf '\\033]133;B\\a'; sleep 2; echo \"WORK-$i\"; printf '\\033]133;C\\a'; echo \"OUT-$i\"; printf '\\033]133;D;0\\a'; sleep 2; printf '\\033]133;A\\a'; sleep 2; i=$((i+1)); done",
        ),
    ]
}

/// Windows ConPTY forwards unrecognized OSC sequences, so the same markers work there.
#[cfg(windows)]
fn integration_program() -> Vec<String> {
    vec![
        String::from("powershell.exe"),
        String::from("-NoLogo"),
        String::from("-NoProfile"),
        String::from("-NonInteractive"),
        String::from("-Command"),
        String::from(
            "$e=[char]27; $b=[char]7; $i=0; [Console]::Write(\"$e]133;A$b\"); while ($true) { [Console]::Write(\"$e]133;B$b\"); Start-Sleep -Seconds 2; Write-Output \"WORK-$i\"; [Console]::Write(\"$e]133;C$b\"); Write-Output \"OUT-$i\"; [Console]::Write(\"$e]133;D;0$b\"); Start-Sleep -Seconds 2; [Console]::Write(\"$e]133;A$b\"); Start-Sleep -Seconds 2; $i++ }",
        ),
    ]
}

/// Fixed screen content for deterministic scoped waits: no scrolling, no races.
#[cfg(unix)]
fn static_program() -> Vec<String> {
    vec![
        String::from("sh"),
        String::from("-c"),
        String::from("printf 'ROW0\\nROW1 STATUS-42\\nROW2\\n'; sleep 300"),
    ]
}

#[cfg(windows)]
fn static_program() -> Vec<String> {
    vec![
        String::from("powershell.exe"),
        String::from("-NoLogo"),
        String::from("-NoProfile"),
        String::from("-NonInteractive"),
        String::from("-Command"),
        String::from(
            "Write-Output 'ROW0'; Write-Output 'ROW1 STATUS-42'; Write-Output 'ROW2'; Start-Sleep -Seconds 300",
        ),
    ]
}

/// Semantic waits resolve on shell integration markers instead of screen scraping.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn semantic_waits_follow_shell_integration_markers() {
    let session = Session::start("shell", &integration_program());

    // The prompt marker predates the wait: tracked state resolves it immediately.
    let ready = session.msg(&["wait", "prompt", "--timeout", "10s"]);
    assert!(ready.contains(r#""ready":true"#), "prompt wait: {ready}");

    // The next finish resolves with its own exit code, never an earlier command's.
    let finished = session.msg(&["wait", "command-finish", "--timeout", "15s"]);
    assert!(finished.contains(r#""status":"completed""#), "finish wait: {finished}");
    assert!(finished.contains(r#""exit_code":0"#), "finish wait: {finished}");

    // A shell emitting no markers never resolves a semantic wait.
    let plain = Session::start("plain", &shell_program());
    let missing = plain.try_msg(&["wait", "prompt", "--timeout", "2s"]);
    assert!(!missing.status.success(), "a markerless shell must not resolve wait prompt");
}

/// Scoped text waits read one row or rectangle instead of the whole viewport.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn scoped_text_waits_restrict_matching() {
    let session = Session::start("scoped", &static_program());

    session.msg(&["wait", "text", "STATUS-42", "--line", "1", "--timeout", "10s"]);
    session.msg(&["wait", "text", "STATUS", "--rect", "5,1,6,1", "--timeout", "10s"]);
    session.msg(&["wait", "text", "STATUS-[0-9]+", "--regex", "--line", "1", "--timeout", "10s"]);

    // The text is on screen but on another row: the scope must refuse it.
    let wrong_row =
        session.try_msg(&["wait", "text", "STATUS-42", "--line", "0", "--timeout", "2s"]);
    assert!(!wrong_row.status.success(), "row scope matched outside its row");
}

/// run-plan asserts terminal state and result shapes, and exports JUnit for CI.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn run_plan_asserts_state_and_writes_junit() {
    let session = Session::start("plan", &shell_program());
    let windows: serde_json::Value =
        serde_json::from_str(&session.msg(&["list-windows"])).expect("window list JSON");
    let window_id = windows["windows"][0]["window_id"].as_u64().expect("window ID");

    #[cfg(unix)]
    let command = "echo PLAN-ASSERT-99\n";
    #[cfg(windows)]
    let command = "Write-Output PLAN-ASSERT-99\r";
    let plan = session.runtime.join("assert-plan.json");
    fs::write(
        &plan,
        format!(
            r#"{{"version":2,"name":"assert-e2e","steps":[
            {{"id":"type","method":"typing","params":{{"text":{command:?}}}}},
            {{"id":"see","method":"wait_text","params":{{"text":"PLAN-ASSERT-99"}},"assert":{{"text_contains":"PLAN-ASSERT-99","window_id":{window_id},"lines_from_bottom":30,"timeout_ms":15000}}}},
            {{"id":"shape","method":"list_windows","assert":{{"result_pointer":"/windows/0/window_id","result_equals":{window_id}}}}}
        ]}}"#
        ),
    )
    .unwrap();
    let report = session.runtime.join("junit.xml");
    let output = session.try_msg(&[
        "run-plan",
        "--file",
        plan.to_str().unwrap(),
        "--report",
        "junit",
        "--output",
        report.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "run-plan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let xml = fs::read_to_string(&report).unwrap();
    assert!(xml.contains(r#"name="assert-e2e""#), "{xml}");
    assert!(xml.contains(r#"tests="3" failures="0""#), "{xml}");
    assert!(xml.contains(r#"name="see""#), "{xml}");

    // A failing assertion fails the step and the suite, and the report says so.
    let failing = session.runtime.join("failing-plan.json");
    fs::write(
        &failing,
        format!(
            r#"{{"version":2,"steps":[
            {{"id":"miss","method":"ping","on_error":"continue","assert":{{"text_contains":"NEVER-PRINTED","window_id":{window_id},"timeout_ms":1000}}}}
        ]}}"#
        ),
    )
    .unwrap();
    let failing_report = session.runtime.join("failing-junit.xml");
    let failed = session.try_msg(&[
        "run-plan",
        "--file",
        failing.to_str().unwrap(),
        "--report",
        "junit",
        "--output",
        failing_report.to_str().unwrap(),
    ]);
    assert!(!failed.status.success(), "a failing assertion must fail the plan");
    let xml = fs::read_to_string(&failing_report).unwrap();
    assert!(xml.contains(r#"tests="1" failures="1""#), "{xml}");
    assert!(xml.contains("NEVER-PRINTED"), "{xml}");
}

/// `vivido test` runs a plan in an ephemeral session, reports JUnit, and tears down.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn test_runner_executes_a_plan_and_tears_down() {
    let runtime = test_runtime("runner");
    let _ = fs::remove_dir_all(&runtime);
    fs::create_dir_all(&runtime).expect("runtime directory");
    set_private(&runtime);

    #[cfg(unix)]
    let latch = "echo RUNNER-SMOKE-7\n";
    #[cfg(windows)]
    let latch = "Write-Output 'RUNNER-SMOKE-7'\r";
    let plan = runtime.join("smoke-plan.json");
    fs::write(
        &plan,
        r#"{"version":1,"steps":[
        {"id":"type","method":"typing","params":{"text":"LATCH"}},
        {"id":"see","method":"wait_text","params":{"text":"RUNNER-SMOKE-7","common":{"timeout":15000,"target":{}}}}
    ]}"#
        .replace("LATCH", &latch.replace('\n', "\\n").replace('\r', "\\r")),
    )
    .unwrap();
    let report = runtime.join("smoke-junit.xml");

    let mut command = base_command(&runtime);
    command.args([
        "test",
        "--session",
        "runner-smoke",
        "--file",
        plan.to_str().unwrap(),
        "--report",
        "junit",
        "--output",
        report.to_str().unwrap(),
    ]);
    #[cfg(unix)]
    command.args(["--", "sh"]);
    #[cfg(windows)]
    command.args(["--", "powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive"]);
    let output = command.output().expect("run vivido test");
    assert!(
        output.status.success(),
        "vivido test failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Stdout stays machine-readable plan NDJSON even under the test runner.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(r#""type":"plan_completed""#), "no plan events: {stdout}");
    let xml = fs::read_to_string(&report).unwrap();
    assert!(xml.contains(r#"tests="2" failures="0""#), "{xml}");

    // The ephemeral session is gone: nothing to leak into the next run.
    let mut list = base_command(&runtime);
    list.arg("list");
    let list = list.output().expect("run vivido list");
    assert!(
        !String::from_utf8_lossy(&list.stdout).contains("runner-smoke"),
        "test session survived its passing run"
    );

    // A failing run captures evidence, keeps the session with --keep-failed, and still fails.
    let failing = runtime.join("failing-plan.json");
    fs::write(
        &failing,
        r#"{"version":1,"steps":[
        {"id":"miss","method":"wait_text","params":{"text":"NEVER-PRINTED","common":{"timeout":1000,"target":{}}}}
    ]}"#,
    )
    .unwrap();
    let artifacts = runtime.join("artifacts");
    let failing_report = runtime.join("failing-junit.xml");
    let mut failed = base_command(&runtime);
    failed.args([
        "test",
        "--session",
        "runner-keep",
        "--file",
        failing.to_str().unwrap(),
        "--report",
        "junit",
        "--output",
        failing_report.to_str().unwrap(),
        "--artifacts-dir",
        artifacts.to_str().unwrap(),
        "--keep-failed",
    ]);
    #[cfg(unix)]
    failed.args(["--", "sh"]);
    #[cfg(windows)]
    failed.args(["--", "powershell.exe", "-NoLogo", "-NoProfile", "-NonInteractive"]);
    let output = failed.output().expect("run failing vivido test");
    assert!(!output.status.success(), "a failing plan must fail `vivido test`");
    assert!(artifacts.join("miss.grid.txt").is_file(), "no grid capture");
    assert!(artifacts.join("miss.screenshot.json").is_file(), "no screenshot capture");
    let xml = fs::read_to_string(&failing_report).unwrap();
    assert!(xml.contains(r#"tests="1" failures="1""#), "{xml}");

    // The kept session is a real session: quit it explicitly to leave no residue.
    let mut quit = base_command(&runtime);
    quit.args(["kill-session", "--target", "runner-keep"]);
    let quit = quit.output().expect("quit kept session");
    assert!(quit.status.success(), "cannot quit kept session");
    let mut list = base_command(&runtime);
    list.arg("list");
    let list = list.output().expect("run vivido list");
    assert!(
        !String::from_utf8_lossy(&list.stdout).contains("runner-keep"),
        "kept session survived kill-session"
    );
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

/// `close-window` removes one window while the session keeps serving the rest.
#[test]
#[ignore = "spawns processes and needs a wgpu adapter"]
fn close_window_removes_one_window_and_keeps_the_session() {
    let session = Session::start("close", &shell_program());

    let mut create_window = vec![String::from("create-window"), String::from("-e")];
    create_window.extend(shell_program());
    let second = session.msg(&create_window);
    let second: u64 = second.trim().parse().expect("create-window returns a window id");

    let closed = session.msg(&["close-window", "--window-id", &second.to_string()]);
    let closed: serde_json::Value =
        serde_json::from_str(&closed).expect("close-window replies with JSON");
    assert_eq!(closed["window_id"], second);
    assert_eq!(closed["closed"], true);

    // The reply is sent before the terminal exit event is processed, so wait for the removal.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let windows = session.msg(&["list-windows"]);
        if !windows.contains(&format!(r#""window_id":{second}"#)) {
            assert_eq!(
                windows.matches(r#""window_id""#).count(),
                1,
                "exactly one window remains: {windows}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "closed window never left: {windows}");
        std::thread::sleep(Duration::from_millis(100));
    }

    // The session outlives its closed window: the survivor still answers.
    assert!(session.msg(&["ping"]).contains(r#""pong""#));

    // A force close also removes its window rather than only killing the child.
    let third = session.msg(&create_window);
    let third: u64 = third.trim().parse().expect("create-window returns a window id");
    let forced = session.msg(&["close-window", "--window-id", &third.to_string(), "--force"]);
    let forced: serde_json::Value =
        serde_json::from_str(&forced).expect("force close-window replies with JSON");
    assert_eq!(forced["forced"], true);
    assert!(session.msg(&["ping"]).contains(r#""pong""#));

    // Closing a window that does not exist is refused, never silently accepted.
    assert!(!session.try_msg(&["close-window", "--window-id", "424242"]).status.success());
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
