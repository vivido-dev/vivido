#!/usr/bin/env python3
"""Write payloads shaped like `kitten __benchmark__` "Only ASCII chars".

ascii.bin       2 MiB + 13 bytes drawn from the benchmark's 88-symbol alphabet
                (86 printable characters plus LF and TAB), seed 1.
ascii_sync.bin  one repetition as the timed loop sends it in default mode: the payload
                inside a synchronized update, with the per-repetition screen reset.
nolf.bin        the same printable bytes with LF and TAB removed (wrap-only case).

Usage: kitty-ascii-payload.py OUT_DIR
"""
import argparse
from pathlib import Path
import random

PRINTABLE = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ  `~!@#$%^&*()_+-=[]{}\\|;:'\",<.>/?"
SIZE = 1024 * 2048 + 13
RESET = b"\x1b[m\x1b[H\x1b[2JRunning: Only ASCII chars\r\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("out_dir", type=Path)
    out = parser.parse_args().out_dir
    out.mkdir(parents=True, exist_ok=True)
    assert len(PRINTABLE) == 86
    alphabet = (PRINTABLE + "\n\t").encode()
    rng = random.Random(1)
    data = bytes(rng.choice(alphabet) for _ in range(SIZE))
    (out / "ascii.bin").write_bytes(data)
    (out / "ascii_sync.bin").write_bytes(b"\x1b[?2026h" + data + RESET + b"\x1b[?2026l\x1b[?2026h")
    (out / "nolf.bin").write_bytes(bytes(b for b in data if b not in b"\n\t"))
    lf, tab = data.count(b"\n"), data.count(b"\t")
    print(f"{out}: {len(data)} bytes, {lf} LF, {tab} TAB")


if __name__ == "__main__":
    main()
