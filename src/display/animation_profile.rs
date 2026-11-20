//! Correlate the headed animation fixture's palette with CPU-observed source submissions.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::terminal::event::EventListener;
use crate::terminal::index::{Column, Line, Point};
use crate::terminal::term::Term;
use crate::terminal::vvte::ansi::Color;

static ROWS: AtomicUsize = AtomicUsize::new(0);
const LIMIT: usize = 1500;

fn uptime_ns() -> Option<u64> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: the clock writes into an initialized, exclusively borrowed timespec.
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut ts) } != 0 {
        return None;
    }
    u64::try_from(ts.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(ts.tv_nsec).ok()?)
}

pub(super) fn sample<T: EventListener>(terminal: &Term<T>) -> Option<(u16, u64)> {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        std::env::var_os("VIVID_PROFILE_ANIMATION_SOURCE").as_deref()
            == Some(std::ffi::OsStr::new("1"))
    }) || ROWS.load(Ordering::Relaxed) >= LIMIT
    {
        return None;
    }
    let cell = &terminal.grid()[Point::new(Line(0), Column(0))];
    let Color::Spec(rgb) = terminal.style(cell).bg else { return None };
    let channels = [rgb.r, rgb.g, rgb.b];
    if !channels.iter().all(|&c| c % 32 == 16)
        || channels.iter().max()? - channels.iter().min()? < 96
    {
        return None;
    }
    let marker = u16::from(rgb.r / 32) * 64 + u16::from(rgb.g / 32) * 8 + u16::from(rgb.b / 32);
    Some((marker, uptime_ns()?))
}

pub(super) fn submitted(marker: Option<(u16, u64)>) {
    if let Some((id, snapshot)) = marker
        && let Some(submit) = uptime_ns()
        && ROWS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| (n < LIMIT).then_some(n + 1))
            .is_ok()
    {
        eprintln!("vv-animation-source {id} {snapshot} {submit}");
    }
}
