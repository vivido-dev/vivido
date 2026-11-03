//! Measure parser or terminal processing of an exported benchmark payload.
//!
//! Run an optimized build with `parser|terminal PAYLOAD [ITERATIONS]`.
//! Input generation, terminal construction, and GPU/PTY work are outside the timer.

use std::error::Error;
use std::hint::black_box;
use std::time::Instant;

use vivido::terminal::event::VoidListener;
use vivido::terminal::term::{Config, Term, test::TermSize};
use vivido::terminal::vvte::ansi::{Handler, Processor};

#[cfg(not(feature = "perf"))]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "perf")]
#[global_allocator]
static ALLOCATOR: vivido::performance::Allocator = vivido::performance::Allocator;

#[derive(Default)]
struct Counter(usize);
impl Handler for Counter {
    fn input(&mut self, c: char) {
        self.0 = self.0.wrapping_add(c as usize);
    }
    fn apc_put(&mut self, bytes: &[u8]) {
        self.0 = self.0.wrapping_add(bytes.len());
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    #[cfg(feature = "perf")]
    let _report = vivido::performance::Report;
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).ok_or("expected parser|terminal PAYLOAD [ITERATIONS]")?;
    if mode != "parser" && mode != "terminal" {
        return Err("expected parser or terminal".into());
    }
    let input = std::fs::read(args.get(2).ok_or("expected payload path")?)?;
    let iterations = args.get(3).map(|n| n.parse::<usize>()).transpose()?.unwrap_or(20);
    if iterations == 0 {
        return Err("iterations must be positive".into());
    }
    let mut parser = Processor::new();
    let mut counter = Counter::default();
    let mut term = Term::new(Config::default(), &TermSize::new(120, 40), VoidListener);
    term.advance(&mut parser, b"\x1b[?1049h");
    let start = Instant::now();
    for _ in 0..iterations {
        for chunk in input.chunks(65536) {
            if mode == "parser" {
                parser.advance(&mut counter, black_box(chunk));
            } else {
                term.advance(&mut parser, black_box(chunk));
            }
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    black_box((counter, term));
    println!(
        "{mode},{iterations},{},{seconds:.6},{:.3}",
        input.len(),
        input.len() as f64 * iterations as f64 / seconds / 1048576.
    );
    Ok(())
}
