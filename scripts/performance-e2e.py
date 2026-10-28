#!/usr/bin/env python3
"""Alternate real Vivido processes using replay corpora or Kitty's driver on macOS.

The benchmark runs inside Vivido's PTY. Use --frontend native to include visible
window presentation; headless uses the production offscreen GPU renderer.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import shlex
import subprocess
import tempfile
import time


def run(args, binary, label, mode, sample, output):
    prefix = output / f"{mode}-{sample}-{label}"
    result = prefix.with_suffix(".txt")
    log = prefix.with_suffix(".log")
    metrics = prefix.with_suffix(".json")
    usage = prefix.with_suffix(".time")
    with tempfile.TemporaryDirectory(prefix="vvperf-", dir="/tmp") as temp:
        runtime = Path(temp)
        status = runtime / "status"
        startup = runtime / "startup"
        env = dict(os.environ, XDG_RUNTIME_DIR=temp, AGENT_MESH_WATCH="off",
                   VIVIDO_PERF_FILE=str(metrics))
        gather = args.before_gather if label == "before" else args.after_gather
        if gather == "default":
            env.pop("VIVIDO_PTY_GATHER", None)
        else:
            env["VIVIDO_PTY_GATHER"] = gather
        env.pop("VIVIDO_SOCKET", None)
        if args.driver == "kitten":
            command = [args.benchmark, "__benchmark__", "--repetitions", str(args.repetitions)]
        else:
            command = [args.benchmark, "--replay", args.corpus, "--repetitions", str(args.repetitions),
                       "--latency-samples", str(args.latency_samples)]
        if "render" in mode:
            command.append("--render")
        if "scroll" in mode:
            command.append("--with-scrollback")
        command.extend(args.workloads)
        child = f"{shlex.join(command)} > {shlex.quote(str(result))} 2>&1; printf '%s' \"$?\" > {shlex.quote(str(status))}"
        socket = str(runtime / "vivido.sock")
        launch = ["/usr/bin/time", "-l", "-o", str(usage), binary, "--socket", socket]
        if args.frontend == "headless":
            launch.extend(["--headless", "--foreground", "--session", "performance",
                           "--headless-size", "120x40"])
        else:
            launch.extend(["--hold", "--config-file", "/dev/null", "-o",
                           "window.dimensions.columns=120", "window.dimensions.lines=40"])
            # Let the native window settle before the benchmark's own warmup.
            child = "sleep 1; " + child
        launch.extend(["-e", "/bin/sh", "-c", child])
        with startup.open("w") as stdout, log.open("w") as stderr:
            process = subprocess.Popen(launch, env=env, stdout=stdout, stderr=stderr, start_new_session=True)
            try:
                deadline = time.monotonic() + args.timeout
                while time.monotonic() < deadline:
                    if status.exists() and Path(socket).exists():
                        break
                    if process.poll() is not None:
                        raise RuntimeError(f"{prefix}: process exited: {log.read_text()[-2000:]}")
                    time.sleep(0.05)
                else:
                    raise TimeoutError(f"{prefix}: benchmark timeout")
                if status.read_text() != "0":
                    raise RuntimeError(f"{prefix}: benchmark failed: {result.read_text()[-2000:]}")
            finally:
                if socket:
                    clean_env = env.copy()
                    clean_env.pop("VIVIDO_PERF_FILE", None)
                    try:
                        subprocess.run([binary, "msg", "-s", socket, "quit"], env=clean_env,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15, check=False)
                    except subprocess.TimeoutExpired:
                        pass
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait()
        clean = re.sub(r"\x1b\[[0-9;]*m", "", result.read_text())
        rates = {name.strip(): float(rate) for name, rate in re.findall(r"^\s*(.*?)\s+:.*?@\s*([0-9.]+)\s*Mi?B/s", clean, re.M)}
        if not rates:
            raise RuntimeError(f"{prefix}: no benchmark rates found")
        latency = [dict(loaded=m[0] == "true", p50_ms=float(m[1]), p99_ms=float(m[2]), max_ms=float(m[3]))
                   for m in re.findall(r"LATENCY loaded=(true|false) samples=\d+ p50_ms=([0-9.]+) p99_ms=([0-9.]+) max_ms=([0-9.]+)", clean)]
        record = dict(label=label, mode=mode, sample=sample, rates=rates, latency=latency,
                      resource_usage=usage.read_text(), binary=binary)
        if metrics.exists():
            record["diagnostics"] = json.loads(metrics.read_text())
        print(json.dumps(record), flush=True)
        return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("before", "after", "benchmark", "output"):
        parser.add_argument("--" + name, required=True)
    parser.add_argument("--corpus")
    parser.add_argument("--driver", choices=["replay", "kitten"], default="replay")
    parser.add_argument("--after-gather", choices=["default", "0", "1"], default="default")
    parser.add_argument("--before-gather", choices=["default", "0", "1"], default="default")
    parser.add_argument("--frontend", choices=["headless", "native"], default="headless")
    parser.add_argument("--samples", type=int, default=5)
    parser.add_argument("--repetitions", type=int, default=200)
    parser.add_argument("--latency-samples", type=int, default=100)
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--modes", nargs="+", choices=["sync", "render", "scroll", "render-scroll"], default=["sync", "render", "scroll", "render-scroll"])
    parser.add_argument("--workloads", nargs="+")
    args = parser.parse_args()
    if args.driver == "replay" and not args.corpus:
        parser.error("--corpus is required for the replay driver")
    if args.workloads is None:
        args.workloads = ["ascii", "unicode", "csi", "long_escape_codes", "images"]
        if args.driver == "replay":
            args.workloads.insert(2, "unique_unicode")
    if min(args.samples, args.repetitions, args.latency_samples, args.timeout) <= 0:
        parser.error("sample counts, repetitions, and timeout must be positive")
    for name in ("before", "after", "benchmark", "corpus", "output"):
        if getattr(args, name) is not None:
            setattr(args, name, str(Path(getattr(args, name)).resolve()))
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    if (output / "results.jsonl").exists():
        parser.error("use a new output directory so samples cannot be mixed")
    def fingerprint(path):
        data = Path(path).read_bytes()
        return {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    metadata = {"arguments": vars(args), "platform": platform.platform(),
                "binaries": {label: fingerprint(getattr(args, label)) for label in ("before", "after", "benchmark")},
                "corpus": {path.name: fingerprint(path) for path in sorted(Path(args.corpus).glob("*.bin"))} if args.corpus else {}}
    if args.driver == "kitten":
        metadata["benchmark_version"] = subprocess.check_output([args.benchmark, "--version"], text=True).strip()
    (output / "metadata.json").write_text(json.dumps(metadata, indent=2) + "\n")
    with (output / "results.jsonl").open("a") as records:
        for mode in args.modes:
            for sample in range(args.samples):
                order = [(args.before, "before"), (args.after, "after")]
                if sample % 2:
                    order.reverse()
                for binary, label in order:
                    record = run(args, binary, label, mode, sample, output)
                    records.write(json.dumps(record) + "\n")
                    records.flush()


if __name__ == "__main__":
    main()
