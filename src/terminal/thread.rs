use std::thread::{Builder, JoinHandle};

/// Like `thread::spawn`, but with a `name` argument.
///
/// # Panics
///
/// Panics if the operating system cannot spawn the named thread.
pub fn spawn_named<F, T, S>(name: S, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
    S: Into<String>,
{
    Builder::new().name(name.into()).spawn(f).expect("thread spawn works")
}
