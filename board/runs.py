"""Per-team run history for the /board scorecard, replayed from history.jsonl.

board.json carries only each track's best and latest run, so a team cannot see what its other pushes
scored. This writes one small public file per team on the board, newest run first:

  runs/<slug>.json = {"generated_at": ..., "runs": [run, ...]}
  run = {"at", "pushed", "c", "tk", "ok", "pts", "K", "T", "M", "S", "st", "g", "f"}

Only fields board.json already publishes for a best or latest run go out: times, commit, track, the
axes, the stage and failed gate, and the one-line failure message. Log tails never do. Untouched-starter
runs are skipped. A file is rewritten only when its runs changed, and with --upload only those go to S3
(the publisher key may put objects but not list the bucket, so `aws s3 sync` is out). A failed upload
stays in .pending and is retried next time. Run by speeddemon-runs.timer every 10 s.

    python3 board/runs.py --history /srv/speeddemon/board/state/history.jsonl \
        --board /srv/speeddemon/board/public/board.json --out /srv/speeddemon/board/public/runs \
        --upload s3://raptors.dev/speeddemon.dev/2026/board/runs
"""
import argparse
import concurrent.futures as cf
import json
import os
import re
import subprocess
import tempfile
import time

SKIP_STAGES = {"unchanged"}
KEEP = 1000
FAILURE_MAX = 200
SLUG = re.compile(r"^[a-z0-9][a-z0-9-]{0,79}$")


def compact(r):
    gate = next((g for g, v in (r.get("gates") or {}).items() if v is False), None)
    run = {"at": r.get("ended_at") or r.get("recorded_at"), "pushed": r.get("pushed_at"),
           "c": (r.get("commit") or "")[:12], "tk": r.get("track"), "ok": bool(r.get("ok"))}
    if run["ok"]:
        run.update(pts=r.get("points"), K=r.get("K"), T=r.get("T_s"), M=r.get("M_bytes"), S=r.get("S"))
    else:
        run.update(st=r.get("stage"), g=gate, f=str(r.get("failure") or "")[:FAILURE_MAX] or None)
    return run


def collect(rows, slugs):
    out = {s: [] for s in slugs}
    for r in rows:
        s = r.get("team_slug")
        if s in out and r.get("stage") not in SKIP_STAGES:
            out[s].append(compact(r))
    return {s: runs[-KEEP:][::-1] for s, runs in out.items()}


def write_if_changed(path, runs):
    """Rewrite only when the runs differ, so an unchanged file keeps its mtime and sync skips it."""
    try:
        with open(path) as f:
            if json.load(f).get("runs") == runs:
                return False
    except (OSError, ValueError):
        pass
    body = {"generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), "runs": runs}
    fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".runs-")
    with os.fdopen(fd, "w") as f:
        json.dump(body, f, separators=(",", ":"))
    os.chmod(tmp, 0o644)
    os.replace(tmp, path)
    return True


def upload(out, dest, changed):
    """aws s3 cp every changed or still-pending file; whatever fails stays pending for the next run."""
    pending_path = os.path.join(out, ".pending")
    try:
        with open(pending_path) as f:
            pending = set(f.read().split())
    except OSError:
        pending = set()
    pending |= set(changed)

    def put(slug):
        src = os.path.join(out, f"{slug}.json")
        r = subprocess.run(["aws", "s3", "cp", src, f"{dest}/{slug}.json", "--cache-control", "max-age=5",
                            "--content-type", "application/json", "--only-show-errors"],
                           capture_output=True, text=True, timeout=60)
        return slug, r.returncode == 0 or not os.path.exists(src)

    with cf.ThreadPoolExecutor(8) as ex:
        left = {s for s, ok in ex.map(put, sorted(pending)) if not ok}
    with open(pending_path + ".tmp", "w") as f:
        f.write("\n".join(sorted(left)))
    os.replace(pending_path + ".tmp", pending_path)
    return len(pending) - len(left), len(left)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--history", required=True)
    ap.add_argument("--board", required=True, help="the public board.json: only its teams get a file")
    ap.add_argument("--out", required=True)
    ap.add_argument("--upload", help="s3:// prefix to copy changed files to")
    a = ap.parse_args()
    with open(a.board) as f:
        slugs = {t.get("slug") for t in json.load(f).get("teams") or []}
    slugs = {s for s in slugs if s and SLUG.match(s)}
    rows = []
    with open(a.history) as f:
        for line in f:
            try:
                rows.append(json.loads(line))
            except ValueError:
                continue               # a torn last line while the board appends
    os.makedirs(a.out, exist_ok=True)
    changed = [s for s, runs in collect(rows, slugs).items() if write_if_changed(os.path.join(a.out, f"{s}.json"), runs)]
    sent, left = upload(a.out, a.upload.rstrip("/"), changed) if a.upload else (0, 0)
    print(f"runs: {len(slugs)} teams, {len(changed)} files changed, {sent} uploaded, {left} pending")
    if left:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
