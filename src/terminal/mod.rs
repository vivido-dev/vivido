//! Vivido - The GPU Enhanced Terminal.

#![warn(rust_2018_idioms)]
#![deny(clippy::all, clippy::if_not_else, clippy::enum_glob_use)]
#![cfg_attr(clippy, deny(warnings))]

pub mod event;
pub mod event_loop;
pub mod graphics;
pub mod grid;
pub mod index;
pub mod selection;
pub mod sync;
pub mod term;
pub mod thread;
pub mod tty;

#[cfg(all(test, vivido_ref_tests))]
mod ref_tests;

pub use crate::terminal::grid::Grid;
pub use crate::terminal::term::Term;
pub use vte;
