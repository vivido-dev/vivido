#!/usr/bin/env python3
"""Summarize alternating performance-e2e.py samples and flag acceptance failures."""
import argparse
import json
from pathlib import Path
import re
from statistics import median


def summarize(path):
    records = [json.loads(line) for line in path.read_text().splitlines()]
    rows = []
    for mode in dict.fromkeys(record["mode"] for record in records):
        groups = {label: [r for r in records if r["mode"] == mode and r["label"] == label]
                  for label in ("before", "after")}
        if not all(groups.values()):
            raise ValueError(f"{mode}: missing a comparison build")
        if len(groups["before"]) != len(groups["after"]):
            raise ValueError(f"{mode}: unpaired samples")
        workloads = {}
        for name in groups["before"][0]["rates"]:
            values = {label: median(r["rates"][name] for r in group) for label, group in groups.items()}
            change = 100 * (values["after"] / values["before"] - 1)
            workloads[name] = dict(values, change_percent=change, passes=change >= -5)
        queries = {}
        for loaded in (False, True):
            values = {label: median(next(q["p99_ms"] for q in r["latency"] if q["loaded"] == loaded)
                                    for r in group) for label, group in groups.items()}
            limit = values["before"] + max(1, values["before"] * .05)
            queries["loaded" if loaded else "idle"] = dict(values, limit_ms=limit, passes=values["after"] <= limit)
        costs = {}
        for label, group in groups.items():
            cpu = []
            rss = []
            for record in group:
                usage = record["resource_usage"]
                times = re.search(r"([0-9.]+) real\s+([0-9.]+) user\s+([0-9.]+) sys", usage)
                cpu.append(float(times[2]) + float(times[3]))
                rss.append(int(re.search(r"(\d+)\s+maximum resident set size", usage)[1]))
            costs[label] = dict(cpu_seconds=median(cpu), peak_rss_bytes=median(rss))
        rows.append(dict(mode=mode, samples=len(groups["before"]), workloads=workloads,
                         query_p99_ms=queries, costs=costs))
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("results", type=Path)
    args = parser.parse_args()
    rows = summarize(args.results)
    destination = args.results.with_name("summary.json")
    destination.write_text(json.dumps(rows, indent=2) + "\n")
    print("| Mode | Workload | Before MiB/s | After MiB/s | Change | Gate |")
    print("|---|---|---:|---:|---:|---|")
    for row in rows:
        for name, result in row["workloads"].items():
            print(f"| {row['mode']} | {name} | {result['before']:.1f} | {result['after']:.1f} | "
                  f"{result['change_percent']:+.1f}% | {'pass' if result['passes'] else 'FAIL'} |")
        for name, query in row["query_p99_ms"].items():
            print(f"{row['mode']} {name} query p99: {query['before']:.3f} → {query['after']:.3f} ms; "
                  f"limit {query['limit_ms']:.3f} ms; {'pass' if query['passes'] else 'FAIL'}")
        print(f"{row['mode']} CPU/RSS: {row['costs']}")


if __name__ == "__main__":
    main()
