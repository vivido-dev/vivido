//! Regression coverage for initialization and unwinding of terminal rows.

use std::cell::Cell;

use vivido::terminal::grid::Row;

#[test]
#[should_panic(expected = "a terminal row requires at least one column")]
fn zero_columns_are_rejected_in_every_build_profile() {
    let _ = Row::<u8>::new(0);
}

#[test]
fn ordinary_and_zero_sized_cells_are_initialized() {
    assert_eq!(Row::<u64>::new(3).into_iter().copied().collect::<Vec<_>>(), [0; 3]);
    assert_eq!(Row::<()>::new(5).into_iter().count(), 5);
}

thread_local! {
    static CREATED: Cell<usize> = const { Cell::new(0) };
    static LIVE: Cell<usize> = const { Cell::new(0) };
}

struct PanickingDefault;

impl Default for PanickingDefault {
    fn default() -> Self {
        CREATED.with(|created| {
            let count = created.get() + 1;
            created.set(count);
            assert!(count < 3, "third cell fails");
        });
        LIVE.with(|live| live.set(live.get() + 1));
        Self
    }
}

impl Drop for PanickingDefault {
    fn drop(&mut self) {
        LIVE.with(|live| live.set(live.get() - 1));
    }
}

#[test]
fn a_panicking_default_drops_every_initialized_cell() {
    CREATED.with(|created| created.set(0));
    LIVE.with(|live| live.set(0));
    assert!(std::panic::catch_unwind(|| Row::<PanickingDefault>::new(4)).is_err());
    LIVE.with(|live| assert_eq!(live.get(), 0));
    CREATED.with(|created| assert_eq!(created.get(), 3));
}
