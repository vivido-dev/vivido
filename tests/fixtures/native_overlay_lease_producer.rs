//! Bounded SDK producer for owned native-window leased-recovery acceptance.

// Rust guideline compliant 2026-10-07
#[cfg(target_os = "macos")]
mod native {
    use std::fs::{self, File};
    use std::io::{self, Read};
    use std::path::Path;
    use std::time::{Duration, Instant};

    use serde::{Deserialize, Serialize};
    use vivid_protocol::registry;
    use vivid_sdk::overlay::{
        Brush, Canvas, Color, Command, Event, HitRole, OverlayWindowOptions, Path as VectorPath,
        PresentationOutcome, Rect, WindowMode,
    };
    use vivid_sdk::{
        CleanupPolicy, OverlayLaneEvent, OverlaySession, ProducerAuthentication, ProducerConfig,
        Session, SessionLeaseBuilder,
    };

    #[derive(Serialize)]
    struct State {
        serial: u64,
        stage: String,
        revision: [u64; 2],
        presented: [bool; 2],
        closed: [bool; 2],
        keys: [u64; 2],
        text: [Vec<String>; 2],
        colors: [u32; 2],
        controls: u64,
        bad_outcome: Option<&'static str>,
        sample_points: [[f64; 2]; 2],
        viewport: [f64; 4],
        reconciled: [[u64; 2]; 2],
        source_owner: u64,
        resumed: bool,
        old_receipt_retired: bool,
        focus_before_fresh_denied: bool,
        stale_cancel_calls: u64,
    }

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        serial: u64,
        operation: String,
        owner: Option<usize>,
        allow_presented: Option<bool>,
    }

    fn canvas(count: usize, color: u32) -> Canvas {
        assert!((1..=4095).contains(&count));
        let mut scene = Canvas::new();
        let bounds = Rect::new(0., 0., 2048., 2048.).unwrap();
        for _ in 0..count {
            scene.fill(VectorPath::rectangle(bounds).unwrap(), Brush::Solid(Color(color))).unwrap();
        }
        scene
            .push(Command::Hit {
                id: 7,
                path: VectorPath::rectangle(bounds).unwrap(),
                role: HitRole::Input,
                cursor: None,
            })
            .unwrap();
        scene
    }

    fn config() -> ProducerConfig {
        let mut required_profiles: Vec<_> = [
            registry::CORE_CONTROL,
            registry::LIVE_MEDIA,
            registry::TERMINAL_SURFACE,
            registry::TERMINAL_OVERLAY,
            registry::VECTOR_SCENE,
            registry::OVERLAY_INPUT,
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        required_profiles.sort();
        ProducerConfig {
            required_profiles,
            optional_profiles: Vec::new(),
            ..ProducerConfig::default()
        }
    }

    fn write(directory: &Path, state: &State) -> io::Result<()> {
        let bytes = serde_json::to_vec(state)?;
        assert!(bytes.len() < 16384);
        fs::write(directory.join("state.tmp"), bytes)?;
        fs::rename(directory.join("state.tmp"), directory.join("state.json"))
    }

    pub(super) fn run(directory: &Path) -> io::Result<()> {
        let mut controller = Session::connect(config())?;
        let context = controller.info().root_context_id;
        let (definition, mut activation) = SessionLeaseBuilder::new(context, 51)
            .permitted_profiles(controller.info().accepted_profiles.clone())
            .contract(controller.info().resource_contract.clone())
            .cleanup_policy(CleanupPolicy::SuspendOnUncleanLoss)
            .disconnect_grace_us(10_000_000)
            .build()?;
        let lease = controller.create_session_lease(&definition, &Default::default())?;
        assert_eq!(lease.disconnect_grace_us, 10_000_000);
        let mut leased = config();
        leased.authentication = ProducerAuthentication::lease_activation_bytes(
            context,
            51,
            activation.take().expect("new activation authority is available once"),
        )?;
        let session = Session::connect(leased)?;
        let source_owner = session.info().session_id;
        let mut owner = OverlaySession::from_session(session)?;
        let peer_owner = OverlaySession::connect(config())?;
        let peer = peer_owner.create_window(OverlayWindowOptions::new(
            Rect::new(0., 0., 56., 56.).unwrap(),
            WindowMode::Floating,
        ))?;
        let viewport = peer.viewport()?;
        assert!(viewport.width.get() > 1300. && viewport.height.get() > 750.);
        let mut first = owner.create_window(OverlayWindowOptions::new(
            Rect::new(0., 0., viewport.width.get() - 56., viewport.height.get()).unwrap(),
            WindowMode::Floating,
        ))?;
        peer.set_bounds(Rect::new(viewport.width.get() - 56., 0., 56., 56.).unwrap())?;
        let mut receipts =
            [Some(first.submit(canvas(1, 0xc02020ff))?), Some(peer.submit(canvas(1, 0x2030c0ff))?)];
        for receipt in receipts.iter().flatten() {
            assert_eq!(
                receipt.wait(Duration::from_secs(15))?,
                Some(PresentationOutcome::Presented)
            );
        }
        first.request_focus()?;
        let old_cancel = owner.cancel_handle()?;
        let mut bad = None;
        let mut state = State {
            serial: 0,
            stage: "ready".into(),
            revision: [1, 1],
            presented: [true, true],
            closed: [false, false],
            keys: [0, 0],
            text: [vec![], vec![]],
            colors: [0xc02020ff, 0x2030c0ff],
            controls: 0,
            bad_outcome: None,
            sample_points: [[80., 120.], [viewport.width.get() - 28., 28.]],
            viewport: [
                viewport.width.get(),
                viewport.height.get(),
                f64::from(viewport.scale_numerator),
                f64::from(viewport.scale_denominator),
            ],
            reconciled: [[1, 1], [1, 1]],
            source_owner,
            resumed: false,
            old_receipt_retired: false,
            focus_before_fresh_denied: false,
            stale_cancel_calls: 0,
        };
        write(directory, &state)?;
        println!("NATIVE_LEASE_READY");
        let until = Instant::now() + Duration::from_secs(120);
        while Instant::now() < until {
            if let Ok(file) = File::open(directory.join("command.json")) {
                let mut bytes = Vec::with_capacity(1025);
                file.take(1025).read_to_end(&mut bytes)?;
                assert!(bytes.len() <= 1024);
                let request: Request = serde_json::from_slice(&bytes)?;
                if request.serial > state.serial {
                    assert_eq!(request.serial, state.serial.checked_add(1).unwrap());
                    assert!(!request.allow_presented.unwrap_or(false));
                    match request.operation.as_str() {
                        "hostile" => {
                            assert!(bad.is_none() && !state.resumed);
                            let receipt = first.submit(canvas(4095, 0x00000080))?;
                            let deadline = Instant::now() + Duration::from_secs(5);
                            while first.reconcile()?.accepted_revision < receipt.revision() {
                                assert!(Instant::now() < deadline);
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            state.revision[0] = receipt.revision();
                            receipts[0] = None;
                            bad = Some(receipt);
                        },
                        "resume" => {
                            assert_eq!(bad.as_ref().unwrap().wait(Duration::ZERO)?, None);
                            owner.resume(config(), &mut [&mut first])?;
                            state.resumed = true;
                            state.old_receipt_retired =
                                bad.as_ref().unwrap().wait(Duration::ZERO).is_err();
                            state.focus_before_fresh_denied = first.request_focus().is_err();
                            assert!(state.old_receipt_retired && state.focus_before_fresh_denied);
                            assert_eq!(first.reconcile()?.presented_revision, 1);
                            assert_eq!(peer.reconcile()?.presented_revision, 1);
                        },
                        "replace" => {
                            assert!(state.resumed);
                            receipts = [
                                Some(first.submit(canvas(1, 0x20c0c0ff))?),
                                Some(peer.submit(canvas(1, 0x20b040ff))?),
                            ];
                            state.revision = [3, 2];
                            assert_eq!(receipts[0].as_ref().unwrap().revision(), 3);
                            assert_eq!(receipts[1].as_ref().unwrap().revision(), 2);
                            state.colors = [0x20c0c0ff, 0x20b040ff];
                        },
                        "focus" => match request.owner.unwrap() {
                            0 => first.request_focus()?,
                            1 => peer.request_focus()?,
                            _ => panic!("fixture owner is out of range"),
                        },
                        "stale_cancel" => {
                            assert!(state.resumed);
                            old_cancel();
                            assert_eq!(first.reconcile()?.presented_revision, 3);
                            state.stale_cancel_calls += 1;
                        },
                        "advance" => {
                            receipts[1] = Some(peer.submit(canvas(1, 0xb020b0ff))?);
                            state.revision[1] = receipts[1].as_ref().unwrap().revision();
                            state.colors[1] = 0xb020b0ff;
                        },
                        "finish" => {
                            assert!(
                                state.resumed
                                    && state.old_receipt_retired
                                    && state.stale_cancel_calls == 2
                            );
                            for receipt in receipts.iter().flatten() {
                                assert_eq!(
                                    receipt.wait(Duration::ZERO)?,
                                    Some(PresentationOutcome::Presented)
                                );
                            }
                            state.serial = request.serial;
                            state.stage = "finished".into();
                            first.close()?;
                            peer.close()?;
                            owner.close()?;
                            peer_owner.close()?;
                            controller.close()?;
                            // A terminal completion proves explicit cleanup succeeded,
                            // even if the native terminal exits before the parent polls.
                            write(directory, &state)?;
                            return Ok(());
                        },
                        _ => panic!("unknown native lease fixture command"),
                    }
                    state.serial = request.serial;
                    state.stage = request.operation;
                    println!("NATIVE_LEASE_STAGE {}", state.serial);
                }
            }
            controller.query_session()?;
            state.controls += 1;
            for (i, window) in [&first, &peer].into_iter().enumerate() {
                let status = window.reconcile()?;
                state.reconciled[i] = [status.accepted_revision, status.presented_revision];
                state.presented[i] = receipts[i].as_ref().is_some_and(|r| {
                    r.wait(Duration::ZERO).unwrap() == Some(PresentationOutcome::Presented)
                });
            }
            if let Some(receipt) = &bad {
                match receipt.wait(Duration::ZERO) {
                    Err(_) => {
                        assert!(state.resumed);
                        state.bad_outcome = Some("retired");
                    },
                    Ok(None) => {},
                    Ok(Some(_)) => {
                        panic!("pending hostile original resolved before lease recovery")
                    },
                }
            }
            for (i, session) in [&owner, &peer_owner].into_iter().enumerate() {
                for _ in 0..32 {
                    match session.wait_event(Duration::ZERO)? {
                        None => break,
                        Some(OverlayLaneEvent::Outcome(_)) => {
                            panic!("duplicate/orphan original outcome")
                        },
                        Some(OverlayLaneEvent::ConnectionLost { .. }) => {
                            panic!("live recovered fixture lost its lane")
                        },
                        Some(OverlayLaneEvent::Input(event)) => {
                            assert!(
                                session
                                    .event_targets(&event, if i == 0 { &first } else { &peer })?
                            );
                            match event.event {
                                Event::Text(text) => {
                                    assert_eq!(event.scene_revision, state.revision[i]);
                                    assert!(text.len() <= 64 && state.text[i].len() < 32);
                                    state.text[i].push(text);
                                },
                                Event::Key { down: true, .. } => {
                                    assert_eq!(event.scene_revision, state.revision[i]);
                                    state.keys[i] += 1;
                                },
                                Event::Dismissed(_) => {
                                    panic!("healthy fixture window was dismissed")
                                },
                                _ => {},
                            }
                        },
                        Some(_) => {},
                    }
                }
            }
            write(directory, &state)?;
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(io::Error::new(io::ErrorKind::TimedOut, "native lease fixture exceeded its deadline"))
    }
}

fn main() {
    #[cfg(target_os = "macos")]
    {
        let directory = std::env::args_os().nth(1).expect("owned fixture directory is required");
        if let Err(error) = native::run(std::path::Path::new(&directory)) {
            eprintln!("native lease fixture failed ({:?})", error.kind());
            std::process::exit(1);
        }
    }
    #[cfg(not(target_os = "macos"))]
    panic!("native leased-window qualification requires macOS");
}
