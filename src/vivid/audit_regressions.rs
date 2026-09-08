// Positive regressions for the 2026-09-07 presenter audit.
#[test]
fn rep_scroll_places_authenticated_anchors_and_preserves_the_other_owner() {
    use crate::terminal::event::{Event as TerminalEvent, EventListener, VoidListener};
    use crate::terminal::event_loop::State;
    use crate::terminal::term::{Config as TermConfig, Term, test::TermSize};
    use vvte::ansi::Handler;

    struct PresenterEvents(Arc<VividService>);
    impl EventListener for PresenterEvents {
        fn send_event(&self, event: TerminalEvent) {
            match event {
                TerminalEvent::VividGridScroll { origin, end, lines, history_size } => {
                    self.0.handle_grid_scroll(origin, end, lines, history_size);
                },
                TerminalEvent::VividMarker { marker, line, column, alternate } => {
                    self.0.handle_terminal_marker(&marker, line, column, alternate);
                },
                _ => {},
            }
        }
    }

    let service = Arc::new(socket_service!(VividService::start_with_wake(
        test_geometry(),
        Arc::new(|_| {}),
    )));
    let first = connect(&service);
    let second = connect(&service);
    let first_context = first.info().root_context_id;
    let second_context = second.info().root_context_id;
    let anchor_id = 11;
    let first_marker = first.anchor_marker(first_context, anchor_id).unwrap();
    let second_marker = second.anchor_marker(second_context, anchor_id).unwrap();
    let mut term = Term::new(
        TermConfig::default(),
        &TermSize::new(80, 24),
        PresenterEvents(service.clone()),
    );
    let mut pipeline = State::default();
    pipeline.advance_test_chunks(&mut term, first_marker.as_bytes().chunks(1));
    assert_eq!(first.query_anchor(first_context, anchor_id).unwrap().state, 1);

    // Enough REP output to wrap and scroll; the following marker must observe the new cursor.
    let input = format!("a\x1b[2000b{second_marker}");
    pipeline.advance_test_chunks(&mut term, input.as_bytes().chunks(1));
    let mut scalar = Term::new(TermConfig::default(), &TermSize::new(80, 24), VoidListener);
    for _ in 0..2001 {
        scalar.input('a');
    }
    let point = scalar.grid().cursor.point;
    assert_eq!(second.query_anchor(second_context, anchor_id).unwrap().state, 1);
    let positions = service.scene.anchor_positions();
    let expected_identity = lock(&service.shared.registry).sessions[&second.info().session_id]
        .identity.context(second_context).unwrap().anchor(anchor_id).unwrap();
    assert!(positions.contains(&(expected_identity, point.column.0, point.line.0, false)));

    // Both owners reused anchor 11. Closing the earlier owner cannot unpublish the later anchor.
    let before = second.query_anchor(second_context, anchor_id).unwrap();
    let first_id = first.info().session_id;
    first.cancel_handle()();
    assert!(wait_until(Duration::from_secs(2), || {
        !lock(&service.shared.registry).sessions.contains_key(&first_id)
    }));
    assert_eq!(second.query_anchor(second_context, anchor_id).unwrap(), before);
    second.query_session().unwrap();
}

#[test]
fn root_hello_replay_is_rejected() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let open = || {
        let ProducerAuthentication::Root { root_secret } = test_config(&service).authentication
        else {
            unreachable!()
        };
        RawClient::open(
            &service,
            HelloAuthentication::Root { proof: [0; 32] },
            move |hello, preface| {
                hello.authenticate_root(&root_secret, preface).map_err(io::Error::other)
            },
        )
    };
    let mut first = open().unwrap();
    assert!(open().is_err());
    first.query_session().unwrap();
}

#[test]
fn lost_welcome_retries_the_same_immediate_lease() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let mut controller = connect(&service);
    let context = controller.info().root_context_id;
    let secret = Secret32::new([0x71; 32]);
    controller
        .create_session_lease(&lease_definition(context, 31, &secret), &RequestMetadata::default())
        .unwrap();
    let first = RawClient::activate(&service, context, 31, &secret).unwrap();
    let id = first.session_id;
    drop(first); // No post-HELLO record: exact activation retry must remain possible.
    assert!(wait_until(Duration::from_secs(2), || {
        !lock(&service.shared.registry).sessions.contains_key(&id)
    }));
    let mut retry = RawClient::activate(&service, context, 31, &secret).unwrap();
    assert_eq!(retry.session_id, id);
    retry.query_session().unwrap();
    assert!(RawClient::activate(&service, context, 31, &secret).is_err());
    controller.query_session().unwrap();
}

