#!/usr/bin/env python3
"""Build a track natively and compare its output on every scale against the known hash.

    python3 tools/verify.py a [--repeat 5] [--no-full]

Prints wall time and peak RSS per scale. Numbers are from this machine, never the board's.
"""
import argparse
import hashlib
import os
import re
import shutil
import statistics
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)
WORK = os.path.dirname(REPO)
OUTPUT = {"a": "summary.txt", "b": "results.txt", "c": "aggregates.txt", "d": "distances.txt"}
FULL = {
    "a": "495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3",
    "b": "f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927",
    "c": "d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536",
    "d": "a23dbdf0a3e49cc8901310a9ee1dc6a9a569a85297b6da75c2a7002cb2ed277e",
}


def sha(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def small_hashes(track):
    hashes = {}
    for line in open(os.path.join(WORK, "refs", "expected-small.txt")):
        t, scale, digest = line.split()
        if t == track:
            hashes[scale] = digest
    return hashes


def run(binary, data, track):
    out = tempfile.mkdtemp(prefix="sd-out-")
    env = dict(os.environ, SD_DATA=data, SD_OUT=out, TRACK=track)
    timed = subprocess.run(["/usr/bin/time", "-l", binary], env=env, capture_output=True, text=True)
    if timed.returncode:
        sys.exit(f"FAIL exit {timed.returncode}: {timed.stderr[-2000:]}")
    secs = float(re.search(r"([\d.]+) real", timed.stderr).group(1))
    rss = int(re.search(r"(\d+)\s+maximum resident", timed.stderr).group(1))
    path = os.path.join(out, OUTPUT[track])
    digest = sha(path)
    data_bytes = open(path, "rb").read()
    shutil.rmtree(out)
    return secs, rss, digest, data_bytes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("track", choices=sorted(OUTPUT))
    ap.add_argument("--repeat", type=int, default=3)
    ap.add_argument("--no-full", action="store_true")
    args = ap.parse_args()
    t = args.track
    target = os.path.join(WORK, ".target", t)
    env = dict(os.environ, CARGO_TARGET_DIR=target, RUSTFLAGS="-C target-cpu=native")
    subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=os.path.join(REPO, f"track-{t}"), env=env, check=True)
    binary = os.path.join(target, "release", "solution")

    failed = False
    case = os.path.join(HERE, "cases", t)
    if os.path.isdir(case):
        _, _, _, got = run(binary, case, t)
        ok = got == open(os.path.join(case, "expected"), "rb").read()
        failed |= not ok
        print(f"{'ok  ' if ok else 'FAIL'} worked example")
    small = small_hashes(t)
    for scale in ("tiny", "sample"):
        secs, rss, digest, _ = run(binary, os.path.join(WORK, "gen", t, scale), t)
        ok = digest == small[scale]
        failed |= not ok
        print(f"{'ok  ' if ok else 'FAIL'} {scale:6} {secs:7.3f} s {rss / 1e6:8.1f} MB")
    if not args.no_full:
        times, rsses, digests = [], [], set()
        for _ in range(args.repeat):
            secs, rss, digest, _ = run(binary, os.path.join(WORK, "data", t), t)
            times.append(secs)
            rsses.append(rss)
            digests.add(digest)
        ok = digests == {FULL[t]}
        failed |= not ok
        print(f"{'ok  ' if ok else 'FAIL'} full   {statistics.median(times):7.3f} s (best {min(times):.3f}) {max(rsses) / 1e6:8.1f} MB  {len(digests)} distinct hash")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
