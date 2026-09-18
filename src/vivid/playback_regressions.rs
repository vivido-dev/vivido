#[cfg(unix)]
mod process_playback {
    use super::*;
    use std::io::Write;
    use std::process::{Child, Command, Stdio};

    // The child sees an actual terminal, so Space/q use vivi's production keyboard queue.
    // No automation endpoint or existing user window is involved. Output is bounded and
    // retained only for failure diagnostics; authenticated media uses separate sockets.
    const PTY_DRIVER: &str = r#"
import os, pty, select, struct, subprocess, sys, termios, fcntl, tempfile, json, shlex
directory = tempfile.TemporaryDirectory(prefix='vivi-playback-')
argv = [os.environ['VIVI_TEST_BIN'], os.environ['VIVI_TEST_MEDIA']]
mux = os.environ.get('VIVI_TEST_USE_MUX') == '1'
if mux:
    os.environ['XDG_RUNTIME_DIR'] = directory.name
    os.environ['XDG_STATE_HOME'] = directory.name + '/state'
    os.environ['XDG_CONFIG_HOME'] = directory.name + '/config'
    config = directory.name + '/vvmux.toml'
    layout = directory.name + '/layout.toml'
    with open(config, 'w') as f:
        f.write('[general]\nshell = "/bin/sh"\n[session]\nauto_snapshot = false\n')
    playback = ' '.join(shlex.quote(arg) for arg in argv)
    with open(layout, 'w') as f:
        f.write('[[tabs]]\nname = "playback"\n[tabs.layout]\npane = "video"\ncommand = ' +
                json.dumps('for attempt in 1 2 3; do ' + playback + '; done') + '\n')
    argv = [os.environ['VVMUX_TEST_BIN'], '--config', config, 'new',
            '--session', 'playback-test', '--layout', layout]
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 40, 100, 1000, 800))
child = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave)
os.close(slave)
tail = b''
try:
    while child.poll() is None:
        ready, _, _ = select.select([master, sys.stdin.buffer], [], [], .02)
        for source in ready:
            if source == master:
                try: data = os.read(master, 4096)
                except OSError: data = b''
                tail = (tail + data)[-8192:]
            else:
                data = os.read(sys.stdin.fileno(), 32)
                if not data:
                    child.terminate()
                    try: child.wait(timeout=2)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
                    break
                os.write(master, data)
    sys.stderr.buffer.write(tail)
    sys.exit(child.wait())
finally:
    if child.poll() is None:
        child.kill()
        child.wait()
    os.close(master)
    if mux:
        subprocess.run([os.environ['VVMUX_TEST_BIN'], '--config', config, 'kill-session',
                        '--target', 'playback-test'], stdout=subprocess.DEVNULL,
                       stderr=subprocess.DEVNULL, timeout=5)
    directory.cleanup()
