#!/usr/bin/env python3
"""Measure maki's resident memory under a real session, honestly.

Two mistakes make maki memory numbers wrong, and both have burned us:

  * Sampling for a fixed wall-clock time. Restoring a session keeps allocating
    long after the first frame paints, so a 22s sample reported a 38% win where
    the real plateau, 50s in, was 24%. This script samples until anonymous RSS
    stops growing and only then reports.
  * Trusting one run. Identical builds have landed anywhere from 122MB to
    169MB. This script interleaves runs across the binaries under test so
    machine drift hits both, and refuses to call a delta real unless it clears
    the spread it actually observed.

Usage:
    scripts/memprobe.py -s <session-id> -n 5 ./base/maki ./target/debug/maki
"""

import argparse
import fcntl
import os
import pty
import signal
import statistics
import struct
import sys
import termios
import time

SAMPLE_INTERVAL = 0.25
PLATEAU_WINDOW = 8.0
PLATEAU_GROWTH = 0.02
MIN_RUNTIME = 15.0
DEFAULT_TIMEOUT = 120.0
DEFAULT_RUNS = 5
MIN_RUNS = 2
TERM_ROWS, TERM_COLS = 50, 200
KIB_PER_MIB = 1024


def anon_rss(pid):
    """Resident anonymous KiB, or None once the process is gone.

    Anonymous rather than total RSS because file-backed pages are the binary
    itself, which is what a debug build inflates and not what we are chasing.
    """
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                if line.startswith("Anonymous:"):
                    return int(line.split()[1])
    except OSError:
        return None
    return 0


def thread_count(pid):
    try:
        return len(os.listdir(f"/proc/{pid}/task"))
    except OSError:
        return 0


def plateaued(samples):
    elapsed = samples[-1][0]
    if elapsed < MIN_RUNTIME:
        return False
    window = [anon for at, anon in samples if at >= elapsed - PLATEAU_WINDOW]
    if len(window) < 2 or not min(window):
        return False
    return (max(window) - min(window)) / min(window) < PLATEAU_GROWTH


def probe(binary, session, env, timeout):
    """Run one binary to plateau. Returns (anon KiB, peak anon KiB, peak threads)."""
    args = [binary] + (["-s", session] if session else [])
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ.update(env)
        os.execv(binary, args)

    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", TERM_ROWS, TERM_COLS, 0, 0))
    os.set_blocking(fd, False)

    samples, peak_threads, start = [], 0, time.time()
    try:
        while time.time() - start < timeout:
            try:
                os.read(fd, 1 << 16)
            except OSError:
                pass
            anon = anon_rss(pid)
            if anon is None:
                break
            peak_threads = max(peak_threads, thread_count(pid))
            samples.append((time.time() - start, anon))
            if plateaued(samples):
                break
            time.sleep(SAMPLE_INTERVAL)
    finally:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except OSError:
            pass
        os.close(fd)

    if not samples:
        raise RuntimeError(f"{binary} produced no samples; did it exit immediately?")
    if not plateaued(samples):
        print(f"  warning: {binary} never plateaued within {timeout}s", file=sys.stderr)
    return samples[-1][1], max(anon for _, anon in samples), peak_threads


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="+")
    parser.add_argument("-s", "--session", help="session id to restore")
    parser.add_argument("-n", "--runs", type=int, default=DEFAULT_RUNS)
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT)
    parser.add_argument(
        "-e",
        "--env",
        action="append",
        default=[],
        metavar="KEY=VALUE",
        help="environment override, repeatable",
    )
    args = parser.parse_args()
    if args.runs < MIN_RUNS:
        # One run has no spread, so every delta would clear a zero noise floor.
        parser.error(f"need at least {MIN_RUNS} runs to estimate the noise floor")

    env = dict(pair.split("=", 1) for pair in args.env)
    results = {b: [] for b in args.binaries}

    for run in range(args.runs):
        for binary in args.binaries:
            anon, peak, threads = probe(binary, args.session, env, args.timeout)
            results[binary].append((anon, peak, threads))
            print(
                f"run {run + 1}/{args.runs} {binary:40} "
                f"anon={anon / KIB_PER_MIB:7.1f}MB peak={peak / KIB_PER_MIB:7.1f}MB "
                f"threads={threads}"
            )

    print()
    medians = {}
    spreads = {}
    for binary, runs in results.items():
        anons = sorted(a for a, _, _ in runs)
        medians[binary] = statistics.median(anons)
        spreads[binary] = anons[-1] - anons[0]
        print(
            f"{binary:40} median={medians[binary] / KIB_PER_MIB:7.1f}MB "
            f"spread={spreads[binary] / KIB_PER_MIB:5.1f}MB "
            f"peak={max(p for _, p, _ in runs) / KIB_PER_MIB:7.1f}MB "
            f"threads={max(t for _, _, t in runs)}"
        )

    baseline = args.binaries[0]
    for binary in args.binaries[1:]:
        delta = medians[binary] - medians[baseline]
        noise = max(spreads[baseline], spreads[binary])
        verdict = "REAL" if abs(delta) > noise else "WITHIN NOISE"
        print(
            f"\n{binary} vs {baseline}: {delta / KIB_PER_MIB:+.1f}MB "
            f"({delta / medians[baseline] * 100:+.1f}%), "
            f"noise floor {noise / KIB_PER_MIB:.1f}MB -> {verdict}"
        )


if __name__ == "__main__":
    main()
