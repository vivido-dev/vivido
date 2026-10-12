//! Exercise an embedding host's IPC handshake, claimed request, and processor shutdown.
//!
//! Run with `cargo run --example library_api`. This creates no native windows, GPU, or PTY.
//! The listener is process-lived: after finishing processor cleanup this example exits, and the
//! operating system releases its listener thread. Production hosts normally retain one listener
//! until process exit. Dropping `IoListenerHandle` alone does not stop that thread.

use std::time::Duration;

use vivido::cli::Options;
use vivido::host::{IoListener, request_method};
use vivido::{EventSink, HeadlessLoop, Processor, UiConfig};
use winit::dpi::PhysicalSize;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(unix)]
    let directory = tempfile::tempdir_in("/tmp")?;
    #[cfg(unix)]
    let endpoint = directory.path().join("host.sock");
    #[cfg(windows)]
    let endpoint = std::path::PathBuf::from(format!(
        r"\\.\pipe\vivido-embedding-example-{}",
        std::process::id()
    ));
    let mut options = Options::default();
    options.socket = Some(endpoint.clone());
    let config = UiConfig::default();
    let (sink, events) = EventSink::headless();
    let _listener = IoListener::spawn(&config, &options, sink.clone())?;
    let mut processor = Processor::new_headless(config, options, sink);
    processor.claim_ipc_methods(&["host_list_panes"]);
    let headless = HeadlessLoop::new(PhysicalSize::new(800, 600), 1.0);
    let client = std::thread::spawn(move || {
        request_method(Some(endpoint), None, "host_list_panes", serde_json::json!({}))
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut answered = false;
    while !client.is_finished() {
        assert!(std::time::Instant::now() < deadline, "embedding request timed out");
        if !processor.pump_headless(&events, &headless) {
            break;
        }
        for request in processor.take_host_requests() {
            request.connection.reply(request.id, serde_json::json!({"panes": []}));
            answered = true;
        }
    }
    processor.finish_headless();
    let (hello, result) = client.join().map_err(|_| std::io::Error::other("client panicked"))??;
    assert!(answered);
    assert!(
        hello["methods"]
            .as_array()
            .is_some_and(|methods| { methods.iter().any(|method| method == "host_list_panes") })
    );
    assert_eq!(result, serde_json::json!({"panes": []}));
    println!("Host handshake, claimed request, reply, and processor cleanup completed.");
    Ok(())
}
