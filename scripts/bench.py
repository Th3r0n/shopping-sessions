#!/usr/bin/env python3
"""
Times the image like the scorer does, several runs, output on a tmpfs.
    bench.py DATA [--runs 5] [--image shopping-sessions:latest] [--env K=V ...] [--cpuset 0-7]
DATA is a host folder or vol:VOLUME/SUBPATH. Prints each run's wall time, the median,
the fixed container cost (SS_NOOP=1) and the phase log of the median run.
"""
import argparse
import statistics
import subprocess
import time
from pathlib import Path

from docker_run import data_mount


def run(args, extra_env):
    cmd = ["docker", "run", "--rm", "--platform", "linux/amd64", "--network", "none", "--read-only",
           "--tmpfs", "/tmp:rw,size=64m,mode=1777", "--tmpfs", "/output:rw,size=4g,mode=1777",
           "--tmpfs", "/scratch:rw,size=64m,mode=1777",
           "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--pids-limit", "4096",
           "--cpus", args.cpus, "--memory", args.mem, "--memory-swap", args.mem, "--user", "1000:1000",
           *data_mount(args.data),
           "-e", "DATA_DIR=/data", "-e", "OUTPUT_DIR=/output", "-e", "SCRATCH_DIR=/scratch", "-e", "HOME=/scratch",
           "-e", f"NCPU={args.cpus}", "-e", f"MEM_GB={args.mem.rstrip('g')}"]
    # later -e flags override earlier ones (e.g. NCPU)
    if args.cpuset:
        cmd += ["--cpuset-cpus", args.cpuset]
    for kv in extra_env:
        cmd += ["-e", kv]
    cmd.append(args.image)
    start = time.perf_counter()
    p = subprocess.run(cmd, capture_output=True, text=True)
    wall = time.perf_counter() - start
    if p.returncode != 0:
        raise SystemExit(f"run failed ({p.returncode}):\n{p.stderr[-3000:]}")
    return wall, p.stderr


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--image", default="shopping-sessions:latest")
    ap.add_argument("--cpus", default="8")
    ap.add_argument("--mem", default="14g")
    ap.add_argument("--cpuset", default="0-7")
    ap.add_argument("--env", nargs="*", default=[])
    args = ap.parse_args()

    noop = sorted(run(args, ["SS_NOOP=1"])[0] for _ in range(3))[1]
    results = []
    for i in range(args.runs):
        wall, log = run(args, args.env)
        results.append((wall, log))
        print(f"run {i + 1}: {wall:.3f}s", flush=True)
    results.sort(key=lambda r: r[0])
    median_wall, median_log = results[len(results) // 2]
    print(f"\nmedian {median_wall:.3f}s  (fixed container cost ~{noop:.3f}s, so ~{median_wall - noop:.3f}s ours)")
    print(f"best {results[0][0]:.3f}s, worst {results[-1][0]:.3f}s\n")
    print(median_log.rstrip())


if __name__ == "__main__":
    main()
