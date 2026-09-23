// A failed record permanently retires the transport.
#[test]
fn writer_rejects_stream_after_write_timeout() {
    let (mut client, server) = stream_pair();
    client.write_all(&encode_preface(ConnectionKind::Control, CONTROL_MAX_RECORD_BODY)).unwrap();
    let (reader, _, _) = Reader::new(server).unwrap();
    let writer = reader.writer(ConnectionKind::Control).unwrap();
    reader.set_write_timeout(Duration::from_millis(50)).unwrap();
    // A single vectored write can fit in Windows' loopback buffers even when it exceeds
    // SO_SNDBUF. Send bounded records until the nonreading peer actually causes backpressure.
    let body = vec![0x55; 64 * 1024];
    let (completed, error) = (0..256)
        .find_map(|completed| {
            writer.write_record(vivid_protocol::messages::PONG, 0, &body)
                .err().map(|error| (completed, error))
        })
        .expect("nonreading peer never backpressured the writer");
    assert!(matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock));
    client.set_nonblocking(true).unwrap();
    let mut buffer = [0; 65536];
    let mut received = 0;
    loop {
        match client.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => received += n,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("{error}"),
        }
    }
    assert!(received > HEADER_SIZE && received < (completed + 1) * (HEADER_SIZE + body.len()));
    assert!(writer.write_record(vivid_protocol::messages::PONG, 0, &[1]).is_err());
}