#[test]
fn egress_close_join_cancels_nonreading_peer() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let peer = RawClient::root(&service).unwrap();
    assert!(wait_until(Duration::from_secs(2), || lock(&service.shared.registry)
        .sessions
        .contains_key(&peer.session_id)));
    let runtime = lock(&service.shared.registry).sessions[&peer.session_id].clone();
    let egress = lock(&runtime.egress).clone().unwrap();
    egress.pause_worker_for_test();
    for _ in 0..8 {
        assert!(egress.send(messages::SESSION_STATUS, 0, vec![0; 64 * 1024]));
    }
    let (done, completed) = mpsc::channel();
    let waiter = thread::spawn(move || {
        egress.close();
        egress.join();
        done.send(()).unwrap();
    });
    let finished = completed.recv_timeout(Duration::from_secs(1)).is_ok();
    let _ = peer.stream.shutdown(std::net::Shutdown::Both);
    drop(peer);
    waiter.join().unwrap();
    assert!(finished);
}

#[test]
fn welcome_respects_hello_receive_ceiling() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let ProducerAuthentication::Root { root_secret } = test_config(&service).authentication else {
        unreachable!()
    };
    let peer = RawClient::open(
        &service,
        HelloAuthentication::Root { proof: [0; 32] },
        move |hello, preface| {
            hello.maximum_control_body = 1;
            hello.authenticate_root(&root_secret, preface).map_err(io::Error::other)
        },
    );
    assert!(peer.is_err());
}

#[test]
fn web_control_rejects_body_larger_than_welcome_ceiling() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let ProducerAuthentication::Root { root_secret } = test_config(&service).authentication else {
        unreachable!()
    };
    let mut peer = RawClient::open(
        &service,
        HelloAuthentication::Root { proof: [0; 32] },
        move |hello, preface| {
            hello.optional_profiles.push(registry::WEB_CARRIER.into());
            hello.authenticate_root(&root_secret, preface).map_err(io::Error::other)
        },
    )
    .unwrap();
    assert!(wait_until(Duration::from_secs(2), || lock(&service.shared.registry)
        .sessions
        .contains_key(&peer.session_id)));
    let runtime = lock(&service.shared.registry).sessions[&peer.session_id].clone();
    assert!(runtime.supports(registry::WEB_CARRIER));
    let body = Envelope::new(2, vec![(99, Value::Bytes(vec![0; 300 * 1024]))]).encode().unwrap();
    assert!(body.len() > vivid_protocol::web::MAX_CONTROL_RECORD_BODY as usize);
    peer.stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
    let _ = write_raw(&mut peer.stream, 2, messages::PING, 0, &body);
    peer.stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    assert!(read_raw(&mut peer.stream).is_err());
}

#[test]
fn cancelled_session_preserves_another_owners_reused_track_and_node_ids() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let mut first = connect(&service);
    let mut second = connect(&service);
    let populate = |session: &mut vivid_sdk::Session| {
        let context = session.info().root_context_id;
        let surface = grid_surface(session, 70);
        session
            .create_node(&grid_node(context, 72, &surface, 2), &RequestMetadata::default())
            .unwrap();
        let track = session
            .create_track(
                TrackConfiguration {
                    direction: Default::default(),
                    context_id: context,
                    surface_id: 70,
                    track_id: 71,
                    slot: scene::SLOT_RASTER,
                    mode: TrackMode::Live,
                    lane: LaneClass::Bulk,
                    maximum_record_body: 128,
                    maximum_rate_millihertz: 60_000,
                    maximum_encoded_bits_per_second: 1_000_000,
                    maximum_records_per_second: 60,
                    maximum_inflight_body_bytes: 4096,
                    kind: KindConfiguration::Raster(RasterConfiguration {
                        width: 2,
                        height: 2,
                        alpha_mode: scene::ALPHA_STRAIGHT,
                        delta_enabled: false,
                        maximum_delta_operations: 1,
                        zstd_enabled: false,
                    }),
                    target_latency_us: 0,
                    maximum_latency_us: 1_000_000,
                    retained_pixel_charge: 4,
                },
                &RequestMetadata::default(),
            )
            .unwrap();
        (surface, track)
    };
    let (_first_surface, _first_track) = populate(&mut first);
    let (surface, track) = populate(&mut second);
    let first_id = first.info().session_id;
    let second_id = second.info().session_id;
    let context = second.info().root_context_id;
    let scene_query = || second.query_session();
    let before = scene_query().unwrap();
    first.cancel_handle()();
    assert!(wait_until(Duration::from_secs(2), || !lock(&service.shared.registry)
        .sessions
        .contains_key(&first_id)));
    assert!(second.query_track(&track).is_ok());
    assert_eq!(scene_query().unwrap(), before);
    assert_ne!(first_id, second_id);
    second.update_node(&grid_node(context, 72, &surface, 3), &RequestMetadata::default()).unwrap();
    assert!(second.query_track(&track).is_ok());
}

