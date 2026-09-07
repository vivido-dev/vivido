// A failed record permanently retires the transport.
#[test]
fn writer_rejects_stream_after_partial_write_timeout() {
    let (mut client, server) = stream_pair();
    client.write_all(&encode_preface(ConnectionKind::Control, CONTROL_MAX_RECORD_BODY)).unwrap();
    let (reader, _, _) = Reader::new(server).unwrap();
    reader.set_write_timeout(Duration::from_millis(50)).unwrap();
    let writer = reader.writer(ConnectionKind::Control).unwrap();
    let body = vec![0x55; CONTROL_MAX_RECORD_BODY as usize];
    assert!(writer.write_record(vivid_protocol::messages::PONG, 0, &body).is_err());
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
    assert!(received > HEADER_SIZE && received < HEADER_SIZE + body.len());
    assert!(writer.write_record(vivid_protocol::messages::PONG, 0, &[1]).is_err());
}