"#;

    struct Player(Child);
    impl Player {
        fn start(service: &VividService, mux: bool) -> Self {
            Self(
                Command::new("python3")
                    .args(["-c", PTY_DRIVER])
                    .env("VIVID_ENDPOINT_CONTROL", service.control_endpoint())
                    .env("VIVID_ENDPOINT_REALTIME", service.control_endpoint())
                    .env("VIVID_ENDPOINT_BULK", service.control_endpoint())
                    .env("VIVID_ROOT_SECRET", service.root_secret())
                    .env("TERM", "xterm-256color")
                    .env("TMUX", "playback-test")
                    .env("VIVI_TEST_USE_MUX", if mux { "1" } else { "0" })
                    .env_remove("VIVID_REMOTE")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap(),
            )
        }

        fn key(&mut self, key: &[u8]) {
            self.0.stdin.as_mut().unwrap().write_all(key).unwrap();
        }

        fn wait(
            &mut self,
            service: &VividService,
            description: &str,
            ready: impl Fn(&VividService) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(20);
            while !ready(service) {
                if let Some(status) = self.0.try_wait().unwrap() {
                    panic!("player exited during {description}: {status}; {}", self.tail());
                }
                assert!(
                    Instant::now() < deadline,
                    "{description}: {:?}",
                    service
                        .scene
                        .track_keys()
                        .iter()
                        .filter_map(|key| {
                            service.scene.track_status(*key).map(|s| {
                                (
                                    key.track_id,
                                    s.last_decoded_pts_us,
                                    service.scene.playback_state(*key, None),
                                    s.state.milestones,
                                )
                            })
                        })
                        .collect::<Vec<_>>()
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn tail(&mut self) -> String {
            use std::io::Read;
            let mut tail = String::new();
            self.0.stderr.as_mut().unwrap().read_to_string(&mut tail).unwrap();
            tail
        }

        fn quit(&mut self) {
            self.key(b"q");
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    assert!(status.success(), "quit failed: {}", self.tail());
                    return;
                }
                assert!(Instant::now() < deadline, "quit did not cancel playback");
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Player {
        fn drop(&mut self) {
            // EOF tells the driver to terminate and reap its own child, including on panic.
            self.0.stdin.take();
            let _ = self.0.wait();
            if thread::panicking() {
                eprintln!("bounded terminal output: {}", self.tail());
            }
        }
    }

    #[test]
    #[ignore = "requires VIVI_TEST_BIN, VIVI_TEST_MEDIA and Python 3; real native media acceptance"]
    fn native_vivi_pause_quit_relaunch() {
        for variable in ["VIVI_TEST_BIN", "VIVI_TEST_MEDIA"] {
            assert!(std::env::var_os(variable).is_some(), "missing {variable}");
        }
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|_| {})).unwrap();
        service.shared.clocked_test_audio.store(true, Ordering::SeqCst);
        for _ in 0..3 {
            let mut player = Player::start(&service, false);
            player.wait(&service, "audio/video start", |service| {
                let video = service.scene.track_keys().into_iter().any(|key| {
                    service.scene.track_status(key).is_some_and(|status| {
                        matches!(status.configuration.kind, KindConfiguration::Video(_))
                            && status.last_decoded_pts_us.is_some_and(|pts| pts > 500_000)
                    })
                });
                video
                    && lock(&service.shared.audio_outputs)
                        .values()
                        .any(|audio| audio.rendered_pts().is_some_and(|pts| pts > 500_000))
            });
            player.key(b" ");
            player.wait(&service, "pause", |service| {
                service
                    .scene
                    .track_keys()
                    .into_iter()
                    .any(|key| service.scene.playback_state(key, None).is_some_and(|s| s.0 == 3))
            });
            player.quit();
            let deadline = Instant::now() + Duration::from_secs(2);
            while !service.scene.track_keys().is_empty() {
                assert!(Instant::now() < deadline, "old producer tracks survived quit");
                thread::sleep(Duration::from_millis(5));
            }
        }
    }

    #[test]
    #[ignore = "requires VIVI_TEST_BIN, VVMUX_TEST_BIN, VIVI_TEST_MEDIA and Python 3"]
    fn vvmux_vivi_pause_quit_relaunch() {
        for variable in ["VIVI_TEST_BIN", "VVMUX_TEST_BIN", "VIVI_TEST_MEDIA"] {
            assert!(std::env::var_os(variable).is_some(), "missing {variable}");
        }
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|_| {})).unwrap();
        service.shared.clocked_test_audio.store(true, Ordering::SeqCst);
        let mut player = Player::start(&service, true);
        let mut retired_track = 0;
        for attempt in 0..3 {
            player.wait(&service, "replacement audio/video start", |service| {
                service.scene.track_keys().into_iter().any(|key| {
                    key.track_id > retired_track
                        && service.scene.track_status(key).is_some_and(|status| {
                            matches!(status.configuration.kind, KindConfiguration::Video(_))
                                && status.last_decoded_pts_us.is_some_and(|pts| pts > 500_000)
                        })
                        && lock(&service.shared.audio_outputs).iter().any(|(audio_key, audio)| {
                            audio_key.surface == key.surface
                                && audio.rendered_pts().is_some_and(|pts| pts > 500_000)
                        })
                })
            });
            player.key(b" ");
            player.wait(&service, "pause before producer exit", |service| {
                service.scene.track_keys().into_iter().any(|key| {
                    key.track_id > retired_track
                        && service.scene.playback_state(key, None).is_some_and(|s| s.0 == 3)
                })
            });
            retired_track =
                service.scene.track_keys().iter().map(|key| key.track_id).max().unwrap();
            if attempt == 2 {
                player.quit();
            } else {
                player.key(b"q");
            }
        }
    }
}

