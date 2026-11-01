#!/usr/bin/env python3
"""Differential test on seeds the board never shows: compare a native build with the Python reference.

    python3 -I tools/seeds.py a [--seeds 2 3 4] [--scales tiny sample]
"""
import argparse
import hashlib
import os
import shutil
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
WORK = os.path.dirname(os.path.dirname(HERE))
PUBLIC = os.path.join(WORK, "refs", "public")
MODULE = {"a": "a_ingest", "b": "b_index", "c": "c_join", "d": "d_traverse"}
OUTPUT = {"a": "summary.txt", "b": "results.txt", "c": "aggregates.txt", "d": "distances.txt"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("track", choices=sorted(MODULE))
    ap.add_argument("--seeds", type=int, nargs="+", default=[2, 3, 4])
    ap.add_argument("--scales", nargs="+", default=["tiny", "sample"])
    args = ap.parse_args()
    t, m, o = args.track, MODULE[args.track], OUTPUT[args.track]
    binary = os.path.join(WORK, ".target", t, "release", "solution")
    bad = 0
    for seed in args.seeds:
        for scale in args.scales:
            d = os.path.join(WORK, "gen-seeds", t, f"{scale}-{seed}")
            ref, mine = d + ".ref", d + ".mine"
            for x in (ref, mine):
                shutil.rmtree(x, ignore_errors=True)
                os.makedirs(x)
            if not os.path.isdir(d):
                subprocess.run([sys.executable, "-I", f"{PUBLIC}/generators/{m}.py", "--seed", str(seed), "--scale", scale, "--out", d], check=True, capture_output=True)
            subprocess.run([sys.executable, "-I", f"{PUBLIC}/reference/{m}.py", "--data", d, "--out", ref], check=True, capture_output=True)
            subprocess.run([binary], env=dict(os.environ, SD_DATA=d, SD_OUT=mine, TRACK=t), check=True)
            a = hashlib.sha256(open(f"{ref}/{o}", "rb").read()).hexdigest()
            b = hashlib.sha256(open(f"{mine}/{o}", "rb").read()).hexdigest()
            print(f"{'ok  ' if a == b else 'FAIL'} seed {seed} {scale}")
            bad += a != b
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
