//! Deterministic coverage of service orchestration without native I/O.
#![cfg(feature = "test-util")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind};
use vivido::update::{UpdateError, UpdateEvent, UpdateManifest, UpdateService};
use vivido::{ConfigMonitor, EventSink, EventType};

#[test]
fn monitor_debounces_bursts_ignores_rename_from_and_stops_on_shutdown() {
    let (sink, events) = EventSink::headless();
    let primary = PathBuf::from("primary.toml");
    let imported = PathBuf::from("imported.toml");
    let (monitor, control) =
        ConfigMonitor::new_mocked(vec![primary.clone(), imported.clone()], sink);
    assert!(!monitor.needs_restart(&[imported.clone(), primary.clone()]));
    assert!(monitor.needs_restart(std::slice::from_ref(&primary)));
    control.notify(
        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
            .add_path(imported.clone()),
    );
    control.advance(Duration::from_secs(1));
    assert!(events.try_recv().is_err());
    for _ in 0..10_000 {
        control.notify(Event::new(EventKind::Any).add_path(imported.clone()));
    }
    control.advance(Duration::from_millis(9));
    assert!(events.try_recv().is_err());
    control.advance(Duration::from_millis(1));
    assert!(
        matches!(events.try_recv().unwrap().payload(), EventType::ConfigReload(path) if *path == primary)
    );
    assert!(events.try_recv().is_err());
    monitor.shutdown();
    control.notify(Event::new(EventKind::Any).add_path(primary));
    control.advance(Duration::from_secs(1));
    assert!(events.try_recv().is_err());
}

fn manifest() -> UpdateManifest {
    serde_json::from_value(serde_json::json!({
        "schema": 1, "product": "vivido", "version": "999.0.0",
        "publishedUtc": "2026-10-08T12:00:00Z",
        "asset": {"name": "test.msi", "url": "https://example.invalid/test.msi",
            "sha256": "0".repeat(64), "bytes": 1, "kind": "msi", "publisher": "Test"}
    }))
    .unwrap()
}

#[test]
fn update_results_are_isolated_and_cancellation_does_not_consume_a_result() {
    let (sink, events) = EventSink::headless();
    let (first, first_control) = UpdateService::new_mocked();
    let (second, second_control) = UpdateService::new_mocked();
    first_control.set_manifest(Err(UpdateError::Http("deterministic timeout".into())));
    second_control.set_manifest(Ok(manifest()));
    first.check(sink.clone(), true);
    assert!(matches!(
        events.try_recv().unwrap().payload(),
        EventType::Update(UpdateEvent::Failed { manual: true, .. })
    ));
    second_control.fail_next_worker_start();
    second.check(sink.clone(), false);
    assert!(matches!(
        events.try_recv().unwrap().payload(),
        EventType::Update(UpdateEvent::Failed { manual: false, .. })
    ));
    second.check(sink.clone(), false);
    assert!(matches!(
        events.try_recv().unwrap().payload(),
        EventType::Update(UpdateEvent::Available { manual: false, .. })
    ));

    first_control.set_download(Ok(PathBuf::from("verified-test-installer")));
    first.download(sink.clone(), manifest(), Arc::new(AtomicBool::new(true)));
    assert!(events.try_recv().is_err());
    first.download(sink, manifest(), Arc::new(AtomicBool::new(false)));
    assert!(
        matches!(events.try_recv().unwrap().payload(), EventType::Update(UpdateEvent::Ready { path, .. }) if path == std::path::Path::new("verified-test-installer"))
    );
}
