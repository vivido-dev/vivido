// Generation ownership and independent reader regressions.
#[test]
fn advancing_transfer_rejects_old_generation_completion() {
    let owner = session(1);
    let mut manager = FileDropManager::default();
    accepted_drop(&mut manager, owner, FileDropDestination::ShellCwd, 7);
    let other = session(2);
    accepted_drop(&mut manager, other, FileDropDestination::ShellCwd, 7);
    let transfer = manager.transfers.get_mut(&(owner, 7)).unwrap();
    transfer.active = true;
    let drop_id = transfer.drop_id;
    manager
        .advance(
            owner,
            AdvanceFileTransfer {
                context_id: 4,
                surface_id: 0,
                drop_id,
                transfer_id: 7,
                expected_generation: FileTransferGeneration::ONE,
                new_generation: FileTransferGeneration::new(2),
                committed_offset: 0,
                maximum_body_bytes: 1 << 20,
                maximum_records: 64,
            },
        )
        .unwrap();
    assert!(!manager.transfers[&(owner, 7)].active);
    assert!(
        manager.offers[&(owner, drop_id)]
            .source
            .as_ref()
            .unwrap()
            .ensure_generation(FileTransferGeneration::ONE)
            .is_err()
    );
    manager.transfers.get_mut(&(owner, 7)).unwrap().active = true;
    manager.connection_lost(owner, 7, FileTransferGeneration::ONE);
    assert!(manager.transfers[&(owner, 7)].active);
    manager.finish_transfer(
        owner,
        result(7, Some("/tmp/report.txt"), FileResultCode::Committed),
        true,
    );
    assert_eq!(manager.transfers[&(owner, 7)].generation, FileTransferGeneration::new(2));
    assert!(manager.offers[&(owner, drop_id)].terminal.is_none());
    assert!(manager.take_pending_pastes().is_empty());
    manager.finish_transfer(
        other,
        result(7, Some("/tmp/report.txt"), FileResultCode::Committed),
        true,
    );
    assert_eq!(manager.take_pending_pastes(), vec!["/tmp/report.txt ".to_owned()]);
    assert!(manager.offers[&(owner, drop_id)].source.is_some());
}

#[test]
fn file_readers_have_independent_offsets() {
    let path = persisted_temp_file(b"report");
    let source = open_source(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let mut first = source.open_stream().unwrap();
    let mut prefix = [0; 3];
    first.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"rep");
    let _second = source.open_stream().unwrap();
    first.read_exact(&mut prefix).unwrap();
    assert_eq!(&prefix, b"ort", "opening the second reader must not rewind the first");
}
