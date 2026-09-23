//! Opt-in release throughput measurements. Set VIVIDO_BENCH_WORKLOAD and VIVIDO_BENCH_LAYER to select a workload and layer.

use std::hint::black_box;

use super::*;
use crate::terminal::event::VoidListener;
use crate::terminal::term::{Config, test::TermSize};

fn number(name: &str, default: usize) -> usize {
    std::env::var(name).map_or(default, |value| value.parse().expect("positive integer"))
}

/// A fixed, portable corpus; randomization is deliberately independent of the system RNG.
fn workload(name: &str) -> Vec<u8> {
    const ASCII: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ  `~!@#$%^&*()_+-=[]{}\\|;:'\",<.>/?\n\t";
    let mut seed = 20260908u64;
    let mut random = |limit: usize| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        usize::try_from(seed >> 32).unwrap() % limit
    };
    let mut data = Vec::with_capacity(1024 * 1024 + 128);
    while data.len() < 1024 * 1024 + 17 {
        let chunk: &[u8] = match name {
            "csi" => match random(100) {
                0..=9 => {
                    let len = random(72) + 1;
                    for _ in 0..len {
                        data.push(ASCII[random(ASCII.len())]);
                    }
                    continue;
                },
                10..=29 => b"\x1b[m\x1b[?1h\x1b[H",
                30..=39 => b"\x1b[1;2;3;4:3;31m",
                40..=49 => b"\x1b[38:5:24;48:2:125:136:147m",
                50..=59 => b"\x1b[58;5;44;2m",
                60..=79 => b"\x1b[m\x1b[10A\x1b[3E\x1b[2K",
                _ => b"\x1b[39m\x1b[10`a\x1b[100b\x1b[?1l",
            },
            "rep" => b"\x1b[Ha\x1b[100b",
            "sgr" => b"\x1b[1;2;3;4:3;31m\x1b[38:5:24;48:2:125:136:147m\x1b[58;5;44;2m\x1b[m",
            "erase" => b"abc\x1b[10A\x1b[3E\x1b[2K\x1b[H",
            "ascii" => {
                data.push(ASCII[random(ASCII.len())]);
                continue;
            },
            "unicode" => "漢字😀e\u{301}\t\n".as_bytes(),
            "long_escape" => {
                data.extend_from_slice(b"\x1b]6;");
                // Match kitty's printable payload, including ordinary `V` candidates.
                for _ in 0..8024 {
                    data.push(ASCII[random(ASCII.len() - 2)]);
                }
                data.push(7);
                continue;
            },
            _ => panic!("unknown VIVIDO_BENCH_WORKLOAD"),
        };
        data.extend_from_slice(chunk);
    }
    data.extend_from_slice(b"\x1b[m");
    data
}

#[test]
#[ignore = "manual release throughput measurement"]
fn parser_throughput() {
    let name = std::env::var("VIVIDO_BENCH_WORKLOAD").unwrap_or_else(|_| "csi".into());
    let data = match std::env::var_os("VIVIDO_BENCH_INPUT") {
        Some(path) => std::fs::read(path).unwrap(),
        None => workload(&name),
    };
    assert!(!data.is_empty());
    let repetitions =
        (number("VIVIDO_BENCH_MIB", 64).checked_mul(1024 * 1024).unwrap() / data.len()).max(1);
    let columns = number("VIVIDO_BENCH_COLUMNS", 80);
    let lines = number("VIVIDO_BENCH_LINES", 24);
    assert!(columns >= 2 && lines > 0);
    let size = TermSize::new(columns, lines);
    let chunk_size = number("VIVIDO_BENCH_CHUNK", MAX_LOCKED_READ);
    assert!(chunk_size > 0);
    let samples_count = number("VIVIDO_BENCH_SAMPLES", 7);
    assert!(samples_count > 0 && samples_count % 2 == 1);
    let sync = number("VIVIDO_BENCH_SYNC", 0) != 0;
    let layer_filter = std::env::var("VIVIDO_BENCH_LAYER").ok();
    let mut framed = Vec::with_capacity(data.len() + 64);
    if sync {
        framed.extend_from_slice(b"\x1b[?2026h");
    }
    framed.extend_from_slice(&data);
    framed.extend_from_slice(b"\x1b[m\x1b[H\x1b[2J");
    if sync {
        framed.extend_from_slice(b"\x1b[?2026l");
    }

    for layer in ["observer", "ansi", "terminal", "pipeline"] {
        if layer_filter.as_ref().is_some_and(|filter| filter != layer) {
            continue;
        }
        let mut samples = Vec::with_capacity(samples_count);
        for sample in 0..=samples_count {
            let mut term = Term::new(Config::default(), &size, VoidListener);
            let mut state = State::default();
            let transcript = Arc::new(Mutex::new(Transcript::default()));
            term.advance(&mut state.parser, b"\x1b[?1049h");
            let start = Instant::now();
            for _ in 0..repetitions {
                for chunk in framed.chunks(chunk_size) {
                    match layer {
                        "observer" => {
                            black_box(state.osc_notifications.advance(black_box(chunk)));
                        },
                        "ansi" => state.parser.advance(&mut term, black_box(chunk)),
                        "terminal" => term.advance(&mut state.parser, black_box(chunk)),
                        _ => {
                            black_box(state.advance(&mut term, black_box(chunk), &transcript));
                        },
                    }
                }
            }
            let elapsed = start.elapsed().as_secs_f64();
            black_box((&term, &state, &transcript));
            if sample != 0 {
                samples.push(elapsed);
            }
        }
        samples.sort_by(f64::total_cmp);
        let mib = data.len().checked_mul(repetitions).unwrap() as f64 / (1024. * 1024.);
        println!(
            "{name} {columns}x{lines} chunk={chunk_size} sync={sync} {layer}: median {:.1} MiB/s (range {:.1}..{:.1})",
            mib / samples[samples_count / 2],
            mib / samples[samples_count - 1],
            mib / samples[0],
        );
    }
}