#[cfg(windows)]
mod windows_process_playback {
    use super::*;
    use crate::terminal::event::WindowSize;
    use crate::terminal::tty::{self, EventedReadWrite, Options, Shell};
    use std::io::{Read, Write};
    use std::os::windows::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    struct TestMux {
        pty: tty::Pty,
        binary: PathBuf,
        name: String,
        _directory: tempfile::TempDir,
        tail: Vec<u8>,
    }
    impl TestMux {
        fn start(service: &VividService, second_tab: bool) -> Self {
            let binary = PathBuf::from(std::env::var_os("VVMUX_TEST_BIN").expect("VVMUX_TEST_BIN"));
            let vivi = std::env::var("VIVI_TEST_BIN").expect("VIVI_TEST_BIN");
            let media = std::env::var("VIVI_TEST_MEDIA").expect("VIVI_TEST_MEDIA");
            let directory = tempfile::tempdir().unwrap();
            let name = format!(
                "playback-test-{}",
                directory.path().file_name().unwrap().to_string_lossy()
            );
            let config = directory.path().join("vvmux.toml");
            let layout = directory.path().join("layout.toml");
            let script = directory.path().join("playback.ps1");
            let ps_quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
            std::fs::write(
                &script,
                if second_tab {
                    format!("& {} {}", ps_quote(&vivi), ps_quote(&media))
                } else {
                    format!(
                        "1..3 | ForEach-Object {{ & {} {} }}",
                        ps_quote(&vivi),
                        ps_quote(&media)
                    )
                },
            )
            .unwrap();
            std::fs::write(
                &config,
                "[general]\nshell = 'powershell.exe'\n[session]\nauto_snapshot = false\n",
            )
            .unwrap();
            let command = format!("powershell.exe -NoProfile -File \"{}\"", script.display());
            let idle_tab = if second_tab {
                "[[tabs]]\nname = 'idle'\n[tabs.layout]\npane = 'idle'\n"
            } else {
                ""
            };
            std::fs::write(
                &layout,
                format!(
                    "[[tabs]]\nname = 'playback'\n[tabs.layout]\npane = 'video'\ncommand = {}\n{idle_tab}",
                    serde_json::to_string(&command).unwrap()
                ),
            )
            .unwrap();
            let options = Options {
                shell: Some(Shell::new(
                    binary.to_string_lossy().into_owned(),
                    vec![
                        "--config".into(),
                        config.to_string_lossy().into_owned(),
                        "new".into(),
                        "--session".into(),
                        name.clone(),
                        "--layout".into(),
                        layout.to_string_lossy().into_owned(),
                    ],
                )),
                escape_args: true,
                env: std::collections::HashMap::from([
                    ("VIVID_ENDPOINT_CONTROL".into(), service.control_endpoint().into()),
                    ("VIVID_ENDPOINT_REALTIME".into(), service.control_endpoint().into()),
                    ("VIVID_ENDPOINT_BULK".into(), service.control_endpoint().into()),
                    ("VIVID_ROOT_SECRET".into(), service.root_secret().into()),
                    ("TMUX".into(), "playback-test".into()),
                ]),
                ..Default::default()
            };
            let pty = tty::new(
                &options,
                WindowSize { num_lines: 40, num_cols: 100, cell_width: 10, cell_height: 20 },
                0,
            )
            .unwrap();
            Self { pty, binary, name, _directory: directory, tail: Vec::new() }
        }
        fn key(&mut self, key: &[u8]) {
            self.pty.writer().write_all(key).unwrap();
        }
        fn drain(&mut self) {
            let mut bytes = [0; 4096];
            while let Ok(count) = self.pty.reader().read(&mut bytes) {
                if count == 0 {
                    break;
                }
                self.tail.extend_from_slice(&bytes[..count]);
                if self.tail.len() > 8192 {
                    self.tail.drain(..self.tail.len() - 8192);
                }
                // ConPTY asks its host for the cursor position during startup.
                if bytes[..count].windows(4).any(|w| w == b"\x1b[6n") {
                    self.key(b"\x1b[1;1R");
                }
            }
        }
        fn wait(
            &mut self,
            service: &VividService,
            label: &str,
            ready: impl Fn(&VividService) -> bool,
        ) {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !ready(service) {
                self.drain();
                assert!(
                    Instant::now() < deadline,
                    "{label}; tracks={:?}; terminal={}",
                    service
                        .scene
                        .track_keys()
                        .iter()
                        .filter_map(|key| service.scene.track_status(*key).map(|s| (
                            key.track_id,
                            s.last_decoded_pts_us,
                            service.scene.playback_state(*key, None),
                            s.state.milestones
                        )))
                        .collect::<Vec<_>>(),
                    String::from_utf8_lossy(&self.tail)
                );
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
    impl Drop for TestMux {
        fn drop(&mut self) {
            // Only the uniquely named session created by this test is targeted.
            let _ = Command::new(&self.binary)
                .args(["kill-session", "--target", &self.name])
                .creation_flags(0x08000000)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            self.drain();
        }
    }

    #[test]
    #[ignore = "requires VIVI_TEST_BIN, VVMUX_TEST_BIN and VIVI_TEST_MEDIA; isolated ConPTY acceptance"]
    fn windows_vvmux_vivi_pause_quit_relaunch() {
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|_| {})).unwrap();
        service.shared.clocked_test_audio.store(true, Ordering::SeqCst);
        let mut player = TestMux::start(&service, false);
        let mut retired_track = 0;
        for _ in 0..3 {
            player.wait(&service, "replacement audio/video start", |service| {
                service.scene.track_keys().into_iter().any(|key| {
                    key.track_id > retired_track
                        && service.scene.track_status(key).is_some_and(|s| {
                            matches!(s.configuration.kind, KindConfiguration::Video(_))
                                && s.last_decoded_pts_us.is_some_and(|pts| pts > 500_000)
                        })
                        && lock(&service.shared.audio_outputs).iter().any(|(audio_key, audio)| {
                            audio_key.surface == key.surface
                                && audio.rendered_pts().is_some_and(|pts| pts > 500_000)
                        })
                })
            });
            player.key(b" ");
            player.wait(&service, "pause before quit", |service| {
                service.scene.track_keys().into_iter().any(|key| {
                    key.track_id > retired_track
                        && service.scene.playback_state(key, None).is_some_and(|s| s.0 == 3)
                })
            });
            retired_track =
                service.scene.track_keys().iter().map(|key| key.track_id).max().unwrap();
            player.key(b"q");
        }
        player.wait(&service, "final producer cleanup", |s| s.scene.track_keys().is_empty());
    }

