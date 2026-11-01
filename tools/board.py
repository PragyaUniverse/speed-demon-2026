#!/usr/bin/env python3
"""Show DareDevils on the live board: python3 -I tools/board.py [--wait COMMIT]"""
import json
import sys
import time
import urllib.request

URL = "https://s3.amazonaws.com/raptors.dev/speeddemon.dev/2026/board/board.json"
FIELDS = ("run_id", "commit", "stage", "ok", "T_s", "M_bytes", "S", "D", "K", "points", "failure")


def fetch():
    req = urllib.request.Request(URL, headers={"User-Agent": "daredevils-board-check/1.0"})
    for attempt in range(10):  # The feed can be caught mid-upload.
        try:
            with urllib.request.urlopen(req, timeout=20) as resp:
                board = json.load(resp)
            return next(t for t in board["teams"] if t["team"] == "DareDevils")
        except ValueError:
            time.sleep(3)
    raise SystemExit("board feed unreadable")


def show(team):
    print("queue", team.get("queue"))
    for track, entry in sorted(team["tracks"].items()):
        for which in ("latest", "best"):
            run = (entry or {}).get(which)
            print(track, which, {f: run.get(f) for f in FIELDS} if run else None)
            if run and run.get("stderr_tail"):
                print(run["stderr_tail"])


if len(sys.argv) > 2:
    want = sys.argv[2]
    for _ in range(90):
        team = fetch()
        runs = [(e or {}).get("latest") or {} for e in team["tracks"].values()]
        idle = not (team.get("queue") or {}).get("in_flight") and not (team.get("queue") or {}).get("queued")
        if idle and sum(r.get("commit", "").startswith(want) for r in runs) == len(runs):
            break
        time.sleep(20)
show(fetch())