#[test]
fn lost_resume_welcome_retries_without_consuming_the_previous_key() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let mut controller = connect(&service);
    let context = controller.info().root_context_id;
    let secret = Secret32::new([0x74; 32]);
    controller
        .create_session_lease(
            &resumable_definition(context, 33, &secret),
            &RequestMetadata::default(),
        )
        .unwrap();
    let mut child = RawClient::activate(&service, context, 33, &secret).unwrap();
    let id = child.session_id;
    let prior = Secret32::new(*child.resume_key.expose());
    child.query_session().unwrap();
    drop(child);
    assert!(wait_until(Duration::from_secs(2), || !lock(&service.shared.registry)
        .sessions
        .contains_key(&id)));
    let resumed = RawClient::resume(&service, context, 33, id, 0, &prior).unwrap();
    assert_eq!(resumed.session_id, id);
    drop(resumed);
    assert!(wait_until(Duration::from_secs(2), || !lock(&service.shared.registry)
        .sessions
        .contains_key(&id)));
    let mut retried = RawClient::resume(&service, context, 33, id, 0, &prior).unwrap();
    assert_eq!(retried.session_id, id);
    retried.query_session().unwrap();
    assert!(RawClient::resume(&service, context, 33, id, 0, &prior).is_err());
    controller.query_session().unwrap();
}

#[test]
#[cfg(unix)]
fn welcome_write_failure_keeps_the_exact_activation_retry() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    let mut controller = connect(&service);
    let context = controller.info().root_context_id;
    let secret = Secret32::new([0x75; 32]);
    controller
        .create_session_lease(&lease_definition(context, 34, &secret), &RequestMetadata::default())
        .unwrap();
    let preface = vivid_protocol::wire::encode_preface(
        ConnectionKind::Control,
        vivid_protocol::CONTROL_MAX_RECORD_BODY,
    );
    let (mut peer, server) = std::os::unix::net::UnixStream::pair().unwrap();
    peer.write_all(&preface).unwrap();
    let (reader, _, _) = Reader::new(server).unwrap();
    let writer = Arc::new(reader.writer(ConnectionKind::Control).unwrap());
    let egress = Egress::start(writer.clone(), "welcome-failure-test").unwrap();
    drop(peer);
    let mut required_profiles =
        vec![registry::CORE_CONTROL.to_owned(), registry::TERMINAL_SURFACE.to_owned()];
    required_profiles.sort();
    let hello = Hello {
        producer_name: "raw".into(),
        producer_version: "0".into(),
        required_profiles,
        optional_profiles: vec![],
        maximum_control_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
        client_nonce: [0x3c; 32],
        target_profile: registry::TERMINAL_SURFACE.into(),
        extensions: vec![],
        authentication: HelloAuthentication::LeaseActivation {
            context_id: context,
            lease_id: 34,
            activation_secret: Secret32::new(*secret.expose()),
            attempt_id: [0x5a; 16],
            proof_of_possession: None,
        },
    };
    assert!(
        establish_root_session(&service.shared, writer, egress.clone(), &preface, &hello, 1)
            .is_err()
    );
    egress.close();
    egress.join();
    let mut retry = RawClient::activate(&service, context, 34, &secret).unwrap();
    retry.query_session().unwrap();
    controller.query_session().unwrap();
}

#[test]
fn root_replay_capacity_expires_without_evicting_live_entries() {
    let service = socket_service!(VividService::start_with_wake(test_geometry(), Arc::new(|_| {})));
    {
        let mut registry = lock(&service.shared.registry);
        for id in 0_u64..4096 {
            let mut nonce = [0; 32];
            nonce[..8].copy_from_slice(&id.to_be_bytes());
            registry.root_nonces.insert(nonce, Instant::now() + Duration::from_secs(300));
        }
    }
    assert!(RawClient::root(&service).is_err());
    {
        let mut registry = lock(&service.shared.registry);
        assert_eq!(registry.root_nonces.len(), 4096);
        for expiry in registry.root_nonces.values_mut() {
            *expiry = Instant::now();
        }
    }
    let _client = RawClient::root(&service).unwrap();
    assert_eq!(lock(&service.shared.registry).root_nonces.len(), 1);
}
