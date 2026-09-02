//! Compile-time smoke test for the embedding API used by native hosts.

use std::mem::size_of;

use vivido::{LoopHandle, Processor, WindowOptions};

fn main() {
    let _ = size_of::<Processor>();
    let _ = size_of::<WindowOptions>();
    let _: Option<LoopHandle<'static>> = None;
}