    #[test]
    #[ignore = "requires VIVI_TEST_BIN, VVMUX_TEST_BIN and VIVI_TEST_MEDIA; isolated ConPTY acceptance"]
    fn windows_vvmux_vivi_hide_resume_and_seek() {
        exercise_hide_resume_and_seek(&[15, 45]);
    }

    #[test]
    #[ignore = "requires VIVI_TEST_BIN, VVMUX_TEST_BIN and VIVI_TEST_MEDIA; isolated ConPTY acceptance"]
    fn windows_vvmux_vivi_paused_tab_return() {
        exercise_hide_resume_and_seek(&[]);
    }

    fn exercise_hide_resume_and_seek(durations: &[u64]) {
        let service = VividService::start_with_wake(test_geometry(), Arc::new(|_| {})).unwrap();
        service.shared.clocked_test_audio.store(true, Ordering::SeqCst);
        let mut player = TestMux::start(&service, true);
        let progress = |service: &VividService, target| {
            service.scene.track_keys().into_iter().any(|key| {
                service.scene.track_status(key).is_some_and(|s| {
                    matches!(s.configuration.kind, KindConfiguration::Video(_))
                        && s.last_decoded_pts_us.is_some_and(|pts| pts > target)
                        && service
                            .scene
                            .latest_frame(key)
                            .is_some_and(|frame| frame.pts_us > target)
                }) && lock(&service.shared.audio_outputs).iter().any(|(audio_key, audio)| {
                    audio_key.surface == key.surface
                        && audio.rendered_pts().is_some_and(|pts| pts > target)
                })
            })
        };
        player.wait(&service, "initial playback", |s| progress(s, 2_000_000));
        for &duration in durations {
            let before = lock(&service.shared.audio_outputs)
                .values()
                .filter_map(|a| a.rendered_pts())
                .max()
                .unwrap();
            player.key(b"\x02n");
            player.wait(&service, "hidden projection retires", |s| s.scene.track_keys().is_empty());
            let deadline = Instant::now() + Duration::from_secs(duration);
            while Instant::now() < deadline {
                player.drain();
                assert!(service.scene.track_keys().is_empty(), "hidden tracks resumed");
                assert!(lock(&service.shared.audio_outputs).is_empty(), "hidden audio retained");
                thread::sleep(Duration::from_millis(10));
            }
            player.key(b"\x02p");
            player.wait(&service, "audio/video resume", |s| progress(s, before + 500_000));
        }
        let before = lock(&service.shared.audio_outputs)
            .values()
            .filter_map(|a| a.rendered_pts())
            .max()
            .unwrap();
        let seek_started = Instant::now();
        player.key(b"f");
        player.wait(&service, "ten-second forward seek", |s| progress(s, before + 9_500_000));
        assert!(
            seek_started.elapsed() < Duration::from_secs(8),
            "ordinary playback masked a failed seek"
        );
        player.key(b" ");
        player.wait(&service, "pause before hiding", |s| {
            s.scene
                .track_keys()
                .into_iter()
                .any(|key| s.scene.playback_state(key, None).is_some_and(|state| state.0 == 3))
        });
        player.key(b"\x02n");
        player.wait(&service, "paused projection retires", |s| s.scene.track_keys().is_empty());
        player.key(b"\x02p");
        player.wait(&service, "paused picture restored", |s| {
            s.scene.track_keys().into_iter().any(|key| {
                s.scene.track_status(key).is_some_and(|status| {
                    matches!(status.configuration.kind, KindConfiguration::Video(_))
                        && status.last_decoded_pts_us.is_some()
                }) && s.scene.playback_state(key, None).is_some_and(|state| {
                    state.0 == 3
                        && s.scene.latest_frame(key).is_some_and(|frame| frame.pts_us >= state.1)
                })
            })
        });
        let paused_until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < paused_until {
            player.drain();
            assert!(
                lock(&service.shared.audio_outputs).values().all(|a| !a.enabled_for_test()),
                "paused restoration started audio"
            );
            thread::sleep(Duration::from_millis(10));
        }
        player.key(b" ");
        player.wait(&service, "unpause after tab return", |s| progress(s, before + 10_500_000));
        player.key(b"q");
    }
}
