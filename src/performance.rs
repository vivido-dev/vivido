//! Optional process-wide performance diagnostics, compiled only with `perf`.

use std::alloc::{GlobalAlloc, Layout};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

static ALLOCS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static LIVE: AtomicU64 = AtomicU64::new(0);
static PEAK: AtomicU64 = AtomicU64::new(0);
static PRESENTED: AtomicU64 = AtomicU64::new(0);

/// Instrumented application allocator with the same mimalloc backend.
#[derive(Debug)]
pub struct Allocator;
fn allocated(size: usize) {
    ALLOCS.fetch_add(1, Ordering::Relaxed);
    BYTES.fetch_add(size as u64, Ordering::Relaxed);
    let live = LIVE.fetch_add(size as u64, Ordering::Relaxed) + size as u64;
    PEAK.fetch_max(live, Ordering::Relaxed);
}
// SAFETY: every operation forwards the original layout and pointer to mimalloc.
// Counters allocate no memory and never dereference allocation pointers.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies the GlobalAlloc layout contract.
        let ptr = unsafe { mimalloc::MiMalloc.alloc(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies the GlobalAlloc layout contract.
        let ptr = unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) };
        if !ptr.is_null() {
            allocated(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        // SAFETY: pointer and layout are forwarded unchanged to the allocating backend.
        unsafe {
            mimalloc::MiMalloc.dealloc(ptr, layout);
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: pointer, layout, and requested size satisfy the caller's allocator contract.
        let next = unsafe { mimalloc::MiMalloc.realloc(ptr, layout, size) };
        if !next.is_null() {
            LIVE.fetch_sub(layout.size() as u64, Ordering::Relaxed);
            allocated(size);
        }
        next
    }
}

/// Bounded logarithmic histogram; each bucket records an upper power-of-two bound.
#[derive(Debug)]
pub struct Histogram {
    bins: [AtomicU64; 64],
    sum: AtomicU64,
}
impl Histogram {
    const fn new() -> Self {
        Self { bins: [const { AtomicU64::new(0) }; 64], sum: AtomicU64::new(0) }
    }
    /// Record a duration in nanoseconds or a byte count.
    pub fn record(&self, value: u64) {
        let index = (64 - value.saturating_sub(1).leading_zeros() as usize).min(63);
        self.bins[index].fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
    }
    fn report(&self) -> serde_json::Value {
        serde_json::json!({"sum": self.sum.load(Ordering::Relaxed),
            "power_of_two_buckets": self.bins.iter().map(|n| n.load(Ordering::Relaxed)).collect::<Vec<_>>()})
    }
}
/// Bytes returned from a native PTY read.
pub static READ_BYTES: Histogram = Histogram::new();
/// Bytes delivered to terminal parsing per call.
pub static PARSE_BYTES: Histogram = Histogram::new();
/// Bytes published by the gather worker.
pub static GATHER_BYTES: Histogram = Histogram::new();
/// Terminal lease and lock wait in nanoseconds.
pub static LOCK_WAIT: Histogram = Histogram::new();
/// Terminal parser critical-section time in nanoseconds.
pub static LOCK_HOLD: Histogram = Histogram::new();
/// Renderer terminal-snapshot time in nanoseconds.
pub static SNAPSHOT: Histogram = Histogram::new();
/// Complete frame work in nanoseconds.
pub static FRAME: Histogram = Histogram::new();
/// Time between successful presentations in the same pane, including idle gaps.
pub static FRAME_INTERVAL: Histogram = Histogram::new();

/// Count a successfully delivered frame, including cached-scene presentations.
pub fn presented(previous: &mut Option<Instant>) {
    let now = Instant::now();
    if let Some(previous) = previous.replace(now) {
        FRAME_INTERVAL
            .record(now.duration_since(previous).as_nanos().min(u128::from(u64::MAX)) as u64);
    }
    PRESENTED.fetch_add(1, Ordering::Relaxed);
}

/// Scoped timing sample for a diagnostic histogram.
#[derive(Debug)]
pub struct Timer {
    start: Instant,
    target: &'static Histogram,
}
impl Timer {
    /// Start a timing sample.
    pub fn new(target: &'static Histogram) -> Self {
        Self { start: Instant::now(), target }
    }
}
impl Drop for Timer {
    fn drop(&mut self) {
        self.target.record(self.start.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64);
    }
}

/// Write numeric diagnostics to `VIVIDO_PERF_FILE` on orderly process exit.
#[derive(Debug)]
pub struct Report;
impl Drop for Report {
    fn drop(&mut self) {
        let Some(path) = std::env::var_os("VIVIDO_PERF_FILE") else {
            return;
        };
        let data = serde_json::json!({"allocations": ALLOCS.load(Ordering::Relaxed),
            "allocated_bytes": BYTES.load(Ordering::Relaxed), "live_bytes": LIVE.load(Ordering::Relaxed),
            "peak_live_bytes": PEAK.load(Ordering::Relaxed),
            "presented_frames": PRESENTED.load(Ordering::Relaxed), "read_bytes": READ_BYTES.report(),
            "parse_bytes": PARSE_BYTES.report(), "gather_bytes": GATHER_BYTES.report(),
            "lock_wait_ns": LOCK_WAIT.report(), "lock_hold_ns": LOCK_HOLD.report(),
            "snapshot_ns": SNAPSHOT.report(), "frame_ns": FRAME.report(),
            "frame_interval_ns": FRAME_INTERVAL.report()});
        if let Ok(data) = serde_json::to_vec_pretty(&data) {
            let _ = std::fs::write(path, data);
        }
    }
}
